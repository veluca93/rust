// ignore-tidy-filelength :(

pub mod ambiguity;
mod infer_ctxt_ext;
pub mod on_unimplemented;
pub mod suggestions;
mod type_err_ctxt_ext;

use rustc_data_structures::fx::FxIndexSet;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_hir::intravisit::Visitor;
use rustc_infer::traits::{Obligation, ObligationCause, ObligationCauseCode, PredicateObligation};
use rustc_middle::ty::print::PrintTraitRefExt as _;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_span::Span;
use std::ops::ControlFlow;

pub use self::infer_ctxt_ext::*;
pub use self::type_err_ctxt_ext::*;

// When outputting impl candidates, prefer showing those that are more similar.
//
// We also compare candidates after skipping lifetimes, which has a lower
// priority than exact matches.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateSimilarity {
    Exact { ignoring_lifetimes: bool },
    Fuzzy { ignoring_lifetimes: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImplCandidate<'tcx> {
    pub trait_ref: ty::TraitRef<'tcx>,
    pub similarity: CandidateSimilarity,
    impl_def_id: DefId,
}

enum GetSafeTransmuteErrorAndReason {
    Silent,
    Error { err_msg: String, safe_transmute_explanation: Option<String> },
}

struct UnsatisfiedConst(pub bool);

/// Crude way of getting back an `Expr` from a `Span`.
pub struct FindExprBySpan<'hir> {
    pub span: Span,
    pub result: Option<&'hir hir::Expr<'hir>>,
    pub ty_result: Option<&'hir hir::Ty<'hir>>,
    pub include_closures: bool,
    pub tcx: TyCtxt<'hir>,
}

impl<'hir> FindExprBySpan<'hir> {
    pub fn new(span: Span, tcx: TyCtxt<'hir>) -> Self {
        Self { span, result: None, ty_result: None, tcx, include_closures: false }
    }
}

impl<'v> Visitor<'v> for FindExprBySpan<'v> {
    type NestedFilter = rustc_middle::hir::nested_filter::OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }

    fn visit_expr(&mut self, ex: &'v hir::Expr<'v>) {
        if self.span == ex.span {
            self.result = Some(ex);
        } else {
            if let hir::ExprKind::Closure(..) = ex.kind
                && self.include_closures
                && let closure_header_sp = self.span.with_hi(ex.span.hi())
                && closure_header_sp == ex.span
            {
                self.result = Some(ex);
            }
            hir::intravisit::walk_expr(self, ex);
        }
    }

    fn visit_ty(&mut self, ty: &'v hir::Ty<'v>) {
        if self.span == ty.span {
            self.ty_result = Some(ty);
        } else {
            hir::intravisit::walk_ty(self, ty);
        }
    }
}

/// Look for type `param` in an ADT being used only through a reference to confirm that suggesting
/// `param: ?Sized` would be a valid constraint.
struct FindTypeParam {
    param: rustc_span::Symbol,
    invalid_spans: Vec<Span>,
    nested: bool,
}

impl<'v> Visitor<'v> for FindTypeParam {
    fn visit_where_predicate(&mut self, _: &'v hir::WherePredicate<'v>) {
        // Skip where-clauses, to avoid suggesting indirection for type parameters found there.
    }

    fn visit_ty(&mut self, ty: &hir::Ty<'_>) {
        // We collect the spans of all uses of the "bare" type param, like in `field: T` or
        // `field: (T, T)` where we could make `T: ?Sized` while skipping cases that are known to be
        // valid like `field: &'a T` or `field: *mut T` and cases that *might* have further `Sized`
        // obligations like `Box<T>` and `Vec<T>`, but we perform no extra analysis for those cases
        // and suggest `T: ?Sized` regardless of their obligations. This is fine because the errors
        // in that case should make what happened clear enough.
        match ty.kind {
            hir::TyKind::Ptr(_) | hir::TyKind::Ref(..) | hir::TyKind::TraitObject(..) => {}
            hir::TyKind::Path(hir::QPath::Resolved(None, path))
                if path.segments.len() == 1 && path.segments[0].ident.name == self.param =>
            {
                if !self.nested {
                    debug!(?ty, "FindTypeParam::visit_ty");
                    self.invalid_spans.push(ty.span);
                }
            }
            hir::TyKind::Path(_) => {
                let prev = self.nested;
                self.nested = true;
                hir::intravisit::walk_ty(self, ty);
                self.nested = prev;
            }
            _ => {
                hir::intravisit::walk_ty(self, ty);
            }
        }
    }
}

/// Summarizes information
#[derive(Clone)]
pub enum ArgKind {
    /// An argument of non-tuple type. Parameters are (name, ty)
    Arg(String, String),

    /// An argument of tuple type. For a "found" argument, the span is
    /// the location in the source of the pattern. For an "expected"
    /// argument, it will be None. The vector is a list of (name, ty)
    /// strings for the components of the tuple.
    Tuple(Option<Span>, Vec<(String, String)>),
}

impl ArgKind {
    fn empty() -> ArgKind {
        ArgKind::Arg("_".to_owned(), "_".to_owned())
    }

    /// Creates an `ArgKind` from the expected type of an
    /// argument. It has no name (`_`) and an optional source span.
    pub fn from_expected_ty(t: Ty<'_>, span: Option<Span>) -> ArgKind {
        match t.kind() {
            ty::Tuple(tys) => ArgKind::Tuple(
                span,
                tys.iter().map(|ty| ("_".to_owned(), ty.to_string())).collect::<Vec<_>>(),
            ),
            _ => ArgKind::Arg("_".to_owned(), t.to_string()),
        }
    }
}

struct HasNumericInferVisitor;

impl<'tcx> ty::TypeVisitor<TyCtxt<'tcx>> for HasNumericInferVisitor {
    type Result = ControlFlow<()>;

    fn visit_ty(&mut self, ty: Ty<'tcx>) -> Self::Result {
        if matches!(ty.kind(), ty::Infer(ty::FloatVar(_) | ty::IntVar(_))) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

#[derive(Copy, Clone)]
pub enum DefIdOrName {
    DefId(DefId),
    Name(&'static str),
}

/// Recovers the "impl X for Y" signature from `impl_def_id` and returns it as a
/// string.
pub(crate) fn to_pretty_impl_header(tcx: TyCtxt<'_>, impl_def_id: DefId) -> Option<String> {
    use std::fmt::Write;

    let trait_ref = tcx.impl_trait_ref(impl_def_id)?.instantiate_identity();
    let mut w = "impl".to_owned();

    let args = ty::GenericArgs::identity_for_item(tcx, impl_def_id);

    // FIXME: Currently only handles ?Sized.
    //        Needs to support ?Move and ?DynSized when they are implemented.
    let mut types_without_default_bounds = FxIndexSet::default();
    let sized_trait = tcx.lang_items().sized_trait();

    let arg_names = args.iter().map(|k| k.to_string()).filter(|k| k != "'_").collect::<Vec<_>>();
    if !arg_names.is_empty() {
        types_without_default_bounds.extend(args.types());
        w.push('<');
        w.push_str(&arg_names.join(", "));
        w.push('>');
    }

    write!(
        w,
        " {} for {}",
        trait_ref.print_only_trait_path(),
        tcx.type_of(impl_def_id).instantiate_identity()
    )
    .unwrap();

    // The predicates will contain default bounds like `T: Sized`. We need to
    // remove these bounds, and add `T: ?Sized` to any untouched type parameters.
    let predicates = tcx.predicates_of(impl_def_id).predicates;
    let mut pretty_predicates =
        Vec::with_capacity(predicates.len() + types_without_default_bounds.len());

    for (p, _) in predicates {
        if let Some(poly_trait_ref) = p.as_trait_clause() {
            if Some(poly_trait_ref.def_id()) == sized_trait {
                // FIXME(#120456) - is `swap_remove` correct?
                types_without_default_bounds.swap_remove(&poly_trait_ref.self_ty().skip_binder());
                continue;
            }
        }
        pretty_predicates.push(p.to_string());
    }

    pretty_predicates.extend(types_without_default_bounds.iter().map(|ty| format!("{ty}: ?Sized")));

    if !pretty_predicates.is_empty() {
        write!(w, "\n  where {}", pretty_predicates.join(", ")).unwrap();
    }

    w.push(';');
    Some(w)
}
