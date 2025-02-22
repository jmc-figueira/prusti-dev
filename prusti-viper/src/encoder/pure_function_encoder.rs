// © 2019, ETH Zurich
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

use crate::encoder::borrows::{compute_procedure_contract, ProcedureContract};
use crate::encoder::builtin_encoder::BuiltinFunctionKind;
use crate::encoder::errors::{PanicCause, RunIfErr};
use crate::encoder::errors::{SpannedEncodingError, ErrorCtxt, WithSpan};
use crate::encoder::foldunfold;
use crate::encoder::mir_encoder::{MirEncoder, PlaceEncoder};
use crate::encoder::mir_encoder::{PRECONDITION_LABEL, WAND_LHS_LABEL};
use crate::encoder::mir_interpreter::{
    run_backward_interpretation, BackwardMirInterpreter, MultiExprBackwardInterpreterState,
};
use crate::encoder::snapshot;
use crate::encoder::Encoder;
use crate::encoder::snapshot_spec_patcher::SnapshotSpecPatcher;
use prusti_common::vir;
use prusti_common::vir::ExprIterator;
use prusti_common::config;
use prusti_interface::specs::typed;
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_middle::mir;
use rustc_middle::ty;
use std::collections::HashMap;
use log::{debug, trace};
use prusti_interface::PrustiError;
use rustc_span::Span;
use crate::encoder::errors::EncodingResult;
use crate::encoder::errors::SpannedEncodingResult;

pub struct PureFunctionEncoder<'p, 'v: 'p, 'tcx: 'v> {
    encoder: &'p Encoder<'v, 'tcx>,
    proc_def_id: DefId,
    mir: &'p mir::Body<'tcx>,
    interpreter: PureFunctionBackwardInterpreter<'p, 'v, 'tcx>,
}

impl<'p, 'v: 'p, 'tcx: 'v> PureFunctionEncoder<'p, 'v, 'tcx> {
    pub fn new(
        encoder: &'p Encoder<'v, 'tcx>,
        proc_def_id: DefId,
        mir: &'p mir::Body<'tcx>,
        is_encoding_assertion: bool,
    ) -> Self {
        trace!("PureFunctionEncoder constructor: {:?}", proc_def_id);
        let interpreter = PureFunctionBackwardInterpreter::new(
            encoder,
            mir,
            proc_def_id,
            is_encoding_assertion,
        );
        PureFunctionEncoder {
            encoder,
            proc_def_id,
            mir,
            interpreter,
        }
    }

    /// Used to encode expressions in assertions
    pub fn encode_body(&self) -> SpannedEncodingResult<vir::Expr> {
        let function_name = self.encoder.env().get_absolute_item_name(self.proc_def_id);
        debug!("Encode body of pure function {}", function_name);

        let state = run_backward_interpretation(self.mir, &self.interpreter)?
            .expect(&format!("Procedure {:?} contains a loop", self.proc_def_id));
        let body_expr = state.into_expressions().remove(0);
        debug!(
            "Pure function {} has been encoded with expr: {}",
            function_name, body_expr
        );
        let subst_strings = self.encoder.type_substitution_strings().with_span(self.mir.span)?;
        let patched_body_expr = body_expr.patch_types(&subst_strings);
        Ok(patched_body_expr)
    }

    pub fn encode_function(&self) -> SpannedEncodingResult<vir::Function> {
        let function_name = self.encode_function_name();
        debug!("Encode pure function {}", function_name);
        let mut state = run_backward_interpretation(self.mir, &self.interpreter)?
            .expect(&format!("Procedure {:?} contains a loop", self.proc_def_id));

        // Fix arguments
        for arg in self.mir.args_iter() {
            let arg_ty = self.interpreter.mir_encoder().get_local_ty(arg);
            let target_place = self.encoder.encode_value_expr(
                vir::Expr::local(
                    self.interpreter
                        .mir_encoder()
                        .encode_local(arg)?
                ),
                arg_ty
            );
            let new_place: vir::Expr = self.encode_local(arg)?.into();
            state.substitute_place(&target_place, new_place);
        }

        let body_expr = state.into_expressions().remove(0);
        debug!(
            "Pure function {} has been encoded with expr: {}",
            function_name, body_expr
        );

        // if the function returns a snapshot, we take a snapshot of the body
        if self.encode_function_return_type()?.is_domain() {
            let ty = self.encoder.resolve_typaram(self.mir.return_ty());
            let return_span = self.get_local_span(mir::RETURN_PLACE);

            if !self.encoder.env().type_is_copy(ty) {
                return Err(SpannedEncodingError::unsupported(
                    "return type of pure function does not implement Copy",
                    return_span,
                ));
            }

            let snapshot = self.encoder.encode_snapshot(&ty)
                .with_span(return_span)?;
            let body_expr = snapshot.snap_call(body_expr);
            self.encode_function_given_body(Some(body_expr))
        } else {
            self.encode_function_given_body(Some(body_expr))
        }
    }

    pub fn encode_bodyless_function(&self)
        -> SpannedEncodingResult<vir::Function>
    {
        let function_name = self.encode_function_name();
        debug!("Encode trusted (bodyless) pure function {}", function_name);

        self.encode_function_given_body(None)
    }

    pub fn encode_predicate_function(&self, predicate_body: &typed::Assertion<'tcx>)
        -> SpannedEncodingResult<vir::Function>
    {
        let function_name = self.encode_function_name();
        debug!("Encode predicate function {}", function_name);

        let mir_span = self.encoder.env().tcx().def_span(self.proc_def_id);
        let contract = self.encoder.get_procedure_contract_for_def(self.proc_def_id).with_span(mir_span)?;
        let encoded_args = contract
            .args
            .iter()
            .map(|local| self.encode_local(local.clone().into()).map(|l| l.into()))
            .collect::<Result<Vec<_>, _>>()?;

        let predicate_body_encoded = self.encoder.encode_assertion(
            predicate_body,
            self.mir,
            None,
            &encoded_args,
            None,
            true,
            None,
            ErrorCtxt::GenericExpression)?;

        self.encode_function_given_body(Some(predicate_body_encoded))
    }

    // Private

    fn encode_function_given_body(&self, body: Option<vir::Expr>)
        -> SpannedEncodingResult<vir::Function>
    {
        let function_name = self.encode_function_name();
        let is_bodyless = body.is_none();
        if is_bodyless {
            debug!("Encode pure function {} given body None", function_name);
        } else {
            debug!(
                "Encode pure function {} given body Some({})",
                function_name,
                body.as_ref().unwrap()
            );
        }

        let contract = self.encoder
            .get_procedure_contract_for_def(self.proc_def_id)
            .with_span(self.mir.span)?;
        let subst_strings = self.encoder.type_substitution_strings().with_span(self.mir.span)?;

        let (type_precondition, func_precondition) = self.encode_precondition_expr(&contract)?;
        let patched_type_precondition = type_precondition.patch_types(&subst_strings);

        let mut precondition = vec![patched_type_precondition, func_precondition];
        let mut postcondition = vec![self.encode_postcondition_expr(&contract)?];

        let mut formal_args = vec![];
        for local in self.mir.args_iter() {
            let mir_encoder = self.interpreter.mir_encoder();
            let var_name = mir_encoder.encode_local_var_name(local);
            let var_span = mir_encoder.get_local_span(local);
            let mir_type = mir_encoder.get_local_ty(local);
            let var_type = self
                .encoder
                .encode_value_or_ref_type(self.encoder.resolve_typaram(mir_type))
                .with_span(var_span)?;
            let var_type = var_type.patch(&subst_strings);
            formal_args.push(vir::LocalVar::new(var_name, var_type))
        };
        let return_type = self.encode_function_return_type()?;

        let res_value_range_pos = self.encoder.error_manager().register(
            self.mir.span,
            ErrorCtxt::PureFunctionPostconditionValueRangeOfResult,
        );
        let pure_fn_return_variable = vir::LocalVar::new("__result", return_type.clone());
        // Add value range of the arguments and return value to the pre/postconditions
        if config::check_overflows() {
            let return_bounds: Vec<_> = self
                .encoder
                .encode_type_bounds(
                    &vir::Expr::local(pure_fn_return_variable),
                    self.mir.return_ty(),
                )
                .into_iter()
                .map(|p| p.set_default_pos(res_value_range_pos))
                .collect();
            postcondition.extend(return_bounds);

            for (formal_arg, local) in formal_args.iter().zip(self.mir.args_iter()) {
                let typ = self.interpreter.mir_encoder().get_local_ty(local);
                let bounds = self
                    .encoder
                    .encode_type_bounds(&vir::Expr::local(formal_arg.clone()), &typ);
                precondition.extend(bounds);
            }
        } else if config::encode_unsigned_num_constraint() {
            if let ty::TyKind::Uint(_) = self.mir.return_ty().kind() {
                let expr = vir::Expr::le_cmp(0.into(), pure_fn_return_variable.into());
                postcondition.push(expr.set_default_pos(res_value_range_pos));
            }
            for (formal_arg, local) in formal_args.iter().zip(self.mir.args_iter()) {
                let typ = self.interpreter.mir_encoder().get_local_ty(local);
                if let ty::TyKind::Uint(_) = typ.kind() {
                    precondition.push(vir::Expr::le_cmp(0.into(), formal_arg.into()));
                }
            }
        }

        debug_assert!(
            !postcondition.iter().any(|p| p.pos().is_default()),
            "Some postcondition has no position: {:?}",
            postcondition
        );

        let postcondition = postcondition
            .into_iter()
            .map(|post|
                SnapshotSpecPatcher::new(self.encoder)
                    .patch_spec(post)
                    // TODO: use a better span
                    .with_span(self.mir.span)
            ).collect::<Result<_, _>>()?;

        let mut function = vir::Function {
            name: function_name.clone(),
            formal_args,
            return_type,
            pres: precondition,
            posts: postcondition,
            body,
        };

        self.encoder
            .log_vir_program_before_foldunfold(function.to_string());

        if config::simplify_encoding() {
            function = vir::optimizations::functions::Simplifier::simplify(function);
        }

        // Add folding/unfolding
        foldunfold::add_folding_unfolding_to_function(
            function,
            self.encoder.get_used_viper_predicates_map(),
        )
        .map_err(|foldunfold_error| {
            SpannedEncodingError::internal(
                format!(
                    "generating unfolding Viper expressions failed ({:?})",
                    foldunfold_error
                ),
                self.mir.span,
            )
        })
    }

    /// Encode the precondition with two expressions:
    /// - one for the type encoding
    /// - one for the functional specification.
    fn encode_precondition_expr(
        &self,
        contract: &ProcedureContract<'tcx>,
    ) -> SpannedEncodingResult<(vir::Expr, vir::Expr)> {
        let mut type_spec = vec![];
        for &local in contract.args.iter() {
            let local_ty = self.interpreter.mir_encoder().get_local_ty(local.into());
            let fraction = if let ty::TyKind::Ref(_, _, hir::Mutability::Not) = local_ty.kind() {
                vir::PermAmount::Read
            } else {
                vir::PermAmount::Write
            };
            let opt_pred_perm = self.interpreter.mir_encoder()
                .encode_place_predicate_permission(self.encode_local(local.into())?.into(), fraction);
            if let Some(spec) = opt_pred_perm {
                type_spec.push(spec)
            }
        };
        let mut func_spec: Vec<vir::Expr> = vec![];

        // Encode functional specification
        let encoded_args: Vec<vir::Expr> = contract
            .args
            .iter()
            .map(|local| self.encode_local(local.clone().into()).map(|l| l.into()))
            .collect::<Result<_, _>>()?;
        for item in contract.functional_precondition() {
            debug!("Encode spec item: {:?}", item);
            func_spec.push(self.encoder.encode_assertion(
                &item,
                &self.mir,
                None,
                &encoded_args,
                None,
                true,
                None,
                ErrorCtxt::GenericExpression,
            )?);
        }

        Ok((
            type_spec.into_iter().conjoin(),
            func_spec
                .into_iter()
                .map(|post|
                    SnapshotSpecPatcher::new(self.encoder)
                        .patch_spec(post)
                        // TODO: use a better span
                        .with_span(self.mir.span)
                )
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .conjoin(),
        ))
    }

    /// Encode the postcondition with one expression just for the functional specification (no
    /// type encoding).
    fn encode_postcondition_expr(&self, contract: &ProcedureContract<'tcx>)
        -> SpannedEncodingResult<vir::Expr>
    {
        let mut func_spec: Vec<vir::Expr> = vec![];

        // Encode functional specification
        let encoded_args: Vec<vir::Expr> = contract
            .args
            .iter()
            .map(|local| self.encode_local(local.clone().into()).map(|l| l.into()))
            .collect::<Result<_, _>>()?;
        let encoded_return = self.encode_local(contract.returned_value.clone().into())?;
        debug!("encoded_return: {:?}", encoded_return);

        for item in contract.functional_postcondition() {
            let encoded_postcond = self.encoder.encode_assertion(
                &item,
                &self.mir,
                None,
                &encoded_args,
                Some(&encoded_return.clone().into()),
                true,
                None,
                ErrorCtxt::GenericExpression,
            )?;
            debug_assert!(!encoded_postcond.pos().is_default());
            func_spec.push(encoded_postcond);
        }

        let post = func_spec.into_iter().conjoin();

        // TODO: use a better span
        let postcondition_pos = self
            .encoder
            .error_manager()
            .register(self.mir.span, ErrorCtxt::GenericExpression);

        // Fix return variable
        let pure_fn_return_variable =
            vir::LocalVar::new("__result", self.encode_function_return_type()?);

        let post = post.replace_place(&encoded_return.into(), &pure_fn_return_variable.into())
            .set_default_pos(postcondition_pos);

        // TODO: use a better span
        Ok(SnapshotSpecPatcher::new(self.encoder)
            .patch_spec(post)
            .with_span(self.mir.span)?
        )
    }

    fn encode_local(&self, local: mir::Local) -> SpannedEncodingResult<vir::LocalVar> {
        let mir_encoder = self.interpreter.mir_encoder();
        let var_name = mir_encoder.encode_local_var_name(local);
        let var_span = mir_encoder.get_local_span(local);
        let var_type = self.encoder
            .encode_value_or_ref_type(self.interpreter.mir_encoder().get_local_ty(local))
            .with_span(var_span)?;
        Ok(vir::LocalVar::new(var_name, var_type))
    }

    fn get_local_span(&self, local: mir::Local) -> Span {
        self.interpreter.mir_encoder().get_local_span(local)
    }

    pub fn encode_function_name(&self) -> String {
        self.encoder.encode_item_name(self.proc_def_id)
    }

    pub fn encode_function_return_type(&self) -> SpannedEncodingResult<vir::Type> {
        let ty = self.encoder.resolve_typaram(self.mir.return_ty());
        let return_span = self.get_local_span(mir::RETURN_PLACE);

        // Return an error for unsupported return types
        let tcx = self.encoder.env().tcx();
        if !is_supported_type_of_pure_expression(tcx, ty) {
            return Err(SpannedEncodingError::incorrect(
                "invalid return type of pure function",
                return_span,
            ));
        }

        let return_local = mir::Place::return_place().as_local().unwrap();
        let span = self.interpreter.mir_encoder().get_local_span(return_local);
        self.encoder.encode_value_type(ty).with_span(span)
    }
}

pub(super) struct PureFunctionBackwardInterpreter<'p, 'v: 'p, 'tcx: 'v> {
    encoder: &'p Encoder<'v, 'tcx>,
    mir: &'p mir::Body<'tcx>,
    mir_encoder: MirEncoder<'p, 'v, 'tcx>,
    /// True if the encoder is currently encoding an assertion and not a pure function body. This
    /// flag is used to distinguish when assert terminators should be translated into `false` and
    /// when to a undefined function calls. This distinction allows overflow checks to be checked
    /// on the caller side and assumed on the definition side.
    is_encoding_assertion: bool,
}

/// XXX: This encoding works backward, but there is the risk of generating expressions whose length
/// is exponential in the number of branches. If this becomes a problem, consider doing a forward
/// encoding (keeping path conditions expressions).
impl<'p, 'v: 'p, 'tcx: 'v> PureFunctionBackwardInterpreter<'p, 'v, 'tcx> {
    pub(super) fn new(
        encoder: &'p Encoder<'v, 'tcx>,
        mir: &'p mir::Body<'tcx>,
        def_id: DefId,
        is_encoding_assertion: bool,
    ) -> Self {
        PureFunctionBackwardInterpreter {
            encoder,
            mir,
            mir_encoder: MirEncoder::new(encoder, mir, def_id),
            is_encoding_assertion,
        }
    }

    pub(super) fn mir_encoder(&self) -> &MirEncoder<'p, 'v, 'tcx> {
        &self.mir_encoder
    }
}

impl<'p, 'v: 'p, 'tcx: 'v> BackwardMirInterpreter<'tcx>
    for PureFunctionBackwardInterpreter<'p, 'v, 'tcx>
{
    type State = MultiExprBackwardInterpreterState;
    type Error = SpannedEncodingError;

    fn apply_terminator(
        &self,
        _bb: mir::BasicBlock,
        term: &mir::Terminator<'tcx>,
        states: HashMap<mir::BasicBlock, &Self::State>,
    ) -> Result<Self::State, Self::Error> {
        trace!("apply_terminator {:?}, states: {:?}", term, states);
        use rustc_middle::mir::TerminatorKind;
        let span = term.source_info.span;

        // Generate a function call that leaves the expression undefined.
        let unreachable_expr = |pos| {
            self.encoder.encode_value_or_ref_type(self.mir.return_ty()).map(|encoded_type| {
                let function_name =
                    self.encoder
                        .encode_builtin_function_use(BuiltinFunctionKind::Unreachable(
                            encoded_type.clone(),
                        ));
                vir::Expr::func_app(function_name, vec![], vec![], encoded_type, pos)
            })
        };

        // Generate a function call that leaves the expression undefined.
        let undef_expr = |pos| {
            self.encoder.encode_value_or_ref_type(self.mir.return_ty()).map(|encoded_type| {
                let function_name = self
                    .encoder
                    .encode_builtin_function_use(BuiltinFunctionKind::Undefined(encoded_type.clone()));
                vir::Expr::func_app(function_name, vec![], vec![], encoded_type, pos)
            })
        };

        Ok(match term.kind {
            TerminatorKind::Unreachable => {
                assert!(states.is_empty());
                let pos = self
                    .encoder
                    .error_manager()
                    .register(term.source_info.span, ErrorCtxt::Unexpected);
                MultiExprBackwardInterpreterState::new_single(
                    undef_expr(pos).with_span(term.source_info.span)?
                )
            }

            TerminatorKind::Abort | TerminatorKind::Resume { .. } => {
                assert!(states.is_empty());
                let pos = self
                    .encoder
                    .error_manager()
                    .register(term.source_info.span, ErrorCtxt::Unexpected);
                MultiExprBackwardInterpreterState::new_single(
                    unreachable_expr(pos).with_span(term.source_info.span)?
                )
            }

            TerminatorKind::Drop { ref target, .. } => {
                assert!(1 <= states.len() && states.len() <= 2);
                states[target].clone()
            }

            TerminatorKind::Goto { ref target } => {
                assert_eq!(states.len(), 1);
                states[target].clone()
            }

            TerminatorKind::FalseEdge {
                ref real_target, ..
            } => {
                assert_eq!(states.len(), 2);
                states[real_target].clone()
            }

            TerminatorKind::FalseUnwind {
                ref real_target, ..
            } => {
                assert_eq!(states.len(), 1);
                states[real_target].clone()
            }

            TerminatorKind::Return => {
                assert!(states.is_empty());
                trace!("Return type: {:?}", self.mir.return_ty());
                let return_type = self.encoder.encode_type(self.mir.return_ty()).with_span(span)?;
                let return_var = vir::LocalVar::new("_0", return_type);
                MultiExprBackwardInterpreterState::new_single(
                    self.encoder.encode_value_expr(
                        vir::Expr::local(return_var.into()),
                        self.mir.return_ty()
                    )
                )
            }

            TerminatorKind::SwitchInt {
                switch_ty,
                ref discr,
                ref targets,
            } => {
                trace!(
                    "SwitchInt ty '{:?}', discr '{:?}', targets '{:?}'",
                    switch_ty,
                    discr,
                    targets
                );
                let mut cfg_targets: Vec<(vir::Expr, mir::BasicBlock)> = vec![];
                let discr_val = self.mir_encoder.encode_operand_expr(discr)
                    .with_span(span)?;
                for (value, target) in targets.iter() {
                    // Convert int to bool, if required
                    let viper_guard = match switch_ty.kind() {
                        ty::TyKind::Bool => {
                            if value == 0 {
                                // If discr is 0 (false)
                                vir::Expr::not(discr_val.clone().into())
                            } else {
                                // If discr is not 0 (true)
                                discr_val.clone().into()
                            }
                        }

                        ty::TyKind::Int(_) | ty::TyKind::Uint(_) => {
                            vir::Expr::eq_cmp(
                                discr_val.clone().into(),
                                self.encoder.encode_int_cast(value, switch_ty),
                            )
                        }

                        ref x => unreachable!("{:?}", x),
                    };
                    cfg_targets.push((viper_guard, target))
                }
                let default_target = targets.otherwise();

                let default_target_terminator = self.mir.basic_blocks()[default_target]
                    .terminator
                    .as_ref()
                    .unwrap();
                trace!("default_target_terminator: {:?}", default_target_terminator);
                let default_is_unreachable = match default_target_terminator.kind {
                    TerminatorKind::Unreachable => true,
                    _ => false,
                };

                trace!("cfg_targets: {:?}", cfg_targets);

                let refined_default_target = if default_is_unreachable && !cfg_targets.is_empty() {
                    // Here we can assume that the `cfg_targets` are exhausive, and that
                    // `default_target` is unreachable
                    trace!("The default target is unreachable");
                    cfg_targets.pop().unwrap().1
                } else {
                    default_target
                };

                trace!("cfg_targets: {:?}", cfg_targets);

                MultiExprBackwardInterpreterState::new(
                    (0..states[&refined_default_target].exprs().len())
                        .map(|expr_index| {
                            cfg_targets.iter().fold(
                                states[&refined_default_target].exprs()[expr_index].clone(),
                                |else_expr, (guard, target)| {
                                    let then_expr = states[&target].exprs()[expr_index].clone();
                                    if then_expr == else_expr {
                                        // Optimization
                                        else_expr
                                    } else {
                                        vir::Expr::ite(guard.clone(), then_expr, else_expr)
                                    }
                                },
                            )
                        })
                        .collect(),
                )
            }

            TerminatorKind::DropAndReplace { .. } => unimplemented!(),

            TerminatorKind::Call {
                ref args,
                ref destination,
                func:
                    mir::Operand::Constant(box mir::Constant {
                        literal: mir::ConstantKind::Ty(
                            ty::Const {
                                ty,
                                val: _,
                            },
                        ),
                        ..
                    }),
                ..
            } => {
                if let ty::TyKind::FnDef(def_id, substs) = ty.kind() {
                    let def_id = *def_id;
                    let full_func_proc_name: &str =
                        &self.encoder.env().tcx().def_path_str(def_id);
                        // &self.encoder.env().tcx().absolute_item_path_str(def_id);
                    let func_proc_name = &self.encoder.env().get_item_name(def_id);

                    let own_substs =
                        ty::List::identity_for_item(self.encoder.env().tcx(), def_id);

                    {
                        // FIXME: this is a hack to support generics. See issue #187.
                        let mut tymap_stack = self.encoder.typaram_repl.borrow_mut();
                        let mut tymap = HashMap::new();

                        for (kind1, kind2) in own_substs.iter().zip(substs.iter()) {
                            if let (
                                ty::subst::GenericArgKind::Type(ty1),
                                ty::subst::GenericArgKind::Type(ty2),
                            ) = (kind1.unpack(), kind2.unpack())
                            {
                                tymap.insert(ty1, ty2);
                            }
                        }
                        tymap_stack.push(tymap);
                    }
                    let cleanup = || {
                        // FIXME: this is a hack to support generics. See issue #187.
                        let mut tymap_stack = self.encoder.typaram_repl.borrow_mut();
                        tymap_stack.pop();
                    };

                    let state = if destination.is_some() {
                        let (ref lhs_place, target_block) = destination.as_ref().unwrap();
                        let (encoded_lhs, ty, _) = self.mir_encoder.encode_place(lhs_place)
                            .with_span(span)
                            .run_if_err(cleanup)?;
                        let lhs_value = self.encoder.encode_value_expr(encoded_lhs.clone(), ty);
                        let encoded_args: Vec<vir::Expr> = args
                            .iter()
                            .map(|arg| self.mir_encoder.encode_operand_expr(arg))
                            .collect::<Result<_, _>>()
                            .with_span(span)?;

                        match full_func_proc_name {
                            "prusti_contracts::old" => {
                                trace!("Encoding old expression {:?}", args[0]);
                                assert_eq!(args.len(), 1);

                                // Return an error for unsupported old(..) types
                                let tcx = self.encoder.env().tcx();
                                if !is_supported_type_of_pure_expression(tcx, ty) {
                                    cleanup();
                                    return Err(SpannedEncodingError::incorrect(
                                        "the type of the old expression is invalid",
                                        term.source_info.span,
                                    ));
                                }

                                let encoded_rhs = self
                                    .mir_encoder
                                    .encode_old_expr(encoded_args[0].clone(), PRECONDITION_LABEL);
                                let mut state = states[&target_block].clone();
                                state.substitute_value(&lhs_value, encoded_rhs);
                                state
                            }

                            "prusti_contracts::before_expiry" => {
                                trace!("Encoding before_expiry expression {:?}", args[0]);
                                assert_eq!(args.len(), 1);
                                let encoded_rhs = self
                                    .mir_encoder
                                    .encode_old_expr(encoded_args[0].clone(), WAND_LHS_LABEL);
                                let mut state = states[&target_block].clone();
                                state.substitute_value(&lhs_value, encoded_rhs);
                                state
                            }
                            // simple function call
                            _ => {
                                let mut is_cmp_call = false;
                                let is_pure_function = self.encoder.is_pure(def_id);
                                let (function_name, return_type) = if is_pure_function {
                                    self.encoder.encode_pure_function_use(def_id)
                                        .with_span(term.source_info.span)
                                        .run_if_err(cleanup)?
                                } else {
                                    // this is an ugly hack as self.env.get_procedure crashes in a compiler-internal
                                    // function
                                    if args.len() == 2 {
                                        let arg_ty = self.mir_encoder.get_operand_ty(&args[0]);
                                        match self.encoder.get_item_name(def_id).as_str() {
                                            "std::cmp::PartialEq::eq"
                                            if self.encoder.has_structural_eq_impl(arg_ty) => {
                                                is_cmp_call = true;
                                                self.encoder.encode_cmp_pure_function_use(def_id, arg_ty, true)
                                                    .run_if_err(cleanup)?
                                            }
                                            "std::cmp::PartialEq::ne"
                                            if self.encoder.has_structural_eq_impl(arg_ty) => {
                                                is_cmp_call = true;
                                                self.encoder.encode_cmp_pure_function_use(def_id, arg_ty, false)
                                                    .run_if_err(cleanup)?
                                            }
                                            _ => {
                                                // TODO: interestingly, this crashes for
                                                // custom implementations of eq as the def_id
                                                // is not local; for details, see
                                                // https://github.com/viperproject/prusti-dev/issues/188.
                                                self.encoder.encode_stub_pure_function_use(def_id)
                                                    .run_if_err(cleanup)?
                                            }
                                        }
                                    } else {
                                        self.encoder.encode_stub_pure_function_use(def_id)
                                            .run_if_err(cleanup)?
                                    }
                                };
                                if !is_pure_function && !is_cmp_call {
                                    cleanup();
                                    return Err(SpannedEncodingError::incorrect(
                                        format!(
                                            "use of impure function {:?} in pure code is not allowed",
                                            func_proc_name
                                        ),
                                        term.source_info.span,
                                    ));
                                }
                                trace!("Encoding pure function call '{}'", function_name);

                                let formal_args: Vec<vir::LocalVar> = args
                                    .iter()
                                    .enumerate()
                                    .map(|(i, arg)| {
                                        self.mir_encoder.encode_operand_expr_type(arg)
                                            .map(|ty| vir::LocalVar::new(format!("x{}", i), ty))
                                    })
                                    .collect::<Result<_, _>>()
                                    .with_span(term.source_info.span)
                                    .run_if_err(cleanup)?;

                                let pos = self
                                    .encoder
                                    .error_manager()
                                    .register(term.source_info.span, ErrorCtxt::PureFunctionCall);
                                let mut encoded_rhs = vir::Expr::func_app(
                                    function_name,
                                    encoded_args,
                                    formal_args,
                                    return_type,
                                    pos,
                                );
                                if config::enable_purification_optimization() {
                                    if let Some(proc_id) = self.encoder.current_proc.borrow().clone() {
                                        if !self.encoder.is_pure(proc_id) {
                                            let snapshots = self.encoder.get_snapshots();
                                            let mut ap = snapshot::AssertPurifier::new(&snapshots, snapshot::two_nat());
                                            encoded_rhs = vir::ExprFolder::fold(&mut ap, encoded_rhs.clone());
                                        }
                                    }
                                }
                                let mut state = states[&target_block].clone();
                                state.substitute_value(&lhs_value, encoded_rhs);
                                state
                            }
                        }
                    } else {
                        // FIXME: Refactor the common code with the procedure encoder.

                        // Encoding of a non-terminating function call
                        let error_ctxt = match full_func_proc_name {
                            "std::rt::begin_panic"
                            | "core::panicking::panic"
                            | "core::panicking::panic_fmt" => {
                                // This is called when a Rust assertion fails
                                // args[0]: message
                                // args[1]: position of failing assertions

                                let panic_cause = self.mir_encoder.encode_panic_cause(
                                    term.source_info
                                );
                                ErrorCtxt::PanicInPureFunction(panic_cause)
                            }

                            _ => ErrorCtxt::DivergingCallInPureFunction,
                        };
                        let pos = self
                            .encoder
                            .error_manager()
                            .register(term.source_info.span, error_ctxt);
                        MultiExprBackwardInterpreterState::new_single(
                            unreachable_expr(pos).with_span(term.source_info.span)
                                .run_if_err(cleanup)?
                        )
                    };

                    cleanup();
                    state
                } else {
                    // Other kind of calls?
                    unimplemented!();
                }
            }

            TerminatorKind::Call { .. } => {
                // Other kind of calls?
                unimplemented!();
            }

            TerminatorKind::Assert {
                ref cond,
                expected,
                ref target,
                ref msg,
                ..
            } => {
                let cond_val = self.mir_encoder.encode_operand_expr(cond)
                    .with_span(span)?;
                let viper_guard = if expected {
                    cond_val
                } else {
                    vir::Expr::not(cond_val)
                };

                let pos = self.encoder.error_manager().register(
                    term.source_info.span,
                    ErrorCtxt::PureFunctionAssertTerminator(msg.description().to_string()),
                );

                MultiExprBackwardInterpreterState::new(
                    states[target]
                        .exprs()
                        .iter()
                        .map(|expr| {
                            let failure_result = if self.is_encoding_assertion {
                                // We are encoding an assertion, so all failures should be
                                // equivalent to false.
                                Ok(false.into())
                            } else {
                                // We are encoding a pure function, so all failures should
                                // be unreachable.
                                unreachable_expr(pos).with_span(term.source_info.span)
                            };
                            failure_result.map(
                                |result| vir::Expr::ite(viper_guard.clone(), expr.clone(), result)
                            )
                        })
                        .collect::<Result<_, _>>()?,
                )
            }

            TerminatorKind::Yield { .. } |
            TerminatorKind::GeneratorDrop |
            TerminatorKind::InlineAsm { .. } => {
                unimplemented!("{:?}", term.kind)
            }
        })
    }

    fn apply_statement(
        &self,
        _bb: mir::BasicBlock,
        _stmt_index: usize,
        stmt: &mir::Statement<'tcx>,
        state: &mut Self::State,
    ) -> Result<(), Self::Error> {
        trace!("apply_statement {:?}, state: {}", stmt, state);
        let span = stmt.source_info.span;

        match stmt.kind {
            mir::StatementKind::StorageLive(..)
            | mir::StatementKind::StorageDead(..)
            | mir::StatementKind::FakeRead(_, _)    // FIXME
            // | mir::StatementKind::ReadForMatch(..)
            // | mir::StatementKind::EndRegion(..)
             => {
                // Nothing to do
            }

            mir::StatementKind::Assign(box (ref lhs, ref rhs)) => {
                let (encoded_lhs, ty, _) = self.mir_encoder.encode_place(lhs).unwrap();

                if !state.use_place(&encoded_lhs) {
                    // If the lhs is not mentioned in our state, do nothing
                    trace!("The state does not mention {:?}", encoded_lhs);
                    return Ok(());
                }

                let opt_lhs_value_place = match ty.kind() {
                    ty::TyKind::Bool
                    | ty::TyKind::Int(..)
                    | ty::TyKind::Uint(..)
                    | ty::TyKind::RawPtr(..)
                    | ty::TyKind::Ref(..) => Some(
                        self.encoder.encode_value_expr(
                            encoded_lhs.clone(),
                            ty
                        )
                        // encoded_lhs
                        //     .clone()
                        //     .field(self.encoder.encode_value_field(ty)),
                    ),
                    _ => None,
                };

                match rhs {
                    &mir::Rvalue::Use(ref operand) => {
                        let opt_encoded_rhs = self.mir_encoder.encode_operand_place(operand)
                            .with_span(span)?;

                        match opt_encoded_rhs {
                            Some(encoded_rhs) => {
                                // Substitute a place
                                state.substitute_place(&encoded_lhs, encoded_rhs);
                            }
                            None => {
                                // Substitute a place of a value with an expression
                                if let Some(lhs_value_place) = &opt_lhs_value_place {
                                    // opt_lhs_value_place can be none in trigger generation code.
                                    let rhs_expr = self.mir_encoder
                                        .encode_operand_expr(operand)
                                        .with_span(span)?;
                                    state.substitute_value(lhs_value_place, rhs_expr);
                                }
                            }
                        }
                    }

                    &mir::Rvalue::Aggregate(ref aggregate, ref operands) => {
                        debug!("Encode aggregate {:?}, {:?}", aggregate, operands);
                        match aggregate.as_ref() {
                            &mir::AggregateKind::Tuple => {
                                let field_types = if let ty::TyKind::Tuple(ref x) = ty.kind() {
                                    x
                                } else {
                                    unreachable!()
                                };
                                for (field_num, operand) in operands.iter().enumerate() {
                                    let field_name = format!("tuple_{}", field_num);
                                    let field_ty = field_types[field_num];
                                    let encoded_field = self.encoder
                                        .encode_raw_ref_field(field_name, field_ty.expect_ty())
                                        .with_span(span)?;
                                    let field_place = encoded_lhs.clone().field(encoded_field);

                                    let encoded_operand = self.mir_encoder.encode_operand_place(operand)
                                        .with_span(span)?;
                                    match encoded_operand {
                                        Some(encoded_rhs) => {
                                            // Substitute a place
                                            state.substitute_place(&field_place, encoded_rhs);
                                        }
                                        None => {
                                            // Substitute a place of a value with an expression
                                            let rhs_expr =
                                                self.mir_encoder.encode_operand_expr(operand)
                                                    .with_span(span)?;
                                            state.substitute_value(
                                                &self.encoder.encode_value_expr(field_place, field_ty.expect_ty()),
                                                rhs_expr,
                                            );
                                        }
                                    }
                                }
                            }

                            &mir::AggregateKind::Adt(adt_def, variant_index, subst, _, _) => {
                                let num_variants = adt_def.variants.len();
                                let variant_def = &adt_def.variants[variant_index];
                                let mut encoded_lhs_variant = encoded_lhs.clone();
                                if num_variants != 1 {
                                    let discr_field = self.encoder.encode_discriminant_field();
                                    state.substitute_value(
                                        &encoded_lhs.clone().field(discr_field),
                                        variant_index.index().into(),
                                    );
                                    encoded_lhs_variant =
                                        encoded_lhs_variant.variant(&variant_def.ident.as_str());
                                }
                                for (field_index, field) in variant_def.fields.iter().enumerate() {
                                    let operand = &operands[field_index];
                                    let field_name = &field.ident.as_str();
                                    let tcx = self.encoder.env().tcx();
                                    let field_ty = field.ty(tcx, subst);
                                    let encoded_field = self.encoder
                                            .encode_struct_field(field_name, field_ty)
                                            .with_span(span)?;

                                    let field_place =
                                        encoded_lhs_variant.clone().field(encoded_field);

                                    let encoded_operand = self.mir_encoder.encode_operand_place(operand)
                                        .with_span(span)?;
                                    match encoded_operand {
                                        Some(encoded_rhs) => {
                                            // Substitute a place
                                            state.substitute_place(&field_place, encoded_rhs);
                                        }
                                        None => {
                                            // Substitute a place of a value with an expression
                                            let rhs_expr =
                                                self.mir_encoder.encode_operand_expr(operand)
                                                    .with_span(span)?;
                                            state.substitute_value(
                                                &self.encoder.encode_value_expr(field_place, field_ty),
                                                rhs_expr,
                                            );
                                        }
                                    }
                                }
                            }

                            ref x => unimplemented!("{:?}", x),
                        }
                    }

                    &mir::Rvalue::BinaryOp(op, box(ref left, ref right)) => {
                        let encoded_left = self.mir_encoder.encode_operand_expr(left)
                            .with_span(span)?;
                        let encoded_right = self.mir_encoder.encode_operand_expr(right)
                            .with_span(span)?;
                        let encoded_value = self.mir_encoder.encode_bin_op_expr(
                            op,
                            encoded_left,
                            encoded_right,
                            ty,
                        ).with_span(span)?;

                        // Substitute a place of a value with an expression
                        state.substitute_value(&opt_lhs_value_place.unwrap(), encoded_value);
                    }

                    &mir::Rvalue::CheckedBinaryOp(op, box (ref left, ref right)) => {
                        let operand_ty = if let ty::TyKind::Tuple(ref types) = ty.kind() {
                            types[0].clone()
                        } else {
                            unreachable!()
                        };

                        let encoded_left = self.mir_encoder.encode_operand_expr(left)
                            .with_span(span)?;
                        let encoded_right = self.mir_encoder.encode_operand_expr(right)
                            .with_span(span)?;

                        let encoded_value = self.mir_encoder.encode_bin_op_expr(
                            op,
                            encoded_left.clone(),
                            encoded_right.clone(),
                            operand_ty.expect_ty(),
                        ).with_span(span)?;
                        let encoded_check = self.mir_encoder.encode_bin_op_check(
                            op,
                            encoded_left,
                            encoded_right,
                            operand_ty.expect_ty(),
                        ).with_span(span)?;

                        let field_types = if let ty::TyKind::Tuple(ref x) = ty.kind() {
                            x
                        } else {
                            unreachable!()
                        };
                        let value_field = self.encoder
                            .encode_raw_ref_field("tuple_0".to_string(), field_types[0].expect_ty())
                            .with_span(span)?;
                        let value_field_value = self.encoder
                            .encode_value_field(field_types[0].expect_ty());
                        let check_field = self.encoder
                            .encode_raw_ref_field("tuple_1".to_string(), field_types[1].expect_ty())
                            .with_span(span)?;
                        let check_field_value = self.encoder
                            .encode_value_field(field_types[1].expect_ty());

                        let lhs_value = encoded_lhs
                            .clone()
                            .field(value_field)
                            .field(value_field_value);
                        let lhs_check = encoded_lhs
                            .clone()
                            .field(check_field)
                            .field(check_field_value);

                        // Substitute a place of a value with an expression
                        state.substitute_value(&lhs_value, encoded_value);
                        state.substitute_value(&lhs_check, encoded_check);
                    }

                    &mir::Rvalue::UnaryOp(op, ref operand) => {
                        let encoded_val = self.mir_encoder.encode_operand_expr(operand)
                            .with_span(span)?;
                        let encoded_value = self.mir_encoder.encode_unary_op_expr(op, encoded_val);

                        // Substitute a place of a value with an expression
                        state.substitute_value(&opt_lhs_value_place.unwrap(), encoded_value);
                    }

                    &mir::Rvalue::NullaryOp(_op, ref _op_ty) => unimplemented!(),

                    &mir::Rvalue::Discriminant(ref src) => {
                        let (encoded_src, src_ty, _) = self.mir_encoder.encode_place(src).unwrap();
                        match src_ty.kind() {
                            ty::TyKind::Adt(ref adt_def, _) if !adt_def.is_box() => {
                                let num_variants = adt_def.variants.len();

                                let discr_value: vir::Expr = if num_variants == 0 {
                                    let pos = self
                                        .encoder
                                        .error_manager()
                                        .register(stmt.source_info.span, ErrorCtxt::Unexpected);
                                    let function_name = self.encoder.encode_builtin_function_use(
                                        BuiltinFunctionKind::Unreachable(vir::Type::Int),
                                    );
                                    vir::Expr::func_app(
                                        function_name,
                                        vec![],
                                        vec![],
                                        vir::Type::Int,
                                        pos,
                                    )
                                } else {
                                    if num_variants == 1 {
                                        0.into()
                                    } else {
                                        let discr_field = self.encoder.encode_discriminant_field();
                                        encoded_src.field(discr_field).into()
                                    }
                                };

                                // Substitute a place of a value with an expression
                                state.substitute_value(&opt_lhs_value_place.unwrap(), discr_value);
                            }
                            ref x => {
                                panic!("The discriminant of type {:?} is not defined", x);
                            }
                        }
                    }

                    &mir::Rvalue::Ref(_, mir::BorrowKind::Unique, ref place)
                    | &mir::Rvalue::Ref(_, mir::BorrowKind::Mut { .. }, ref place)
                    | &mir::Rvalue::Ref(_, mir::BorrowKind::Shared, ref place) => {
                        // will panic if attempting to encode unsupported type
                        let encoded_place = self.mir_encoder.encode_place(place).unwrap().0;
                        let encoded_ref = match encoded_place {
                            vir::Expr::Field(
                                box ref base,
                                vir::Field { ref name, .. },
                                ref _pos,
                            ) if name == "val_ref" => {
                                // Simplify "address of reference"
                                base.clone()
                            }
                            other_place => other_place.addr_of(),
                        };

                        // Substitute the place
                        state.substitute_place(&encoded_lhs, encoded_ref);
                    }

                    &mir::Rvalue::Cast(mir::CastKind::Misc, ref operand, dst_ty) => {
                        let encoded_val = self.mir_encoder
                            .encode_cast_expr(operand, dst_ty, stmt.source_info.span)?;

                        // Substitute a place of a value with an expression
                        state.substitute_value(&opt_lhs_value_place.unwrap(), encoded_val);
                    }

                    ref rhs => {
                        unimplemented!("encoding of '{:?}'", rhs);
                    }
                }
            }

            ref stmt => unimplemented!("encoding of '{:?}'", stmt),
        }

        Ok(())
    }
}

fn is_supported_type_of_pure_expression<'tcx>(tcx: ty::TyCtxt<'tcx>, ty: ty::Ty<'tcx>) -> bool {
    // Since we don't support box, references and raw pointers this will not recurse forever.
    match ty.kind() {
        ty::TyKind::Bool
        | ty::TyKind::Int(_)
        | ty::TyKind::Uint(_)
        | ty::TyKind::Char => true,

        ty::TyKind::Tuple(elems) => {
            elems.types().all(|t| is_supported_type_of_pure_expression(tcx, t))
        }

        ty::TyKind::Adt(adt_def, subst) if !adt_def.is_box() => {
            adt_def.all_fields()
                    .map(|field| field.ty(tcx, subst))
                    .all(|t| is_supported_type_of_pure_expression(tcx, t))
        }

        _ => false,
    }
}
