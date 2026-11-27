/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `pyrefly/typeFacts`: the types of many expressions in one file, in one request.
//!
//! TSP's type requests answer one position at a time with a full `Type` built for
//! re-resolving declarations in an editor. Batch analysis clients such as linters
//! ask about hundreds of expressions per file and want to know what a type *is*:
//! which class (and its MRO), which module, which function and what it returns.
//! This request answers a list of ranges against one transaction with those facts.

use lsp_types::Range;
use pyrefly_build::handle::Handle;
use pyrefly_python::qname::QName;
use pyrefly_types::class::Class;
use pyrefly_types::function::FuncFlags;
use pyrefly_types::function::FuncMetadata;
use pyrefly_types::types::BoundMethodType;
use pyrefly_types::types::Forallable;
use pyrefly_types::types::OverloadType;
use pyrefly_types::types::Type;
use pyrefly_util::display::DisplayWithCtx;
use ruff_python_ast::name::Name;
use serde::Deserialize;
use serde::Serialize;

use crate::alt::answers_solver::AnswersSolver;
use crate::state::state::Transaction;
use crate::state::state::TransactionHandle;
use crate::tsp::type_conversion::path_to_uri;
use crate::types::env::TypeEnv;

pub const TYPE_FACTS_METHOD: &str = "pyrefly/typeFacts";

/// How many levels of type arguments, union members and return types to describe.
/// Two covers `Callable[..., ClassType[T]]`-shaped questions without letting a
/// deeply generic type blow up the response.
const NESTING: usize = 2;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeFactsParams {
    pub uri: String,
    pub snapshot: i32,
    pub queries: Vec<TypeFactsQuery>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeFactsQuery {
    pub range: Range,
    /// Ask for the type the surrounding code expects at `range.start` (a call
    /// argument's parameter type, an annotated target's type) instead of the
    /// type computed for `range`. Unlike `typeServer/getExpectedType` there is no
    /// fallback to the computed type, so `null` means "no expectation here".
    #[serde(default)]
    pub expected: bool,
    /// For a class or an instance, also list what the class defines in its own body.
    #[serde(default)]
    pub members: bool,
}

/// Where a class or function is defined: the file of its module and the range of its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Definition {
    pub uri: String,
    pub range: Range,
}

/// What a function's own definition says about it: `async def`, and decorators that
/// Pyrefly interprets rather than applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FunctionFlag {
    Async,
    Staticmethod,
    Classmethod,
    Property,
    /// Comes with `property`: Pyrefly treats a `cached_property` as a property.
    CachedProperty,
    Abstract,
    Overload,
    Override,
    Final,
    Deprecated,
}

/// What `class` and `instance` facts say about the class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassFacts {
    pub qname: String,
    pub definition: Definition,
    pub mro: Vec<String>,
    /// Present for a dataclass, an attrs class, or another `dataclass_transform` class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataclass: Option<DataclassFacts>,
    /// What the class's own body defines, in source order. Present only when the
    /// query asks for members.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<Member>>,
    #[serde(rename = "typeArgs", skip_serializing_if = "Vec::is_empty")]
    pub type_args: Vec<TypeFact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DataclassFacts {
    pub frozen: bool,
}

/// A name a class body binds (a method, a nested class, or an assigned or annotated
/// attribute) and its type after decorators, described without nesting. A `def` whose
/// decorator returns another object, such as `functools.lru_cache`, has that object's type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeFact,
}

/// What a type is, reduced to what an analysis compares on. Qualified names are
/// the defining module plus the name (`pkg.mod.Cls`), not the re-exported path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TypeFact {
    /// A class object: `C` or `type[C]`. Type arguments are present when the
    /// class object is specialized, as in the annotation `list[int]`.
    Class(ClassFacts),
    /// An instance of a class.
    Instance(ClassFacts),
    Module {
        name: String,
    },
    /// A function, bound method, or overloaded function. `qname` is absent for
    /// functions without a name (lambdas), and `definition` for functions without a
    /// `def` (lambdas, synthesized callables).
    Function {
        #[serde(skip_serializing_if = "Option::is_none")]
        qname: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        definition: Option<Definition>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        flags: Vec<FunctionFlag>,
        #[serde(skip_serializing_if = "Option::is_none")]
        returns: Option<Box<TypeFact>>,
    },
    Union {
        members: Vec<TypeFact>,
    },
    Any,
    /// Anything else, described by Pyrefly's display of the type.
    Other {
        display: String,
    },
}

/// Describe `ty`, resolving classes through `handle`'s transaction. `members` lists
/// the members of the class `ty` names, but not of the classes nested in it.
pub fn type_fact(transaction: &Transaction, handle: &Handle, ty: &Type, members: bool) -> TypeFact {
    describe(
        transaction,
        handle,
        &transaction.type_env(handle),
        ty,
        NESTING,
        members,
    )
}

fn describe(
    transaction: &Transaction,
    handle: &Handle,
    env: &dyn TypeEnv,
    ty: &Type,
    depth: usize,
    members: bool,
) -> TypeFact {
    let nested = |types: &[Type]| -> Vec<TypeFact> {
        if depth == 0 {
            return Vec::new();
        }
        types
            .iter()
            .map(|t| describe(transaction, handle, env, t, depth - 1, false))
            .collect()
    };
    let class = |cls: &Class, type_args: Vec<TypeFact>| {
        let solved = solve_class(transaction, handle, cls, members);
        ClassFacts {
            qname: qname(cls.qname(env)),
            definition: definition(cls.qname(env)),
            mro: solved.mro,
            dataclass: solved.dataclass,
            members: solved.members.map(|members| {
                members
                    .into_iter()
                    .map(|(name, ty)| Member {
                        name: name.to_string(),
                        ty: describe(transaction, handle, env, &ty, 0, false),
                    })
                    .collect()
            }),
            type_args,
        }
    };
    let function = |metadata: &FuncMetadata, ret: Option<&Type>| TypeFact::Function {
        qname: function_qname(env, metadata),
        definition: metadata
            .kind
            .as_func_def_id()
            .map(|id| definition(&id.qname)),
        flags: function_flags(&metadata.flags),
        returns: ret
            .filter(|_| depth > 0)
            .map(|r| Box::new(describe(transaction, handle, env, r, depth - 1, false))),
    };
    match ty {
        Type::ClassType(ct) => {
            TypeFact::Instance(class(ct.class_object(), nested(ct.targs().as_slice())))
        }
        Type::ClassDef(cls) => TypeFact::Class(class(cls, Vec::new())),
        Type::SpecializedClassDef(ct) => {
            TypeFact::Class(class(ct.class_object(), nested(ct.targs().as_slice())))
        }
        Type::Type(inner) => match inner.as_ref() {
            Type::ClassType(ct) => {
                TypeFact::Class(class(ct.class_object(), nested(ct.targs().as_slice())))
            }
            // `type[A | None]` -- what an annotation like `A | None` evaluates to --
            // is described as the union of the classes it names, so a client reading
            // a field's annotation sees each member rather than an opaque display.
            Type::Union(union) => TypeFact::Union {
                members: union
                    .members
                    .iter()
                    .map(|member| {
                        describe(
                            transaction,
                            handle,
                            env,
                            &Type::type_of(member.clone()),
                            depth,
                            false,
                        )
                    })
                    .collect(),
            },
            _ => TypeFact::Other {
                display: ty.display_with(env).to_string(),
            },
        },
        Type::Module(module) => TypeFact::Module {
            name: module.to_string(),
        },
        Type::Function(f) => function(&f.metadata, Some(&f.signature.ret)),
        Type::Forall(forall) if let Forallable::Function(f) = &forall.body => {
            function(&f.metadata, Some(&f.signature.ret))
        }
        Type::BoundMethod(bound) => match &bound.func {
            BoundMethodType::Function(f) => function(&f.metadata, Some(&f.signature.ret)),
            BoundMethodType::Forall(forall) => {
                function(&forall.body.metadata, Some(&forall.body.signature.ret))
            }
            BoundMethodType::Overload(overload) => {
                function(&overload.metadata, overload_return(&overload.signatures))
            }
        },
        Type::Overload(overload) => {
            function(&overload.metadata, overload_return(&overload.signatures))
        }
        Type::Union(union) => TypeFact::Union {
            members: nested(&union.members),
        },
        Type::Any(_) => TypeFact::Any,
        _ => TypeFact::Other {
            display: ty.display_with(env).to_string(),
        },
    }
}

/// The first signature's return type: overloads of one function almost always
/// share the class an analysis cares about, and the facts are a summary.
fn overload_return(signatures: &[OverloadType]) -> Option<&Type> {
    signatures.first().map(|signature| match signature {
        OverloadType::Function(f) => &f.signature.ret,
        OverloadType::Forall(forall) => &forall.body.signature.ret,
    })
}

fn qname(name: &QName) -> String {
    format!("{}.{}", name.module_name(), name.id())
}

fn function_flags(flags: &FuncFlags) -> Vec<FunctionFlag> {
    [
        (flags.is_async, FunctionFlag::Async),
        (flags.is_staticmethod, FunctionFlag::Staticmethod),
        (flags.is_classmethod, FunctionFlag::Classmethod),
        (flags.property_metadata.is_some(), FunctionFlag::Property),
        (flags.is_cached_property, FunctionFlag::CachedProperty),
        (flags.is_abstract_method, FunctionFlag::Abstract),
        (flags.is_overload, FunctionFlag::Overload),
        (flags.is_override, FunctionFlag::Override),
        (flags.has_final_decoration, FunctionFlag::Final),
        (flags.deprecation.is_some(), FunctionFlag::Deprecated),
    ]
    .into_iter()
    .filter_map(|(set, flag)| set.then_some(flag))
    .collect()
}

fn definition(name: &QName) -> Definition {
    Definition {
        uri: path_to_uri(name.module_path()),
        range: name.module().to_lsp_range(name.range()),
    }
}

fn function_qname(env: &dyn TypeEnv, metadata: &FuncMetadata) -> Option<String> {
    let symbol = metadata.kind.to_func_symbol()?;
    Some(match &symbol.cls {
        Some(cls) => format!("{}.{}.{}", symbol.module.name(), cls.name(env), symbol.name),
        None => format!("{}.{}", symbol.module.name(), symbol.name),
    })
}

/// What the solver knows about a class.
#[derive(Default)]
struct SolvedClass {
    /// Ancestors in MRO order, excluding the class itself and `object`.
    mro: Vec<String>,
    dataclass: Option<DataclassFacts>,
    /// The names the class body binds, with their types.
    members: Option<Vec<(Name, Type)>>,
}

/// Solve `cls` from `handle`, listing its members if `members` is set. Empty when
/// the class's module cannot be solved from this handle.
fn solve_class(
    transaction: &Transaction,
    handle: &Handle,
    cls: &Class,
    members: bool,
) -> SolvedClass {
    transaction
        .ad_hoc_solve(handle, "type_facts_class", |solver| SolvedClass {
            mro: solver
                .get_mro_for_class(cls)
                .ancestors_no_object()
                .iter()
                .map(|ancestor| qname(ancestor.class_object().qname(&solver)))
                .collect(),
            dataclass: solver
                .get_metadata_for_class(cls)
                .dataclass_metadata()
                .map(|metadata| DataclassFacts {
                    frozen: metadata.kws.frozen,
                }),
            members: members.then(|| own_members(&solver, cls)),
        })
        .unwrap_or_default()
}

fn own_members(solver: &AnswersSolver<TransactionHandle>, cls: &Class) -> Vec<(Name, Type)> {
    let Some(fields) = solver.get_class_fields(cls) else {
        return Vec::new();
    };
    fields
        .class_body_fields()
        .filter_map(|name| {
            let field = solver.get_field_from_current_class_only(cls, name)?;
            Some((name.clone(), field.ty()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::util::get_batched_lsp_operations_report;

    #[test]
    fn type_facts_specialized_class_def() {
        let code = "class Box[T]: ...\ndef f():\n    X = Box[int]\n    X\n#   ^\n";
        let report = get_batched_lsp_operations_report(
            &[("main", code)],
            |state, handle, position| {
                let transaction = state.transaction();
                let ty = transaction.get_type_at(handle, position).unwrap();
                let fact = type_fact(&transaction, handle, &ty, false);
                let TypeFact::Class(ClassFacts {
                    qname, type_args, ..
                }) = fact
                else {
                    panic!("Expected a class fact, got {fact:?}");
                };
                assert_eq!(qname, "main.Box");
                assert!(
                    matches!(type_args.as_slice(), [TypeFact::Instance(ClassFacts { qname, .. })] if qname == "builtins.int")
                );
                String::new()
            },
        );
        assert!(!report.is_empty());
    }
}
