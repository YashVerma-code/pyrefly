/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use crate::test::pydantic::util::pydantic_env;
use crate::test::util::TestEnv;
use crate::testcase;

// Static API shapes from Dagster 1.13.31. Runtime validation is exercised separately
// against the installed package; the fixture does not execute annotation rewriting.
// Typed dependency compatibility and declared requiredness are static contracts;
// Dagster's runtime Any rewrite is deliberately more permissive.
fn dagster_env() -> TestEnv {
    let mut env = pydantic_env();
    env.add("dagster._config", "");
    env.add("dagster._config.pythonic_config", "");
    env.add("dagster._core", "");
    env.add("dagster._core.definitions", "");
    env.add(
        "dagster._core.definitions.resource_definition",
        r#"
from typing import Any
class ResourceDefinition:
    def __init__(self, resource_fn: Any) -> None: ...
"#,
    );
    env.add(
        "dagster._config.pythonic_config.typing_utils",
        r#"
from typing import Any
from pydantic import Field
from pydantic._internal._model_construction import ModelMetaclass
from typing_extensions import Self, dataclass_transform

@dataclass_transform(kw_only_default=True, field_specifiers=(Field,))
class BaseConfigMeta(ModelMetaclass): ...

@dataclass_transform(kw_only_default=True, field_specifiers=(Field,))
class BaseResourceMeta(BaseConfigMeta): ...

class TypecheckAllowPartialResourceInitParams:
    def __get__(self, obj: Any, owner: Any) -> Self: ...
    def __set__(self, obj: object | None, value: Any) -> None: ...
"#,
    );
    env.add(
        "dagster._config.pythonic_config.config",
        r#"
from pydantic import BaseModel, ConfigDict
from dagster._config.pythonic_config.typing_utils import BaseConfigMeta

class Config(BaseModel, metaclass=BaseConfigMeta):
    model_config = ConfigDict(frozen=True, arbitrary_types_allowed=True)
"#,
    );
    env.add(
        "dagster._config.pythonic_config.resource",
        r#"
from typing import Any, Generic, TypeVar
from typing_extensions import Self
from dagster._config.pythonic_config.config import Config
from dagster._config.pythonic_config.typing_utils import BaseResourceMeta, TypecheckAllowPartialResourceInitParams
from dagster._core.definitions.resource_definition import ResourceDefinition

T = TypeVar("T")

class ConfigurableResourceFactory(Config, TypecheckAllowPartialResourceInitParams, Generic[T], metaclass=BaseResourceMeta):
    def __init__(self, **data: Any) -> None: ...
    def create_resource(self, context: Any) -> T: ...
    @classmethod
    def configure_at_launch(cls, **kwargs: Any) -> "PartialResource[Self]": ...

class ConfigurableResource(ConfigurableResourceFactory[T]): ...

class PartialResource(Config, Generic[T]):
    def __init__(self, resource_cls: type[ConfigurableResourceFactory[T]], data: dict[str, Any]) -> None: ...

class ResourceDependency(Generic[T]):
    def __get__(self, obj: ConfigurableResourceFactory, owner: Any) -> T: ...
    def __set__(self, obj: object | None, value: ConfigurableResourceFactory[T] | PartialResource[T] | ResourceDefinition | T) -> None: ...
"#,
    );
    env.add(
        "dagster",
        r#"
from dagster._config.pythonic_config.config import Config as Config
from dagster._config.pythonic_config.resource import ConfigurableResource as ConfigurableResource
from dagster._config.pythonic_config.resource import ResourceDependency as ResourceDependency
"#,
    );
    env
}

// https://github.com/pyrefly-org/pyrefly/issues/5714
// Dagster's BaseResourceMeta changes annotations before Pydantic's ModelMetaclass sees them:
// ResourceDependency[V] becomes Annotated[Any, "resource_dependency"], and a resource class R
// becomes R | PartialResource. These are constructor inputs; descriptor getters still describe
// the post-initialization V/R values. We retain Dagster's declared typed dependency contract
// instead of accepting arbitrary Any, and keep declared requiredness despite runtime None injection.
// The rewrite only processes direct annotations in the declaring resource class's namespace.
// Annotated wrappers, unions, and fields declared by a plain Config retain Pydantic handling.
// Runtime rewrite: https://github.com/dagster-io/dagster/blob/1.13.31/python_modules/dagster/dagster/_config/pythonic_config/typing_utils.py#L104-L168
// Dependency contract: https://github.com/dagster-io/dagster/blob/1.13.31/python_modules/dagster/dagster/_config/pythonic_config/resource.py#L760-L783
testcase!(
    test_resource_constructor_inputs,
    dagster_env(),
    r#"
from typing import Any, assert_type
from dagster import ConfigurableResource, ResourceDependency
from dagster._config.pythonic_config.resource import ConfigurableResourceFactory, PartialResource
from dagster._core.definitions.resource_definition import ResourceDefinition

class Creds(ConfigurableResource):
    user: str

class StringFactory(ConfigurableResourceFactory[str]):
    def create_resource(self, context: Any) -> str:
        return "x"

class IntFactory(ConfigurableResourceFactory[int]):
    def create_resource(self, context: Any) -> int:
        return 0

class Client(ConfigurableResource):
    dep: ResourceDependency[str]
    creds: Creds
    timeout: int = 10

creds = Creds(user="u")
partial = Creds.configure_at_launch(user="u")
partial_string = PartialResource[str](resource_cls=StringFactory, data={})

client = Client(dep="x", creds=creds)
Client(dep=StringFactory(), creds=partial)
Client(dep=partial_string, creds=creds)
Client(dep=ResourceDefinition(resource_fn=lambda _: "x"), creds={"user": "u"})
Client(dep=1, creds=creds)  # E: is not assignable to parameter `dep`
Client(dep=IntFactory(), creds=creds)  # E: is not assignable to parameter `dep`
Client(dep="x", creds=42)  # E: is not assignable to parameter `creds`

assert_type(client.dep, str)
assert_type(client.creds, Creds)
assert_type(client.timeout, int)
assert_type(partial, PartialResource[Creds])
Client()  # E: Missing argument `dep` # E: Missing argument `creds`
Client("x", creds)  # E: Expected argument `dep` to be passed by name # E: Expected argument `creds` to be passed by name
"#,
);

testcase!(
    test_resource_constructor_strict_inputs,
    dagster_env(),
    r#"
from dagster import ConfigurableResource, ResourceDependency

class Creds(ConfigurableResource):
    user: str

class StrictClient(ConfigurableResource, strict=True):
    dep: ResourceDependency[Creds]
    creds: Creds

creds = Creds(user="u")
partial = Creds.configure_at_launch(user="u")
StrictClient(dep=creds, creds=creds)
StrictClient(dep=partial, creds=partial)
StrictClient(dep=creds, creds={"user": "u"})
# The declared dependency type remains checked despite the permissive runtime rewrite.
StrictClient(dep=1, creds=creds)  # E: is not assignable to parameter `dep`
StrictClient(dep=creds, creds=42)  # E: is not assignable to parameter `creds`
"#,
);

testcase!(
    test_resource_constructor_unrewritten_annotations,
    dagster_env(),
    r#"
from typing import Annotated
from pydantic import Field
from dagster import Config, ConfigurableResource, ResourceDependency

class Creds(ConfigurableResource):
    user: str

class OptionalClient(ConfigurableResource):
    dep: ResourceDependency[str] | None = None
    creds: Creds | None = None

creds = Creds(user="u")
partial = Creds.configure_at_launch(user="u")
OptionalClient()
OptionalClient(dep=None, creds=None)
OptionalClient(creds={"user": "u"})
OptionalClient(dep="x")  # E: is not assignable to parameter `dep`
OptionalClient(creds=partial)  # E: is not assignable to parameter `creds`

class AnnotatedClient(ConfigurableResource):
    dep: Annotated[ResourceDependency[str], Field()]
    creds: Annotated[Creds, Field()]

class OpaqueAnnotatedClient(ConfigurableResource):
    dep: Annotated[ResourceDependency[str], "metadata"]

AnnotatedClient(dep=ResourceDependency[str](), creds=creds)
AnnotatedClient(dep="x", creds=creds)  # E: is not assignable to parameter `dep`
AnnotatedClient(dep=ResourceDependency[str](), creds=partial)  # E: is not assignable to parameter `creds`
OpaqueAnnotatedClient(dep="x")  # E: is not assignable to parameter `dep`

# A resource metaclass does not rewrite fields originally declared by a plain Config.
class Mixin(Config):
    creds: Creds

class InheritedClient(Mixin, ConfigurableResource): ...

InheritedClient(creds=creds)
InheritedClient(creds=partial)  # E: is not assignable to parameter `creds`

# BaseConfigMeta does not perform the resource annotation rewrite.
class PlainConfig(Config):
    dep: ResourceDependency[str]
    creds: Creds

PlainConfig(dep=ResourceDependency[str](), creds=creds)
PlainConfig(dep="x", creds=creds)  # E: is not assignable to parameter `dep`
PlainConfig(dep=ResourceDependency[str](), creds=partial)  # E: is not assignable to parameter `creds`
"#,
);

// Verified with Dagster 1.13.31 and Pydantic 2.14.4 on Python 3.12: the apparent
// resource-instance default is not a usable Pydantic default, because its class-level
// descriptor getter raises AttributeError. Redeclaring the annotation does not supply
// a default, so the missing-argument diagnostic below is intentional and correct.
testcase!(
    test_resource_redeclaration_requires_input_after_descriptor_default,
    dagster_env(),
    r#"
from dagster import Config, ConfigurableResource

class Creds(ConfigurableResource):
    user: str = "u"

class Parent(Config):
    creds: Creds = Creds()

class Child(Parent, ConfigurableResource):
    creds: Creds

Child(creds=Creds())
Child(creds=Creds.configure_at_launch())
Child()  # E: Missing argument `creds`
"#,
);

testcase!(
    test_resource_constructor_redeclarations,
    dagster_env(),
    r#"
from typing import Annotated
from pydantic import Field
from dagster import Config, ConfigurableResource, ResourceDependency

class Creds(ConfigurableResource):
    user: str

class Mixin(Config):
    creds: Creds

class LocalClient(Mixin, ConfigurableResource):
    creds: Creds

creds = Creds(user="u")
partial = Creds.configure_at_launch(user="u")
LocalClient(creds=creds)
LocalClient(creds=partial)

class Parent(ConfigurableResource):
    dep: ResourceDependency[str]

class WrappedChild(Parent):
    dep: Annotated[ResourceDependency[str], Field()]

WrappedChild(dep=ResourceDependency[str]())
WrappedChild(dep="x")  # E: is not assignable to parameter `dep`
"#,
);
