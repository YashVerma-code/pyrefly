/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! See `lib/test/pydantic/dagster.rs` for the runtime contract and examples.

use std::iter;

use pyrefly_python::module_name::ModuleName;
use pyrefly_types::known_class::KnownClass;
use ruff_python_ast::Expr;
use ruff_python_ast::name::Name;
use ruff_text_size::TextRange;

use crate::alt::answers::LookupAnswer;
use crate::alt::answers_solver::AnswersSolver;
use crate::alt::types::class_metadata::ClassMetadata;
use crate::types::types::Type;

const RESOURCE_FACTORY: Name = Name::new_static("ConfigurableResourceFactory");
const PARTIAL_RESOURCE: Name = Name::new_static("PartialResource");
const RESOURCE_DEFINITION: Name = Name::new_static("ResourceDefinition");

impl<'ctx, 'answer, Ans: LookupAnswer> AnswersSolver<'ctx, 'answer, Ans> {
    pub fn has_dagster_resource_metaclass(&self, metadata: &ClassMetadata) -> bool {
        let Some(metaclass) = metadata.custom_metaclass() else {
            return false;
        };
        iter::once(metaclass)
            .chain(
                self.get_mro_for_class(metaclass.class_object())
                    .ancestors_no_object(),
            )
            .any(|base| base.class_object().is(KnownClass::DagsterBaseResourceMeta))
    }

    pub fn dagster_rewrites_annotation(&self, annotation: &Expr) -> bool {
        let errors = self.error_swallower();
        match annotation {
            Expr::Subscript(subscript) => matches!(
                self.expr_infer(&subscript.value, &errors),
                Type::ClassDef(class) if class.is(KnownClass::DagsterResourceDependency)
            ),
            Expr::Name(_) | Expr::Attribute(_) => {
                let Type::ClassDef(class) = self.expr_infer(annotation, &errors) else {
                    return false;
                };
                class.is(KnownClass::DagsterConfigurableResourceFactory)
                    || (class.known().is_none()
                        && self
                            .get_mro_for_class(&class)
                            .ancestors_no_object()
                            .iter()
                            .any(|base| {
                                base.class_object()
                                    .is(KnownClass::DagsterConfigurableResourceFactory)
                            }))
            }
            _ => false,
        }
    }

    pub fn get_dagster_init_field_type(&self, ty: &Type) -> Option<Type> {
        let Type::ClassType(field) = ty else {
            return None;
        };
        if field.is(KnownClass::DagsterResourceDependency) {
            let [value] = field.targs().as_slice() else {
                return None;
            };
            let factory = self.specialize_dagster_resource(RESOURCE_FACTORY, value.clone())?;
            let partial = self.specialize_dagster_resource(PARTIAL_RESOURCE, value.clone())?;
            let Type::ClassDef(definition) = self.try_get_from_export(
                ModuleName::from_str("dagster._core.definitions.resource_definition"),
                RESOURCE_DEFINITION,
            )?
            else {
                return None;
            };
            return Some(self.unions(vec![
                value.clone(),
                factory,
                partial,
                self.instantiate(definition),
            ]));
        }
        if iter::once(field)
            .chain(
                self.get_mro_for_class(field.class_object())
                    .ancestors_no_object(),
            )
            .any(|base| {
                base.class_object()
                    .is(KnownClass::DagsterConfigurableResourceFactory)
            })
        {
            let partial = self.specialize_dagster_resource(PARTIAL_RESOURCE, ty.clone())?;
            return Some(self.union(self.expand_type_for_lax_mode(ty), partial));
        }
        None
    }

    fn specialize_dagster_resource(&self, name: Name, value: Type) -> Option<Type> {
        let Type::ClassDef(class) = self.try_get_from_export(
            ModuleName::from_str("dagster._config.pythonic_config.resource"),
            name,
        )?
        else {
            return None;
        };
        Some(self.specialize(
            class,
            vec![value],
            TextRange::default(),
            &self.error_swallower(),
        ))
    }
}
