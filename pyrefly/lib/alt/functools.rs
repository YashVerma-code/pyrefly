/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Type-checks `functools.partial` and `partialmethod` bound arguments and remaining signatures.

use std::borrow::Cow;

use itertools::Itertools;
use pyrefly_python::ast::Ast;
use pyrefly_python::dunder;
use pyrefly_types::class::ClassType;
use pyrefly_types::known_class::KnownClass;
use pyrefly_types::quantified::Quantified;
use pyrefly_types::quantified::QuantifiedKind;
use pyrefly_types::types::Var;
use ruff_python_ast::Expr;
use ruff_python_ast::name::Name;
use ruff_text_size::Ranged;
use ruff_text_size::TextRange;
use starlark_map::small_map::SmallMap;
use starlark_map::small_set::SmallSet;
use vec1::Vec1;

use crate::alt::answers::LookupAnswer;
use crate::alt::answers_solver::AnswersSolver;
use crate::alt::call::CallTargetLookup;
use crate::alt::callable::CallArg;
use crate::alt::callable::CallKeyword;
use crate::alt::callable::CallValue;
use crate::alt::callable::CallWithTypes;
use crate::alt::class::class_field::ClassAttribute;
use crate::alt::expr::TypeOrExpr;
use crate::alt::unwrap::HintRef;
use crate::config::error_kind::ErrorKind;
use crate::error::collector::ErrorCollector;
use crate::types::callable::Callable;
use crate::types::callable::Param;
use crate::types::callable::ParamList;
use crate::types::callable::Params;
use crate::types::callable::PrefixParam;
use crate::types::callable::Required;
use crate::types::function::FuncMetadata;
use crate::types::function::Function;
use crate::types::types::BoundMethodType;
use crate::types::types::Overload;
use crate::types::types::OverloadType;
use crate::types::types::Type;

impl<'ctx, 'answer, Ans: LookupAnswer> AnswersSolver<'ctx, 'answer, Ans> {
    /// Handle partial application, retaining a nominal descriptor for `partialmethod`.
    pub fn call_functools_partial(
        &self,
        partial_ty: &Type,
        args: &[CallArg],
        kws: &[CallKeyword],
        callee_range: TextRange,
        arg_range: TextRange,
        hint: Option<HintRef>,
        errors: &ErrorCollector,
    ) -> Type {
        let Type::ClassDef(partial_cls) = partial_ty else {
            unreachable!("call_functools_partial dispatched on a non-class callee");
        };
        let is_partialmethod = partial_cls.is(KnownClass::FunctoolsPartialMethod);
        // Hoist hint-independent inference; contextual arguments remain expressions.
        let call = CallWithTypes::new();
        let supports_placeholders = self.bindings().sys_info().version().at_least(3, 14);
        let mut prepared_args = Cow::Borrowed(args);
        if supports_placeholders {
            for (i, arg) in args.iter().enumerate().skip(1) {
                if let CallArg::Arg(value) = arg
                    && can_prepare_partial_arg(*value)
                {
                    prepared_args.to_mut()[i] = call.call_arg(arg, self, errors);
                }
            }
        }
        let args = prepared_args.as_ref();
        let any_explicit = self.heap.mk_any_explicit();
        let mut kws_for_check = Vec::with_capacity(kws.len());
        for kw in kws {
            let kw = if supports_placeholders && can_prepare_partial_arg(kw.value) {
                call.call_keyword(kw, self, errors)
            } else {
                kw.clone()
            };
            if is_placeholder_arg(self, &kw.value) {
                self.error(
                    errors,
                    kw.range(),
                    ErrorKind::BadKeywordArgument,
                    "cannot use functools.Placeholder as keyword argument".to_owned(),
                );
                kws_for_check.push(CallKeyword {
                    range: kw.range,
                    arg: kw.arg,
                    value: CallValue::Plain(TypeOrExpr::Type(&any_explicit, kw.value.range())),
                });
            } else {
                kws_for_check.push(kw);
            }
        }
        let Some(CallArg::Arg(target)) = args.first() else {
            // No inspectable callable target (e.g. a `*args` splat); defer to the stub.
            return self
                .freeform_call_infer(
                    partial_ty.clone(),
                    args,
                    &kws_for_check,
                    callee_range,
                    arg_range,
                    hint,
                    errors,
                )
                .ty;
        };
        let target_ty = match target.infer(self, errors) {
            Type::TypeAlias(alias) => self.get_type_alias(&alias).as_value(self.stdlib),
            ty => ty,
        };
        let infer_stub = |func: &Type| {
            let mut args_with_ty = args.to_vec();
            args_with_ty[0] = CallArg::ty(func, target.range());
            self.freeform_call_infer(
                partial_ty.clone(),
                &args_with_ty,
                &kws_for_check,
                callee_range,
                arg_range,
                hint,
                errors,
            )
            .ty
        };
        let target_ty = if is_partialmethod {
            let signature = match &target_ty {
                Type::ClassType(cls) => cls.partialmethod_signature(),
                Type::Function(_) | Type::Forall(_) | Type::Overload(_) | Type::Callable(_) => {
                    Some(&target_ty)
                }
                _ => None,
            };
            let Some(signature) = signature.filter(|ty| {
                !args.iter().any(|arg| matches!(arg, CallArg::Star(..)))
                    && kws.iter().all(|kw| kw.arg.is_some())
                    && !ty.toplevel_func_metadata().is_some_and(|metadata| {
                        metadata.flags.is_staticmethod || metadata.flags.is_classmethod
                    })
                    && ty.toplevel_callable_signatures().next().is_some()
                    && ty.toplevel_callable_signatures().all(|(sig, tparams)| {
                        tparams.as_ref().is_none_or(|tparams| {
                            let mut receiver_quantifieds = SmallSet::new();
                            if let Some(receiver) = sig.get_first_param() {
                                receiver.collect_quantifieds(&mut receiver_quantifieds);
                            }
                            tparams.iter().all(|q| q.kind() == QuantifiedKind::TypeVar
                                && !receiver_quantifieds.contains(q))
                        }) && matches!(&sig.params, Params::List(params)
                            if matches!(params.items().first(), Some(Param::PosOnly(..) | Param::Pos(..))))
                    })
            }) else {
                return infer_stub(&target_ty);
            };
            signature.clone()
        } else {
            target_ty
        };
        // A class object / `type[C]` is callable via its constructor; normalize to that signature
        // so the same argument checking and partial construction apply, with the instance as the return.
        let target_ty = match target_ty {
            // A class name refers to the exact class, while a `type[C]` value may hold a
            // concrete subclass. Mirror the direct-instantiation checks in `call.rs`.
            ty @ (Type::ClassDef(_) | Type::SpecializedClassDef(_)) => {
                let (cls, instance) = match &ty {
                    Type::ClassDef(cls) => (cls, self.promote_silently(cls)),
                    Type::SpecializedClassDef(cls) => {
                        (cls.class_object(), Type::ClassType(cls.clone()))
                    }
                    _ => unreachable!("the target is a bare or specialized class definition"),
                };
                let metadata = self.get_metadata_for_class(cls);
                if metadata.is_protocol() {
                    self.error(
                        errors,
                        callee_range,
                        ErrorKind::BadInstantiation,
                        format!(
                            "Cannot instantiate `{}` because it is a protocol",
                            cls.name(self)
                        ),
                    );
                } else if !metadata.is_new_type() {
                    let abstract_members = self.get_abstract_members_for_class(cls);
                    let unimplemented = abstract_members.unimplemented_abstract_methods();
                    if !unimplemented.is_empty() {
                        self.error(
                            errors,
                            callee_range,
                            ErrorKind::BadInstantiation,
                            format!(
                                "Cannot instantiate `{}` because the following members are abstract: {}",
                                cls.name(self),
                                unimplemented
                                    .iter()
                                    .map(|x| format!("`{x}`"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        );
                    } else if metadata.is_explicitly_abstract() {
                        self.error(
                            errors,
                            callee_range,
                            ErrorKind::DirectAbstractBaseInstantiation,
                            format!(
                                "Cannot instantiate `{}` because it directly extends `ABC` or uses `ABCMeta`",
                                cls.name(self)
                            ),
                        );
                    }
                }
                match instance {
                    Type::ClassType(instance) => self.constructor_to_callable(&instance),
                    _ => ty,
                }
            }
            Type::Type(inner) => match *inner {
                Type::ClassType(instance) => self.constructor_to_callable(&instance),
                other => Type::Type(Box::new(other)),
            },
            // A callback-protocol instance normalizes to its `__call__` bound method (a
            // `Type::BoundMethod`), whose receiver the signature match below strips. A plain
            // instance with no `__call__` defers to the stub.
            Type::ClassType(instance) => match self.instance_as_dunder_call(&instance) {
                Some(dunder_call) => dunder_call,
                None => Type::ClassType(instance),
            },
            other => other,
        };
        // Overloaded bound methods need their receiver bound before checking each overload's arguments.
        let target_ty = if let Type::BoundMethod(method) = &target_ty
            && matches!(&method.func, BoundMethodType::Overload(_))
            && let Some(bound) =
                self.bind_boundmethod(method, &mut |got, want| self.is_subset_eq(got, want))
        {
            bound
        } else {
            target_ty
        };
        // A union target with a non-callable member can never be a valid partial target for that
        // member, so flag it the way a direct call would; the stub still reports the whole union as
        // not assignable to `func`.
        if let Type::Union(union) = &target_ty {
            for member in &union.members {
                if matches!(
                    self.as_call_target(member.clone()),
                    CallTargetLookup::Error(..)
                ) {
                    self.error(
                        errors,
                        target.range(),
                        ErrorKind::NotCallable,
                        format!(
                            "Expected a callable, got `{}`",
                            self.display(member.clone())
                        ),
                    );
                }
            }
        }
        // Fall back to the stub, reusing the already-inferred target so it isn't inferred twice.
        let fallback = |me: &Self| {
            let result = infer_stub(&target_ty);
            if matches!(&target_ty, Type::Overload(overload) if overload.signatures.iter().any(|x| matches!(x, OverloadType::Forall(_))))
            {
                me.specialize(
                    partial_cls,
                    vec![me.heap.mk_any_implicit()],
                    callee_range,
                    errors,
                )
            } else {
                result
            }
        };
        let wrap_result = |mut signature: Type| {
            if !is_partialmethod {
                return signature;
            }
            signature.transform_toplevel_callable_signatures(|callable, _| {
                if let Params::Partial(params) = &callable.params {
                    callable.params = Params::List(params.clone());
                }
            });
            let ret = self.unions(
                signature
                    .toplevel_callable_signatures()
                    .map(|(sig, tparams)| {
                        let mut ret = sig.ret.clone();
                        ret.subst_mut_fn(&mut |q| {
                            tparams
                                .as_ref()
                                .is_some_and(|tparams| tparams.iter().any(|tp| tp == q))
                                .then(|| q.as_gradual_type())
                        });
                        ret
                    })
                    .collect(),
            );
            let Type::ClassType(cls) =
                self.specialize(partial_cls, vec![ret], callee_range, errors)
            else {
                unreachable!("partialmethod specializes to a class instance");
            };
            cls.with_partialmethod_signature(signature).to_type()
        };
        // `partial(f)` with nothing bound is a pure forwarder; for an overloaded target hand the
        // overload back unchanged so ordinary overload resolution still applies at the call site (a
        // single remaining parameter list can't preserve overload branches).
        if !is_partialmethod
            && args.len() == 1
            && kws.is_empty()
            && matches!(target_ty, Type::Overload(_))
        {
            return target_ty;
        }
        // The remaining parameters are keyed by the names the bound keywords consume. A `**` splat of an
        // `Unpack[TypedDict]` binds exactly the TypedDict's declared fields, so expand it to those
        // names; a splat of any other type can't be reduced structurally, so defer to the stub.
        let Some(bound_kw_names) = self.partial_bound_kw_names(&kws_for_check) else {
            return fallback(self);
        };
        let mut bound_args = Cow::Borrowed(&args[1..]);
        let mut is_placeholder: Vec<bool> = bound_args
            .iter()
            .map(|arg| match arg {
                CallArg::Arg(x) => is_placeholder_arg(self, x),
                CallArg::Star(..) => false,
            })
            .collect();
        let args_for_check: Vec<CallArg> = bound_args
            .iter()
            .zip(is_placeholder.iter())
            .map(|(arg, &is_ph)| {
                if is_ph {
                    CallArg::ty(&any_explicit, arg.range())
                } else {
                    arg.clone()
                }
            })
            .collect();
        // Preserve the receiver in the remaining signature; it binds at access time.
        if is_partialmethod {
            bound_args
                .to_mut()
                .insert(0, CallArg::ty(&any_explicit, target.range()));
            is_placeholder.insert(0, true);
        }
        // Overloaded target with bound arguments: drop branches the bound arguments can't satisfy and
        // recombine the surviving partial callables into an overload, so per-call resolution still works.
        if let Type::Overload(overload) = &target_ty {
            let mut partials: Vec<Callable> = Vec::new();
            // The soft errors of the accepted branches, reported only if the partial is built from
            // them; the fallback re-infers the call and reports its own errors.
            let mut accepted_trials = Vec::new();
            for ot in overload.signatures.iter() {
                let (branch_sig, quantified) = match ot {
                    OverloadType::Function(func) => (func.signature.clone(), None),
                    OverloadType::Forall(forall) => {
                        let (qs, sig) = self.instantiate_fresh_callable(
                            &forall.tparams,
                            forall.body.signature.clone(),
                        );
                        (sig, Some(qs))
                    }
                };
                // Trial-check the bound arguments against this branch; keep it only if they produce
                // no hard errors.
                // Optional params keep the still-unbound parameters from erroring as missing.
                let mut probe = branch_sig.clone();
                prepare_partial_arg_check(&mut probe, is_partialmethod);
                let trial = self.error_collector();
                self.freeform_call_infer(
                    self.heap.mk_callable_from(probe),
                    &args_for_check,
                    &kws_for_check,
                    target.range(),
                    arg_range,
                    None,
                    &trial,
                );
                let generic = quantified.is_some();
                // Keep solutions learned from the bound arguments. Variables left unsolved in a
                // generic branch become gradual rather than escaping without a binder.
                let specialization_ok = quantified
                    .map(|qs| self.finish_quantified(qs, false).is_ok())
                    .unwrap_or(true);
                if trial.has_hard() || !specialization_ok {
                    continue;
                }
                let mut branch_sig = if generic {
                    let mut ty = self.heap.mk_callable_from(branch_sig);
                    self.solver().expand_mut(&mut ty, self);
                    let Type::Callable(callable) = ty else {
                        unreachable!("built by mk_callable_from");
                    };
                    *callable
                } else {
                    branch_sig
                };
                self.expand_unpack_kwargs(&mut branch_sig);
                // Defer the whole overload rather than silently drop a matched branch we can't
                // represent, which would break a call that only matched that branch.
                match partial_callable(&branch_sig, &bound_args, &is_placeholder, &bound_kw_names) {
                    Some(partial) => {
                        partials.push(partial);
                        accepted_trials.push(trial);
                    }
                    None => return fallback(self),
                }
            }
            for trial in accepted_trials {
                errors.extend(trial);
            }
            let result = match partials.len() {
                0 => return fallback(self),
                1 => self.heap.mk_callable_from(partials.pop().unwrap()),
                _ => {
                    let branches = partials
                        .into_iter()
                        .map(|c| {
                            OverloadType::Function(Function {
                                signature: c,
                                metadata: (*overload.metadata).clone(),
                            })
                        })
                        .collect::<Vec<_>>();
                    Type::Overload(Overload {
                        signatures: Vec1::try_from_vec(branches).unwrap(),
                        metadata: overload.metadata.clone(),
                    })
                }
            };
            return wrap_result(result);
        }
        // We handle a directly-typed function/callable and a generic (`Forall`-wrapped) function.
        // A generic target keeps its `tparams`: type variables the bound args don't pin stay symbolic
        // in the remaining signature and are finalized at the result boundary, so a partial over a
        // generic function (including decorator use) preserves its genericity instead of leaking them
        // through the stub. Class objects, bound methods, and unions defer.
        let Ok((sig, tparams)) = target_ty.toplevel_callable_signatures().exactly_one() else {
            return fallback(self);
        };
        let mut sig = if matches!(target_ty, Type::BoundMethod(_)) {
            // Strip the already-bound `self`/`cls` so the partial has only the remaining parameters;
            // bound-argument checking against `target_ty` still binds the receiver as usual.
            match sig.strip_first_param() {
                Some(stripped_sig) => stripped_sig,
                None => return fallback(self),
            }
        } else {
            sig.clone()
        };
        // Only plain type variables are re-scoped correctly; a `ParamSpec` or `TypeVarTuple` target
        // needs structural partial handling we don't do, so defer it to the stub.
        if let Some(tparams) = &tparams
            && !tparams.iter().all(|q| q.kind() == QuantifiedKind::TypeVar)
        {
            return fallback(self);
        }
        if !matches!(sig.params, Params::List(_)) {
            return fallback(self);
        }
        self.expand_unpack_kwargs(&mut sig);
        // Nominal `partial[ret]` fallback for when no precise signature can be built. For a generic target
        // erase the target's own type vars so they don't leak out of scope; otherwise defer to the stub.
        let nominal_partial = |me: &Self, ret: Type| -> Type {
            match &tparams {
                None => fallback(me),
                Some(tparams) => {
                    let Type::ClassDef(cls) = partial_ty else {
                        unreachable!("call_functools_partial dispatched on a non-class callee");
                    };
                    let mut ret = ret;
                    ret.subst_mut_fn(&mut |q| {
                        tparams
                            .iter()
                            .any(|tp| tp == q)
                            .then(|| q.as_gradual_type())
                    });
                    me.specialize(cls, vec![ret], callee_range, errors)
                }
            }
        };
        // Type-check the bound arguments; making every parameter optional means binding only a prefix
        // doesn't report the remaining parameters as missing. For a generic target we instantiate the
        // type parameters as fresh vars and check against those, so a bound argument can *solve* a
        // typevar (e.g. pin it to an enclosing-scope typevar); the partial is then built from the
        // solved signature, and only the typevars the bound args left unsolved are restored as
        // quantifieds marked for finalization at the result boundary.
        let (sig, generic_target) = match &tparams {
            None => {
                let mut callee = target_ty.clone();
                callee.transform_toplevel_callable_signatures(|c: &mut Callable, _| {
                    self.expand_unpack_kwargs(c);
                    prepare_partial_arg_check(c, is_partialmethod);
                });
                self.freeform_call_infer(
                    callee,
                    &args_for_check,
                    &kws_for_check,
                    target.range(),
                    arg_range,
                    None,
                    errors,
                );
                (sig, None)
            }
            Some(tparams) => {
                let (qs, inst) = self.instantiate_fresh_callable(tparams, sig);
                let var_to_q: SmallMap<Var, Quantified> = qs
                    .vars()
                    .iter()
                    .copied()
                    .zip(tparams.iter().cloned())
                    .collect();
                let mut callee = self.heap.mk_callable_from(inst.clone());
                callee.transform_toplevel_callable_signatures(|c: &mut Callable, _| {
                    self.expand_unpack_kwargs(c);
                    prepare_partial_arg_check(c, is_partialmethod);
                });
                let outcome = self.freeform_call_infer(
                    callee,
                    &args_for_check,
                    &kws_for_check,
                    target.range(),
                    arg_range,
                    None,
                    errors,
                );
                // A typevar in a *required* remaining param stays symbolic so a later call arg can re-solve
                // it (as a direct call would); one only in optional params keeps its solved value (GH #3546).
                let mut regeneric_vars: SmallSet<Var> = SmallSet::new();
                if let Some(remaining) =
                    partial_params(&inst, &bound_args, &is_placeholder, &bound_kw_names)
                {
                    for param in remaining.items() {
                        let vars = match param {
                            Param::Kwargs(_, ty) if let Some(td) = ty.unpacked_typed_dict() => {
                                // Declared fields are represented by named overlays. Only the
                                // type of genuine extra items remains variadic here.
                                self.typed_dict_extra_items(td)
                                    .extra_item(self.stdlib)
                                    .ty
                                    .collect_all_vars()
                            }
                            Param::PosOnly(_, ty, Required::Required)
                            | Param::Pos(_, ty, Required::Required)
                            | Param::KwOnly(_, ty, Required::Required)
                            | Param::Varargs(_, ty)
                            | Param::Kwargs(_, ty) => ty.collect_all_vars(),
                            _ => continue,
                        };
                        for v in vars {
                            if var_to_q.contains_key(&v) {
                                regeneric_vars.insert(v);
                            }
                        }
                    }
                }
                // Finalize the vars `instantiate_fresh_callable` registered. Their substitution is
                // applied by `partial_solved_signature` below, and any specialization error was reported by
                // the bound-argument check via `freeform_call_infer`, so dropping the result is safe.
                let _ = self.finish_quantified(qs, false);
                let solved = self.partial_solved_signature(&inst, &var_to_q, &regeneric_vars);
                (
                    solved,
                    Some((outcome.overload_table, inst, var_to_q, regeneric_vars)),
                )
            }
        };
        let build = |sig: &Callable| {
            let remaining = partial_params(sig, &bound_args, &is_placeholder, &bound_kw_names)?;
            // A `TypeGuard`/`TypeIs` narrows only in a direct call; the partial just returns `bool`.
            let ret = match &sig.ret {
                Type::TypeGuard(_) | Type::TypeIs(_) => self.stdlib.bool().clone().to_type(),
                other => other.clone(),
            };
            let callable = Callable::partial(remaining, ret);
            // A free type parameter left in the partial is one the partial declares, which
            // finishing the type at this boundary does.
            Some(
                self.heap
                    .mk_callable_from(callable)
                    .finalize_exposed_free_quantifieds(),
            )
        };
        // The arguments can't be reduced to a precise signature (e.g. too many bound positionals); hand
        // back the nominal `partial[ret]` rather than re-running the stub over a `Forall`.
        let Some(result) = build(&sig) else {
            return nominal_partial(self, sig.ret);
        };
        // Rebuild the partial under each solution to preserve correlations between solved variables.
        if let Some((overload_table, inst, var_to_q, regeneric_vars)) = &generic_target {
            let per_row = self.solver().per_row(overload_table, || {
                build(&self.partial_solved_signature(inst, var_to_q, regeneric_vars))
                    .unwrap_or_else(|| result.clone())
            });
            return wrap_result(self.combine_overload_results(per_row, overload_table));
        }
        wrap_result(result)
    }

    /// Synthesize `__get__` while leaving ordinary descriptor resolution in charge of access.
    pub(crate) fn get_partialmethod_attribute(
        &self,
        cls: &ClassType,
        name: &Name,
    ) -> Option<ClassAttribute> {
        if name != &dunder::GET {
            return None;
        }
        let unbound = cls.partialmethod_signature()?;
        // Class reads may add binders whose quantifieds cannot escape into getter parameters.
        let mut receiver = self.get_callable_positional_param(unbound, 0);
        for (_, tparams) in unbound.toplevel_callable_signatures() {
            if let Some(tparams) = tparams {
                receiver.subst_mut_fn(&mut |q| {
                    tparams
                        .iter()
                        .any(|tp| tp == q)
                        .then(|| q.as_gradual_type())
                });
            }
        }
        let mut bound = unbound.clone();
        bound.transform_toplevel_callable_signatures(|signature, _| {
            *signature = signature
                .strip_first_param()
                .expect("partialmethod retains its receiver");
        });
        let metadata = FuncMetadata::method(self, cls.class_object(), dunder::GET);
        let signatures = [(self.heap.mk_none(), unbound.clone()), (receiver, bound)]
            .into_iter()
            .map(|(obj, ret)| {
                OverloadType::Function(Function {
                    signature: Callable::list(
                        ParamList::new(vec![
                            Param::PosOnly(None, obj, Required::Required),
                            Param::PosOnly(
                                None,
                                self.heap.mk_any_explicit(),
                                Required::Optional(None),
                            ),
                        ]),
                        ret,
                    ),
                    metadata: metadata.clone(),
                })
            })
            .collect();
        let getter = Type::Overload(Overload {
            signatures: Vec1::try_from_vec(signatures).expect("two descriptor access branches"),
            metadata: Box::new(metadata),
        });
        Some(ClassAttribute::read_write(getter))
    }

    /// The target's signature with the bound arguments' solutions applied.
    ///
    /// A type parameter still standing in a required remaining parameter is one a later call can
    /// solve, so it is marked for finalization before expanding rather than being frozen to what
    /// the bound arguments happened to imply. `expand_with_bounds` then substitutes the ones a
    /// bound argument pinned and leaves the rest, which are marked likewise. Finishing instead of
    /// expanding would erase them all to `Any`.
    fn partial_solved_signature(
        &self,
        inst: &Callable,
        var_to_q: &SmallMap<Var, Quantified>,
        regeneric_vars: &SmallSet<Var>,
    ) -> Callable {
        let mut solved = self.heap.mk_callable_from(inst.clone());
        solved = solved.transform(&mut |t: &mut Type| {
            if let Type::Var(v) = t
                && regeneric_vars.contains(v)
                && let Some(q) = var_to_q.get(v)
            {
                *t = self.heap.mk_quantified(q.clone().with_needs_finalization());
            }
        });
        self.solver().expand_with_bounds(&mut solved, self);
        let solved = solved.transform(&mut |t: &mut Type| {
            if let Type::Var(v) = t
                && let Some(q) = var_to_q.get(v)
            {
                *t = self.heap.mk_quantified(q.clone().with_needs_finalization());
            }
        });
        match solved {
            Type::Callable(c) => *c,
            _ => unreachable!("built by mk_callable_from"),
        }
    }

    /// The parameter names the bound keyword arguments consume, used to build the partial. A named
    /// keyword contributes its own name; a `**` splat of an `Unpack[TypedDict]` contributes every
    /// field the TypedDict declares. Returns `None` for a splat of any other type, which can't be
    /// reduced to a fixed set of names.
    fn partial_bound_kw_names(&self, keywords: &[CallKeyword]) -> Option<Vec<Name>> {
        let mut names = Vec::new();
        for kw in keywords {
            match kw.arg {
                Some(id) => names.push(id.id.clone()),
                None => {
                    // The bound arguments are re-inferred and validated below; here we only want the
                    // splat's field names, so any inference error is reported there, not here.
                    let ty = kw.value.infer(self, &self.error_collector());
                    match &ty {
                        Type::ClassType(ct) if self.is_typed_dict_class(ct.class_object()) => {
                            names.extend(self.typed_dict_fields(ct).into_iter().map(|(n, _)| n))
                        }
                        Type::AnonymousTypedDict(td) => {
                            names.extend(td.fields.iter().map(|(name, _)| name.clone()))
                        }
                        _ => return None,
                    }
                }
            }
        }
        Some(names)
    }

    /// Expose TypedDict fields for partial keyword binding, retaining their unpacked contract.
    /// Named parameters override the corresponding fields after binding so required keys can
    /// become optional without losing the policy for extra keyword arguments.
    fn expand_unpack_kwargs(&self, callable: &mut Callable) {
        let Params::List(params) = &mut callable.params else {
            return;
        };
        if !params.items().iter().any(
            |param| matches!(param, Param::Kwargs(_, ty) if ty.unpacked_typed_dict().is_some()),
        ) {
            return;
        }
        let explicit_names: SmallSet<&Name> = params
            .items()
            .iter()
            .filter_map(|param| match param {
                Param::Pos(name, ..) | Param::KwOnly(name, ..) => Some(name),
                _ => None,
            })
            .collect();
        let mut expanded: Vec<Param> = Vec::with_capacity(params.items().len());
        for param in params.items() {
            if let Param::Kwargs(_, ty) = param
                && let Some(td) = ty.unpacked_typed_dict()
            {
                for (name, ty, required) in self.typed_dict_kw_param_info(td) {
                    if !explicit_names.contains(&name) {
                        expanded.push(Param::KwOnly(name, ty, required));
                    }
                }
            }
            expanded.push(param.clone());
        }
        drop(explicit_names);
        *params = ParamList::new(expanded);
    }
}

/// Prepare the signature for checking supplied bound arguments. A method's receiver binds later,
/// and remaining parameters may be left unbound by partial application.
fn prepare_partial_arg_check(callable: &mut Callable, is_partialmethod: bool) {
    if is_partialmethod {
        *callable = callable
            .strip_first_param()
            .expect("supported partialmethod targets retain a receiver");
    }
    match &mut callable.params {
        Params::List(params) => {
            for param in params.items_mut() {
                match param {
                    Param::PosOnly(_, _, r) | Param::Pos(_, _, r) | Param::KwOnly(_, _, r) => {
                        *r = Required::Optional(None)
                    }
                    Param::Varargs(..) | Param::Kwargs(..) => {}
                }
            }
        }
        // The trailing `ParamSpec` already absorbs extra arguments; only the prefix needs relaxing.
        Params::ParamSpec(prefix, _) => {
            for param in prefix.iter_mut() {
                match param {
                    PrefixParam::PosOnly(_, _, r) | PrefixParam::Pos(_, _, r) => {
                        *r = Required::Optional(None)
                    }
                }
            }
        }
        Params::Partial(_) | Params::Ellipsis | Params::Materialization => {}
    }
}

/// Partial `Callable` for one matched overload branch, or `None` if it can't be represented
/// structurally (the caller then defers instead of dropping the branch).
fn partial_callable(
    branch: &Callable,
    bound_args: &[CallArg],
    is_placeholder: &[bool],
    keyword_names: &[Name],
) -> Option<Callable> {
    match &branch.params {
        Params::List(_) => partial_params(branch, bound_args, is_placeholder, keyword_names)
            .map(|params| Callable::partial(params, branch.ret.clone())),
        // `(...)` still accepts anything after binding a prefix.
        Params::Ellipsis => Some(Callable::ellipsis(branch.ret.clone())),
        // `Concatenate[..., P]` binds its prefix first; the partial keeps the unbound prefix and `P`.
        Params::ParamSpec(prefix, tail) => {
            if is_placeholder.iter().any(|&b| b) {
                return None;
            }
            partial_paramspec_prefix(prefix, bound_args, keyword_names).map(|prefix| Callable {
                params: Params::ParamSpec(prefix, tail.clone()),
                ret: branch.ret.clone(),
            })
        }
        Params::Partial(_) | Params::Materialization => None,
    }
}

/// Peel the bound `bound_args`/`keywords` off a `Concatenate` prefix. `None` if an argument would
/// fall through into the trailing `ParamSpec`, which can't be peeled structurally.
fn partial_paramspec_prefix(
    prefix: &[PrefixParam],
    bound_args: &[CallArg],
    keyword_names: &[Name],
) -> Option<Box<[PrefixParam]>> {
    let mut remaining = prefix.to_vec();
    for arg in bound_args {
        let CallArg::Arg(_) = arg else {
            return None;
        };
        if remaining.is_empty() {
            return None;
        }
        remaining.remove(0);
    }
    for name in keyword_names {
        let idx = remaining
            .iter()
            .position(|p| matches!(p, PrefixParam::Pos(n, ..) if n == name))?;
        remaining.remove(idx);
    }
    Some(remaining.into_boxed_slice())
}

/// Parameters remaining after binding arguments to `callable`. Returns `None` when the
/// target/arguments can't be reduced.
fn partial_params(
    callable: &Callable,
    bound_args: &[CallArg],
    is_placeholder: &[bool],
    keyword_names: &[Name],
) -> Option<ParamList> {
    let Params::List(params) = &callable.params else {
        return None;
    };
    let mut remaining = params.items().to_vec();
    let mut placeholder_params: Vec<Param> = Vec::new();
    for (arg, &is_ph) in bound_args.iter().zip(is_placeholder) {
        let CallArg::Arg(_) = arg else {
            return None;
        };
        let idx = remaining
            .iter()
            .position(|p| matches!(p, Param::PosOnly(..) | Param::Pos(..) | Param::Varargs(..)))?;
        let param = if matches!(remaining[idx], Param::Varargs(..)) {
            remaining[idx].clone()
        } else {
            remaining.remove(idx)
        };
        if is_ph {
            placeholder_params.push(match param {
                Param::PosOnly(n, t, _) => Param::PosOnly(n, t, Required::Required),
                Param::Pos(n, t, _) => Param::PosOnly(Some(n), t, Required::Required),
                Param::Varargs(n, t) => Param::PosOnly(n, t, Required::Required),
                _ => unreachable!("position() only selects positional parameters"),
            });
        }
    }
    placeholder_params.extend(remaining);
    let mut remaining = placeholder_params;
    for name in keyword_names {
        let idx = remaining.iter().position(|p| {
            matches!(p, Param::Pos(n, ..) | Param::KwOnly(n, ..) if n == name)
                || matches!(p, Param::Kwargs(..))
        })?;
        if matches!(remaining[idx], Param::Kwargs(..)) {
            continue;
        }
        // A positional can't follow a bound keyword, so demote later positionals to keyword-only.
        if matches!(remaining[idx], Param::Pos(..)) {
            for later in remaining.iter_mut().skip(idx + 1) {
                if let Param::Pos(n, t, r) = later {
                    *later = Param::KwOnly(n.clone(), t.clone(), r.clone());
                }
            }
        }
        let (n, t) = match &remaining[idx] {
            Param::Pos(n, t, _) | Param::KwOnly(n, t, _) => (n.clone(), t.clone()),
            _ => unreachable!("matched a positional or keyword-only parameter"),
        };
        remaining[idx] = Param::KwOnly(n, t, Required::Optional(None));
    }
    Some(ParamList::new(remaining))
}

fn can_prepare_partial_arg(value: CallValue) -> bool {
    match value.as_type_or_expr() {
        TypeOrExpr::Expr(Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_)) => true,
        TypeOrExpr::Expr(Expr::Named(named)) => {
            matches!(&*named.target, Expr::Name(name) if !Ast::is_synthesized_empty_name(name))
        }
        _ => false,
    }
}

fn is_placeholder_arg<Ans: LookupAnswer>(solver: &AnswersSolver<Ans>, x: &CallValue) -> bool {
    let TypeOrExpr::Type(ty, _) = x.as_type_or_expr() else {
        return false;
    };
    let ty = solver.solver().expand(ty.clone(), solver);
    matches!(
        ty,
        Type::ClassType(cls)
            if {
                let qname = cls.qname(solver);
                qname.module_name().as_str() == "functools"
                    && qname.id().as_str() == "_PlaceholderType"
            }
    )
}
