//! The `Visitor` responsible for actually checking a `mir::Body` for invalid operations.

use rustc_errors::{Applicability, Diagnostic, ErrorReported};
use rustc_hir::def_id::DefId;
use rustc_hir::{self as hir, HirId, LangItem};
use rustc_index::bit_set::BitSet;
use rustc_infer::infer::TyCtxtInferExt;
use rustc_infer::traits::{ImplSource, Obligation, ObligationCause};
use rustc_middle::mir::visit::{MutatingUseContext, NonMutatingUseContext, PlaceContext, Visitor};
use rustc_middle::mir::*;
use rustc_middle::ty::cast::CastTy;
use rustc_middle::ty::subst::GenericArgKind;
use rustc_middle::ty::{self, adjustment::PointerCast, Instance, InstanceDef, Ty, TyCtxt};
use rustc_middle::ty::{Binder, TraitPredicate, TraitRef};
use rustc_span::{sym, Span, Symbol};
use rustc_trait_selection::traits::error_reporting::InferCtxtExt;
use rustc_trait_selection::traits::{self, SelectionContext, TraitEngine};

use std::mem;
use std::ops::Deref;

use super::ops::{self, NonConstOp, Status};
use super::qualifs::{self, CustomEq, HasMutInterior, NeedsDrop};
use super::resolver::FlowSensitiveAnalysis;
use super::{is_lang_panic_fn, ConstCx, Qualif};
use crate::const_eval::is_unstable_const_fn;
use crate::dataflow::impls::MaybeMutBorrowedLocals;
use crate::dataflow::{self, Analysis};

// We are using `MaybeMutBorrowedLocals` as a proxy for whether an item may have been mutated
// through a pointer prior to the given point. This is okay even though `MaybeMutBorrowedLocals`
// kills locals upon `StorageDead` because a local will never be used after a `StorageDead`.
type IndirectlyMutableResults<'mir, 'tcx> =
    dataflow::ResultsCursor<'mir, 'tcx, MaybeMutBorrowedLocals<'mir, 'tcx>>;

type QualifResults<'mir, 'tcx, Q> =
    dataflow::ResultsCursor<'mir, 'tcx, FlowSensitiveAnalysis<'mir, 'mir, 'tcx, Q>>;

#[derive(Default)]
pub struct Qualifs<'mir, 'tcx> {
    has_mut_interior: Option<QualifResults<'mir, 'tcx, HasMutInterior>>,
    needs_drop: Option<QualifResults<'mir, 'tcx, NeedsDrop>>,
    indirectly_mutable: Option<IndirectlyMutableResults<'mir, 'tcx>>,
}

impl Qualifs<'mir, 'tcx> {
    pub fn indirectly_mutable(
        &mut self,
        ccx: &'mir ConstCx<'mir, 'tcx>,
        local: Local,
        location: Location,
    ) -> bool {
        let indirectly_mutable = self.indirectly_mutable.get_or_insert_with(|| {
            let ConstCx { tcx, body, param_env, .. } = *ccx;

            // We can use `unsound_ignore_borrow_on_drop` here because custom drop impls are not
            // allowed in a const.
            //
            // FIXME(ecstaticmorse): Someday we want to allow custom drop impls. How do we do this
            // without breaking stable code?
            MaybeMutBorrowedLocals::mut_borrows_only(tcx, &body, param_env)
                .unsound_ignore_borrow_on_drop()
                .into_engine(tcx, &body)
                .pass_name("const_qualification")
                .iterate_to_fixpoint()
                .into_results_cursor(&body)
        });

        indirectly_mutable.seek_before_primary_effect(location);
        indirectly_mutable.get().contains(local)
    }

    /// Returns `true` if `local` is `NeedsDrop` at the given `Location`.
    ///
    /// Only updates the cursor if absolutely necessary
    pub fn needs_drop(
        &mut self,
        ccx: &'mir ConstCx<'mir, 'tcx>,
        local: Local,
        location: Location,
    ) -> bool {
        let ty = ccx.body.local_decls[local].ty;
        if !NeedsDrop::in_any_value_of_ty(ccx, ty) {
            return false;
        }

        let needs_drop = self.needs_drop.get_or_insert_with(|| {
            let ConstCx { tcx, body, .. } = *ccx;

            FlowSensitiveAnalysis::new(NeedsDrop, ccx)
                .into_engine(tcx, &body)
                .iterate_to_fixpoint()
                .into_results_cursor(&body)
        });

        needs_drop.seek_before_primary_effect(location);
        needs_drop.get().contains(local) || self.indirectly_mutable(ccx, local, location)
    }

    /// Returns `true` if `local` is `HasMutInterior` at the given `Location`.
    ///
    /// Only updates the cursor if absolutely necessary.
    pub fn has_mut_interior(
        &mut self,
        ccx: &'mir ConstCx<'mir, 'tcx>,
        local: Local,
        location: Location,
    ) -> bool {
        let ty = ccx.body.local_decls[local].ty;
        if !HasMutInterior::in_any_value_of_ty(ccx, ty) {
            return false;
        }

        let has_mut_interior = self.has_mut_interior.get_or_insert_with(|| {
            let ConstCx { tcx, body, .. } = *ccx;

            FlowSensitiveAnalysis::new(HasMutInterior, ccx)
                .into_engine(tcx, &body)
                .iterate_to_fixpoint()
                .into_results_cursor(&body)
        });

        has_mut_interior.seek_before_primary_effect(location);
        has_mut_interior.get().contains(local) || self.indirectly_mutable(ccx, local, location)
    }

    fn in_return_place(
        &mut self,
        ccx: &'mir ConstCx<'mir, 'tcx>,
        error_occured: Option<ErrorReported>,
    ) -> ConstQualifs {
        // Find the `Return` terminator if one exists.
        //
        // If no `Return` terminator exists, this MIR is divergent. Just return the conservative
        // qualifs for the return type.
        let return_block = ccx
            .body
            .basic_blocks()
            .iter_enumerated()
            .find(|(_, block)| match block.terminator().kind {
                TerminatorKind::Return => true,
                _ => false,
            })
            .map(|(bb, _)| bb);

        let return_block = match return_block {
            None => return qualifs::in_any_value_of_ty(ccx, ccx.body.return_ty(), error_occured),
            Some(bb) => bb,
        };

        let return_loc = ccx.body.terminator_loc(return_block);

        let custom_eq = match ccx.const_kind() {
            // We don't care whether a `const fn` returns a value that is not structurally
            // matchable. Functions calls are opaque and always use type-based qualification, so
            // this value should never be used.
            hir::ConstContext::ConstFn => true,

            // If we know that all values of the return type are structurally matchable, there's no
            // need to run dataflow.
            _ if !CustomEq::in_any_value_of_ty(ccx, ccx.body.return_ty()) => false,

            hir::ConstContext::Const | hir::ConstContext::Static(_) => {
                let mut cursor = FlowSensitiveAnalysis::new(CustomEq, ccx)
                    .into_engine(ccx.tcx, &ccx.body)
                    .iterate_to_fixpoint()
                    .into_results_cursor(&ccx.body);

                cursor.seek_after_primary_effect(return_loc);
                cursor.contains(RETURN_PLACE)
            }
        };

        ConstQualifs {
            needs_drop: self.needs_drop(ccx, RETURN_PLACE, return_loc),
            has_mut_interior: self.has_mut_interior(ccx, RETURN_PLACE, return_loc),
            custom_eq,
            error_occured,
        }
    }
}

pub struct Validator<'mir, 'tcx> {
    ccx: &'mir ConstCx<'mir, 'tcx>,
    qualifs: Qualifs<'mir, 'tcx>,

    /// The span of the current statement.
    span: Span,

    /// A set that stores for each local whether it has a `StorageDead` for it somewhere.
    local_has_storage_dead: Option<BitSet<Local>>,

    error_emitted: Option<ErrorReported>,
    secondary_errors: Vec<Diagnostic>,
}

impl Deref for Validator<'mir, 'tcx> {
    type Target = ConstCx<'mir, 'tcx>;

    fn deref(&self) -> &Self::Target {
        &self.ccx
    }
}

impl Validator<'mir, 'tcx> {
    pub fn new(ccx: &'mir ConstCx<'mir, 'tcx>) -> Self {
        Validator {
            span: ccx.body.span,
            ccx,
            qualifs: Default::default(),
            local_has_storage_dead: None,
            error_emitted: None,
            secondary_errors: Vec::new(),
        }
    }

    pub fn check_body(&mut self) {
        let ConstCx { tcx, body, .. } = *self.ccx;
        let def_id = self.ccx.def_id();

        // `async` functions cannot be `const fn`. This is checked during AST lowering, so there's
        // no need to emit duplicate errors here.
        if is_async_fn(self.ccx) || body.generator.is_some() {
            tcx.sess.delay_span_bug(body.span, "`async` functions cannot be `const fn`");
            return;
        }

        // The local type and predicate checks are not free and only relevant for `const fn`s.
        if self.const_kind() == hir::ConstContext::ConstFn {
            // Prevent const trait methods from being annotated as `stable`.
            // FIXME: Do this as part of stability checking.
            if self.is_const_stable_const_fn() {
                let hir_id = tcx.hir().local_def_id_to_hir_id(def_id);
                if crate::const_eval::is_parent_const_impl_raw(tcx, hir_id) {
                    self.ccx
                        .tcx
                        .sess
                        .struct_span_err(self.span, "trait methods cannot be stable const fn")
                        .emit();
                }
            }

            self.check_item_predicates();

            for (idx, local) in body.local_decls.iter_enumerated() {
                // Handle the return place below.
                if idx == RETURN_PLACE || local.internal {
                    continue;
                }

                self.span = local.source_info.span;
                self.check_local_or_return_ty(local.ty, idx);
            }

            // impl trait is gone in MIR, so check the return type of a const fn by its signature
            // instead of the type of the return place.
            self.span = body.local_decls[RETURN_PLACE].source_info.span;
            let return_ty = tcx.fn_sig(def_id).output();
            self.check_local_or_return_ty(return_ty.skip_binder(), RETURN_PLACE);
        }

        self.visit_body(&body);

        // Ensure that the end result is `Sync` in a non-thread local `static`.
        let should_check_for_sync = self.const_kind()
            == hir::ConstContext::Static(hir::Mutability::Not)
            && !tcx.is_thread_local_static(def_id.to_def_id());

        if should_check_for_sync {
            let hir_id = tcx.hir().local_def_id_to_hir_id(def_id);
            check_return_ty_is_sync(tcx, &body, hir_id);
        }

        // If we got through const-checking without emitting any "primary" errors, emit any
        // "secondary" errors if they occurred.
        let secondary_errors = mem::take(&mut self.secondary_errors);
        if self.error_emitted.is_none() {
            for error in secondary_errors {
                self.tcx.sess.diagnostic().emit_diagnostic(&error);
            }
        } else {
            assert!(self.tcx.sess.has_errors());
        }
    }

    fn local_has_storage_dead(&mut self, local: Local) -> bool {
        let ccx = self.ccx;
        self.local_has_storage_dead
            .get_or_insert_with(|| {
                struct StorageDeads {
                    locals: BitSet<Local>,
                }
                impl Visitor<'tcx> for StorageDeads {
                    fn visit_statement(&mut self, stmt: &Statement<'tcx>, _: Location) {
                        if let StatementKind::StorageDead(l) = stmt.kind {
                            self.locals.insert(l);
                        }
                    }
                }
                let mut v = StorageDeads { locals: BitSet::new_empty(ccx.body.local_decls.len()) };
                v.visit_body(ccx.body);
                v.locals
            })
            .contains(local)
    }

    pub fn qualifs_in_return_place(&mut self) -> ConstQualifs {
        self.qualifs.in_return_place(self.ccx, self.error_emitted)
    }

    /// Emits an error if an expression cannot be evaluated in the current context.
    pub fn check_op(&mut self, op: impl NonConstOp) {
        self.check_op_spanned(op, self.span);
    }

    /// Emits an error at the given `span` if an expression cannot be evaluated in the current
    /// context.
    pub fn check_op_spanned<O: NonConstOp>(&mut self, op: O, span: Span) {
        let gate = match op.status_in_item(self.ccx) {
            Status::Allowed => return,

            Status::Unstable(gate) if self.tcx.features().enabled(gate) => {
                let unstable_in_stable = self.ccx.is_const_stable_const_fn()
                    && !super::rustc_allow_const_fn_unstable(
                        self.tcx,
                        self.def_id().to_def_id(),
                        gate,
                    );
                if unstable_in_stable {
                    emit_unstable_in_stable_error(self.ccx, span, gate);
                }

                return;
            }

            Status::Unstable(gate) => Some(gate),
            Status::Forbidden => None,
        };

        if self.tcx.sess.opts.debugging_opts.unleash_the_miri_inside_of_you {
            self.tcx.sess.miri_unleashed_feature(span, gate);
            return;
        }

        let mut err = op.build_error(self.ccx, span);
        assert!(err.is_error());

        match op.importance() {
            ops::DiagnosticImportance::Primary => {
                self.error_emitted = Some(ErrorReported);
                err.emit();
            }

            ops::DiagnosticImportance::Secondary => err.buffer(&mut self.secondary_errors),
        }
    }

    fn check_static(&mut self, def_id: DefId, span: Span) {
        assert!(
            !self.tcx.is_thread_local_static(def_id),
            "tls access is checked in `Rvalue::ThreadLocalRef"
        );
        self.check_op_spanned(ops::StaticAccess, span)
    }

    fn check_local_or_return_ty(&mut self, ty: Ty<'tcx>, local: Local) {
        let kind = self.body.local_kind(local);

        for ty in ty.walk() {
            let ty = match ty.unpack() {
                GenericArgKind::Type(ty) => ty,

                // No constraints on lifetimes or constants, except potentially
                // constants' types, but `walk` will get to them as well.
                GenericArgKind::Lifetime(_) | GenericArgKind::Const(_) => continue,
            };

            match *ty.kind() {
                ty::Ref(_, _, hir::Mutability::Mut) => self.check_op(ops::ty::MutRef(kind)),
                ty::Opaque(..) => self.check_op(ops::ty::ImplTrait),
                ty::FnPtr(..) => self.check_op(ops::ty::FnPtr(kind)),

                ty::Dynamic(preds, _) => {
                    for pred in preds.iter() {
                        match pred.skip_binder() {
                            ty::ExistentialPredicate::AutoTrait(_)
                            | ty::ExistentialPredicate::Projection(_) => {
                                self.check_op(ops::ty::TraitBound(kind))
                            }
                            ty::ExistentialPredicate::Trait(trait_ref) => {
                                if Some(trait_ref.def_id) != self.tcx.lang_items().sized_trait() {
                                    self.check_op(ops::ty::TraitBound(kind))
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn check_item_predicates(&mut self) {
        let ConstCx { tcx, .. } = *self.ccx;

        let mut current = self.def_id().to_def_id();
        loop {
            let predicates = tcx.predicates_of(current);
            for (predicate, _) in predicates.predicates {
                match predicate.kind().skip_binder() {
                    ty::PredicateKind::RegionOutlives(_)
                    | ty::PredicateKind::TypeOutlives(_)
                    | ty::PredicateKind::WellFormed(_)
                    | ty::PredicateKind::Projection(_)
                    | ty::PredicateKind::ConstEvaluatable(..)
                    | ty::PredicateKind::ConstEquate(..)
                    | ty::PredicateKind::TypeWellFormedFromEnv(..) => continue,
                    ty::PredicateKind::ObjectSafe(_) => {
                        bug!("object safe predicate on function: {:#?}", predicate)
                    }
                    ty::PredicateKind::ClosureKind(..) => {
                        bug!("closure kind predicate on function: {:#?}", predicate)
                    }
                    ty::PredicateKind::Subtype(_) => {
                        bug!("subtype predicate on function: {:#?}", predicate)
                    }
                    ty::PredicateKind::Trait(pred, _constness) => {
                        if Some(pred.def_id()) == tcx.lang_items().sized_trait() {
                            continue;
                        }
                        match pred.self_ty().kind() {
                            ty::Param(p) => {
                                let generics = tcx.generics_of(current);
                                let def = generics.type_param(p, tcx);
                                let span = tcx.def_span(def.def_id);

                                // These are part of the function signature, so treat them like
                                // arguments when determining importance.
                                let kind = LocalKind::Arg;

                                self.check_op_spanned(ops::ty::TraitBound(kind), span);
                            }
                            // other kinds of bounds are either tautologies
                            // or cause errors in other passes
                            _ => continue,
                        }
                    }
                }
            }
            match predicates.parent {
                Some(parent) => current = parent,
                None => break,
            }
        }
    }

    fn check_mut_borrow(&mut self, local: Local, kind: hir::BorrowKind) {
        match self.const_kind() {
            // In a const fn all borrows are transient or point to the places given via
            // references in the arguments (so we already checked them with
            // TransientMutBorrow/MutBorrow as appropriate).
            // The borrow checker guarantees that no new non-transient borrows are created.
            // NOTE: Once we have heap allocations during CTFE we need to figure out
            // how to prevent `const fn` to create long-lived allocations that point
            // to mutable memory.
            hir::ConstContext::ConstFn => self.check_op(ops::TransientMutBorrow(kind)),
            _ => {
                // Locals with StorageDead do not live beyond the evaluation and can
                // thus safely be borrowed without being able to be leaked to the final
                // value of the constant.
                if self.local_has_storage_dead(local) {
                    self.check_op(ops::TransientMutBorrow(kind));
                } else {
                    self.check_op(ops::MutBorrow(kind));
                }
            }
        }
    }
}

impl Visitor<'tcx> for Validator<'mir, 'tcx> {
    fn visit_basic_block_data(&mut self, bb: BasicBlock, block: &BasicBlockData<'tcx>) {
        trace!("visit_basic_block_data: bb={:?} is_cleanup={:?}", bb, block.is_cleanup);

        // We don't const-check basic blocks on the cleanup path since we never unwind during
        // const-eval: a panic causes an immediate compile error. In other words, cleanup blocks
        // are unreachable during const-eval.
        //
        // We can't be more conservative (e.g., by const-checking cleanup blocks anyways) because
        // locals that would never be dropped during normal execution are sometimes dropped during
        // unwinding, which means backwards-incompatible live-drop errors.
        if block.is_cleanup {
            return;
        }

        self.super_basic_block_data(bb, block);
    }

    fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: Location) {
        trace!("visit_rvalue: rvalue={:?} location={:?}", rvalue, location);

        // Special-case reborrows to be more like a copy of a reference.
        match *rvalue {
            Rvalue::Ref(_, kind, place) => {
                if let Some(reborrowed_place_ref) = place_as_reborrow(self.tcx, self.body, place) {
                    let ctx = match kind {
                        BorrowKind::Shared => {
                            PlaceContext::NonMutatingUse(NonMutatingUseContext::SharedBorrow)
                        }
                        BorrowKind::Shallow => {
                            PlaceContext::NonMutatingUse(NonMutatingUseContext::ShallowBorrow)
                        }
                        BorrowKind::Unique => {
                            PlaceContext::NonMutatingUse(NonMutatingUseContext::UniqueBorrow)
                        }
                        BorrowKind::Mut { .. } => {
                            PlaceContext::MutatingUse(MutatingUseContext::Borrow)
                        }
                    };
                    self.visit_local(&reborrowed_place_ref.local, ctx, location);
                    self.visit_projection(reborrowed_place_ref, ctx, location);
                    return;
                }
            }
            Rvalue::AddressOf(mutbl, place) => {
                if let Some(reborrowed_place_ref) = place_as_reborrow(self.tcx, self.body, place) {
                    let ctx = match mutbl {
                        Mutability::Not => {
                            PlaceContext::NonMutatingUse(NonMutatingUseContext::AddressOf)
                        }
                        Mutability::Mut => PlaceContext::MutatingUse(MutatingUseContext::AddressOf),
                    };
                    self.visit_local(&reborrowed_place_ref.local, ctx, location);
                    self.visit_projection(reborrowed_place_ref, ctx, location);
                    return;
                }
            }
            _ => {}
        }

        self.super_rvalue(rvalue, location);

        match *rvalue {
            Rvalue::ThreadLocalRef(_) => self.check_op(ops::ThreadLocalAccess),

            Rvalue::Use(_)
            | Rvalue::Repeat(..)
            | Rvalue::Discriminant(..)
            | Rvalue::Len(_)
            | Rvalue::Aggregate(..) => {}

            Rvalue::Ref(_, kind @ BorrowKind::Mut { .. }, ref place)
            | Rvalue::Ref(_, kind @ BorrowKind::Unique, ref place) => {
                let ty = place.ty(self.body, self.tcx).ty;
                let is_allowed = match ty.kind() {
                    // Inside a `static mut`, `&mut [...]` is allowed.
                    ty::Array(..) | ty::Slice(_)
                        if self.const_kind() == hir::ConstContext::Static(hir::Mutability::Mut) =>
                    {
                        true
                    }

                    // FIXME(ecstaticmorse): We could allow `&mut []` inside a const context given
                    // that this is merely a ZST and it is already eligible for promotion.
                    // This may require an RFC?
                    /*
                    ty::Array(_, len) if len.try_eval_usize(cx.tcx, cx.param_env) == Some(0)
                        => true,
                    */
                    _ => false,
                };

                if !is_allowed {
                    if let BorrowKind::Mut { .. } = kind {
                        self.check_mut_borrow(place.local, hir::BorrowKind::Ref)
                    } else {
                        self.check_op(ops::CellBorrow);
                    }
                }
            }

            Rvalue::AddressOf(Mutability::Mut, ref place) => {
                self.check_mut_borrow(place.local, hir::BorrowKind::Raw)
            }

            Rvalue::Ref(_, BorrowKind::Shared | BorrowKind::Shallow, ref place)
            | Rvalue::AddressOf(Mutability::Not, ref place) => {
                let borrowed_place_has_mut_interior = qualifs::in_place::<HasMutInterior, _>(
                    &self.ccx,
                    &mut |local| self.qualifs.has_mut_interior(self.ccx, local, location),
                    place.as_ref(),
                );

                if borrowed_place_has_mut_interior {
                    match self.const_kind() {
                        // In a const fn all borrows are transient or point to the places given via
                        // references in the arguments (so we already checked them with
                        // TransientCellBorrow/CellBorrow as appropriate).
                        // The borrow checker guarantees that no new non-transient borrows are created.
                        // NOTE: Once we have heap allocations during CTFE we need to figure out
                        // how to prevent `const fn` to create long-lived allocations that point
                        // to (interior) mutable memory.
                        hir::ConstContext::ConstFn => self.check_op(ops::TransientCellBorrow),
                        _ => {
                            // Locals with StorageDead are definitely not part of the final constant value, and
                            // it is thus inherently safe to permit such locals to have their
                            // address taken as we can't end up with a reference to them in the
                            // final value.
                            // Note: This is only sound if every local that has a `StorageDead` has a
                            // `StorageDead` in every control flow path leading to a `return` terminator.
                            if self.local_has_storage_dead(place.local) {
                                self.check_op(ops::TransientCellBorrow);
                            } else {
                                self.check_op(ops::CellBorrow);
                            }
                        }
                    }
                }
            }

            Rvalue::Cast(
                CastKind::Pointer(PointerCast::MutToConstPointer | PointerCast::ArrayToPointer),
                _,
                _,
            ) => {}

            Rvalue::Cast(
                CastKind::Pointer(
                    PointerCast::UnsafeFnPointer
                    | PointerCast::ClosureFnPointer(_)
                    | PointerCast::ReifyFnPointer,
                ),
                _,
                _,
            ) => self.check_op(ops::FnPtrCast),

            Rvalue::Cast(CastKind::Pointer(PointerCast::Unsize), _, _) => {
                // Nothing to check here (`check_local_or_return_ty` ensures no trait objects occur
                // in the type of any local, which also excludes casts).
            }

            Rvalue::Cast(CastKind::Misc, ref operand, cast_ty) => {
                let operand_ty = operand.ty(self.body, self.tcx);
                let cast_in = CastTy::from_ty(operand_ty).expect("bad input type for cast");
                let cast_out = CastTy::from_ty(cast_ty).expect("bad output type for cast");

                if let (CastTy::Ptr(_) | CastTy::FnPtr, CastTy::Int(_)) = (cast_in, cast_out) {
                    self.check_op(ops::RawPtrToIntCast);
                }
            }

            Rvalue::NullaryOp(NullOp::SizeOf, _) => {}
            Rvalue::NullaryOp(NullOp::Box, _) => self.check_op(ops::HeapAllocation),

            Rvalue::UnaryOp(_, ref operand) => {
                let ty = operand.ty(self.body, self.tcx);
                if is_int_bool_or_char(ty) {
                    // Int, bool, and char operations are fine.
                } else if ty.is_floating_point() {
                    self.check_op(ops::FloatingPointOp);
                } else {
                    span_bug!(self.span, "non-primitive type in `Rvalue::UnaryOp`: {:?}", ty);
                }
            }

            Rvalue::BinaryOp(op, box (ref lhs, ref rhs))
            | Rvalue::CheckedBinaryOp(op, box (ref lhs, ref rhs)) => {
                let lhs_ty = lhs.ty(self.body, self.tcx);
                let rhs_ty = rhs.ty(self.body, self.tcx);

                if is_int_bool_or_char(lhs_ty) && is_int_bool_or_char(rhs_ty) {
                    // Int, bool, and char operations are fine.
                } else if lhs_ty.is_fn_ptr() || lhs_ty.is_unsafe_ptr() {
                    assert_eq!(lhs_ty, rhs_ty);
                    assert!(
                        op == BinOp::Eq
                            || op == BinOp::Ne
                            || op == BinOp::Le
                            || op == BinOp::Lt
                            || op == BinOp::Ge
                            || op == BinOp::Gt
                            || op == BinOp::Offset
                    );

                    self.check_op(ops::RawPtrComparison);
                } else if lhs_ty.is_floating_point() || rhs_ty.is_floating_point() {
                    self.check_op(ops::FloatingPointOp);
                } else {
                    span_bug!(
                        self.span,
                        "non-primitive type in `Rvalue::BinaryOp`: {:?} ⚬ {:?}",
                        lhs_ty,
                        rhs_ty
                    );
                }
            }
        }
    }

    fn visit_operand(&mut self, op: &Operand<'tcx>, location: Location) {
        self.super_operand(op, location);
        if let Operand::Constant(c) = op {
            if let Some(def_id) = c.check_static_ptr(self.tcx) {
                self.check_static(def_id, self.span);
            }
        }
    }
    fn visit_projection_elem(
        &mut self,
        place_local: Local,
        proj_base: &[PlaceElem<'tcx>],
        elem: PlaceElem<'tcx>,
        context: PlaceContext,
        location: Location,
    ) {
        trace!(
            "visit_projection_elem: place_local={:?} proj_base={:?} elem={:?} \
            context={:?} location={:?}",
            place_local,
            proj_base,
            elem,
            context,
            location,
        );

        self.super_projection_elem(place_local, proj_base, elem, context, location);

        match elem {
            ProjectionElem::Deref => {
                let base_ty = Place::ty_from(place_local, proj_base, self.body, self.tcx).ty;
                if let ty::RawPtr(_) = base_ty.kind() {
                    if proj_base.is_empty() {
                        if let (local, []) = (place_local, proj_base) {
                            let decl = &self.body.local_decls[local];
                            if let Some(box LocalInfo::StaticRef {
                                def_id,
                                is_thread_local: false,
                            }) = decl.local_info
                            {
                                let span = decl.source_info.span;
                                self.check_static(def_id, span);
                                return;
                            }
                        }
                    }
                    self.check_op(ops::RawPtrDeref);
                }

                if context.is_mutating_use() {
                    self.check_op(ops::MutDeref);
                }
            }

            ProjectionElem::ConstantIndex { .. }
            | ProjectionElem::Downcast(..)
            | ProjectionElem::Subslice { .. }
            | ProjectionElem::Field(..)
            | ProjectionElem::Index(_) => {
                let base_ty = Place::ty_from(place_local, proj_base, self.body, self.tcx).ty;
                match base_ty.ty_adt_def() {
                    Some(def) if def.is_union() => {
                        self.check_op(ops::UnionAccess);
                    }

                    _ => {}
                }
            }
        }
    }

    fn visit_source_info(&mut self, source_info: &SourceInfo) {
        trace!("visit_source_info: source_info={:?}", source_info);
        self.span = source_info.span;
    }

    fn visit_statement(&mut self, statement: &Statement<'tcx>, location: Location) {
        trace!("visit_statement: statement={:?} location={:?}", statement, location);

        self.super_statement(statement, location);

        match statement.kind {
            StatementKind::LlvmInlineAsm { .. } => {
                self.check_op(ops::InlineAsm);
            }

            StatementKind::Assign(..)
            | StatementKind::SetDiscriminant { .. }
            | StatementKind::FakeRead(..)
            | StatementKind::StorageLive(_)
            | StatementKind::StorageDead(_)
            | StatementKind::Retag { .. }
            | StatementKind::AscribeUserType(..)
            | StatementKind::Coverage(..)
            | StatementKind::CopyNonOverlapping(..)
            | StatementKind::Nop => {}
        }
    }

    #[instrument(level = "debug", skip(self))]
    fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, location: Location) {
        use rustc_target::spec::abi::Abi::RustIntrinsic;

        self.super_terminator(terminator, location);

        match &terminator.kind {
            TerminatorKind::Call { func, args, .. } => {
                let ConstCx { tcx, body, param_env, .. } = *self.ccx;
                let caller = self.def_id().to_def_id();

                let fn_ty = func.ty(body, tcx);

                let (mut callee, substs) = match *fn_ty.kind() {
                    ty::FnDef(def_id, substs) => (def_id, substs),

                    ty::FnPtr(_) => {
                        self.check_op(ops::FnCallIndirect);
                        return;
                    }
                    _ => {
                        span_bug!(terminator.source_info.span, "invalid callee of type {:?}", fn_ty)
                    }
                };

                // Attempting to call a trait method?
                if let Some(trait_id) = tcx.trait_of_item(callee) {
                    trace!("attempting to call a trait method");
                    if !self.tcx.features().const_trait_impl {
                        self.check_op(ops::FnCallNonConst);
                        return;
                    }

                    let trait_ref = TraitRef::from_method(tcx, trait_id, substs);
                    let obligation = Obligation::new(
                        ObligationCause::dummy(),
                        param_env,
                        Binder::bind(
                            TraitPredicate {
                                trait_ref: TraitRef::from_method(tcx, trait_id, substs),
                            },
                            tcx,
                        ),
                    );

                    let implsrc = tcx.infer_ctxt().enter(|infcx| {
                        let mut selcx = SelectionContext::new(&infcx);
                        selcx.select(&obligation).unwrap()
                    });

                    // If the method is provided via a where-clause that does not use the `?const`
                    // opt-out, the call is allowed.
                    if let Some(ImplSource::Param(_, hir::Constness::Const)) = implsrc {
                        debug!(
                            "const_trait_impl: provided {:?} via where-clause in {:?}",
                            trait_ref, param_env
                        );
                        return;
                    }

                    // Resolve a trait method call to its concrete implementation, which may be in a
                    // `const` trait impl.
                    let instance = Instance::resolve(tcx, param_env, callee, substs);
                    debug!("Resolving ({:?}) -> {:?}", callee, instance);
                    if let Ok(Some(func)) = instance {
                        if let InstanceDef::Item(def) = func.def {
                            callee = def.did;
                        }
                    }
                }

                // At this point, we are calling a function, `callee`, whose `DefId` is known...
                if is_lang_panic_fn(tcx, callee) {
                    self.check_op(ops::Panic);

                    // const-eval of the `begin_panic` fn assumes the argument is `&str`
                    if Some(callee) == tcx.lang_items().begin_panic_fn() {
                        match args[0].ty(&self.ccx.body.local_decls, tcx).kind() {
                            ty::Ref(_, ty, _) if ty.is_str() => (),
                            _ => self.check_op(ops::PanicNonStr),
                        }
                    }

                    return;
                }

                // `async` blocks get lowered to `std::future::from_generator(/* a closure */)`.
                let is_async_block = Some(callee) == tcx.lang_items().from_generator_fn();
                if is_async_block {
                    let kind = hir::GeneratorKind::Async(hir::AsyncGeneratorKind::Block);
                    self.check_op(ops::Generator(kind));
                    return;
                }

                let is_intrinsic = tcx.fn_sig(callee).abi() == RustIntrinsic;

                // HACK: This is to "unstabilize" the `transmute` intrinsic
                // within const fns. `transmute` is allowed in all other const contexts.
                // This won't really scale to more intrinsics or functions. Let's allow const
                // transmutes in const fn before we add more hacks to this.
                if is_intrinsic && tcx.item_name(callee) == sym::transmute {
                    self.check_op(ops::Transmute);
                    return;
                }

                if !tcx.is_const_fn_raw(callee) {
                    self.check_op(ops::FnCallNonConst);
                    return;
                }

                // If the `const fn` we are trying to call is not const-stable, ensure that we have
                // the proper feature gate enabled.
                if let Some(gate) = is_unstable_const_fn(tcx, callee) {
                    trace!(?gate, "calling unstable const fn");
                    if self.span.allows_unstable(gate) {
                        return;
                    }

                    // Calling an unstable function *always* requires that the corresponding gate
                    // be enabled, even if the function has `#[rustc_allow_const_fn_unstable(the_gate)]`.
                    if !tcx.features().declared_lib_features.iter().any(|&(sym, _)| sym == gate) {
                        self.check_op(ops::FnCallUnstable(callee, Some(gate)));
                        return;
                    }

                    // If this crate is not using stability attributes, or the caller is not claiming to be a
                    // stable `const fn`, that is all that is required.
                    if !self.ccx.is_const_stable_const_fn() {
                        trace!("crate not using stability attributes or caller not stably const");
                        return;
                    }

                    // Otherwise, we are something const-stable calling a const-unstable fn.

                    if super::rustc_allow_const_fn_unstable(tcx, caller, gate) {
                        trace!("rustc_allow_const_fn_unstable gate active");
                        return;
                    }

                    self.check_op(ops::FnCallUnstable(callee, Some(gate)));
                    return;
                }

                // FIXME(ecstaticmorse); For compatibility, we consider `unstable` callees that
                // have no `rustc_const_stable` attributes to be const-unstable as well. This
                // should be fixed later.
                let callee_is_unstable_unmarked = tcx.lookup_const_stability(callee).is_none()
                    && tcx.lookup_stability(callee).map_or(false, |s| s.level.is_unstable());
                if callee_is_unstable_unmarked {
                    trace!("callee_is_unstable_unmarked");
                    // We do not use `const` modifiers for intrinsic "functions", as intrinsics are
                    // `extern` funtions, and these have no way to get marked `const`. So instead we
                    // use `rustc_const_(un)stable` attributes to mean that the intrinsic is `const`
                    if self.ccx.is_const_stable_const_fn() || is_intrinsic {
                        self.check_op(ops::FnCallUnstable(callee, None));
                        return;
                    }
                }
                trace!("permitting call");
            }

            // Forbid all `Drop` terminators unless the place being dropped is a local with no
            // projections that cannot be `NeedsDrop`.
            TerminatorKind::Drop { place: dropped_place, .. }
            | TerminatorKind::DropAndReplace { place: dropped_place, .. } => {
                // If we are checking live drops after drop-elaboration, don't emit duplicate
                // errors here.
                if super::post_drop_elaboration::checking_enabled(self.ccx) {
                    return;
                }

                let mut err_span = self.span;

                // Check to see if the type of this place can ever have a drop impl. If not, this
                // `Drop` terminator is frivolous.
                let ty_needs_drop =
                    dropped_place.ty(self.body, self.tcx).ty.needs_drop(self.tcx, self.param_env);

                if !ty_needs_drop {
                    return;
                }

                let needs_drop = if let Some(local) = dropped_place.as_local() {
                    // Use the span where the local was declared as the span of the drop error.
                    err_span = self.body.local_decls[local].source_info.span;
                    self.qualifs.needs_drop(self.ccx, local, location)
                } else {
                    true
                };

                if needs_drop {
                    self.check_op_spanned(
                        ops::LiveDrop { dropped_at: Some(terminator.source_info.span) },
                        err_span,
                    );
                }
            }

            TerminatorKind::InlineAsm { .. } => self.check_op(ops::InlineAsm),

            TerminatorKind::GeneratorDrop | TerminatorKind::Yield { .. } => {
                self.check_op(ops::Generator(hir::GeneratorKind::Gen))
            }

            TerminatorKind::Abort => {
                // Cleanup blocks are skipped for const checking (see `visit_basic_block_data`).
                span_bug!(self.span, "`Abort` terminator outside of cleanup block")
            }

            TerminatorKind::Assert { .. }
            | TerminatorKind::FalseEdge { .. }
            | TerminatorKind::FalseUnwind { .. }
            | TerminatorKind::Goto { .. }
            | TerminatorKind::Resume
            | TerminatorKind::Return
            | TerminatorKind::SwitchInt { .. }
            | TerminatorKind::Unreachable => {}
        }
    }
}

fn check_return_ty_is_sync(tcx: TyCtxt<'tcx>, body: &Body<'tcx>, hir_id: HirId) {
    let ty = body.return_ty();
    tcx.infer_ctxt().enter(|infcx| {
        let cause = traits::ObligationCause::new(body.span, hir_id, traits::SharedStatic);
        let mut fulfillment_cx = traits::FulfillmentContext::new();
        let sync_def_id = tcx.require_lang_item(LangItem::Sync, Some(body.span));
        fulfillment_cx.register_bound(&infcx, ty::ParamEnv::empty(), ty, sync_def_id, cause);
        if let Err(err) = fulfillment_cx.select_all_or_error(&infcx) {
            infcx.report_fulfillment_errors(&err, None, false);
        }
    });
}

fn place_as_reborrow(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    place: Place<'tcx>,
) -> Option<PlaceRef<'tcx>> {
    match place.as_ref().last_projection() {
        Some((place_base, ProjectionElem::Deref)) => {
            // A borrow of a `static` also looks like `&(*_1)` in the MIR, but `_1` is a `const`
            // that points to the allocation for the static. Don't treat these as reborrows.
            if body.local_decls[place_base.local].is_ref_to_static() {
                None
            } else {
                // Ensure the type being derefed is a reference and not a raw pointer.
                // This is sufficient to prevent an access to a `static mut` from being marked as a
                // reborrow, even if the check above were to disappear.
                let inner_ty = place_base.ty(body, tcx).ty;

                if let ty::Ref(..) = inner_ty.kind() {
                    return Some(place_base);
                } else {
                    return None;
                }
            }
        }
        _ => None,
    }
}

fn is_int_bool_or_char(ty: Ty<'_>) -> bool {
    ty.is_bool() || ty.is_integral() || ty.is_char()
}

fn is_async_fn(ccx: &ConstCx<'_, '_>) -> bool {
    ccx.fn_sig().map_or(false, |sig| sig.header.asyncness == hir::IsAsync::Async)
}

fn emit_unstable_in_stable_error(ccx: &ConstCx<'_, '_>, span: Span, gate: Symbol) {
    let attr_span = ccx.fn_sig().map_or(ccx.body.span, |sig| sig.span.shrink_to_lo());

    ccx.tcx
        .sess
        .struct_span_err(
            span,
            &format!("const-stable function cannot use `#[feature({})]`", gate.as_str()),
        )
        .span_suggestion(
            attr_span,
            "if it is not part of the public API, make this function unstably const",
            concat!(r#"#[rustc_const_unstable(feature = "...", issue = "...")]"#, '\n').to_owned(),
            Applicability::HasPlaceholders,
        )
        .span_suggestion(
            attr_span,
            "otherwise `#[rustc_allow_const_fn_unstable]` can be used to bypass stability checks",
            format!("#[rustc_allow_const_fn_unstable({})]\n", gate),
            Applicability::MaybeIncorrect,
        )
        .emit();
}
