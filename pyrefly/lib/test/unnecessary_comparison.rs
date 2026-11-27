/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

use pyrefly_python::sys_info::PythonVersion;

use crate::test::util::TestEnv;
use crate::testcase;

testcase!(
    test_literal_both_sides_different,
    r#"
if 1 is 2:  # E: Identity comparison `1 is 2` is always False
    pass
if True is False:  # E: Identity comparison `True is False` is always False
    pass
"#,
);

testcase!(
    test_literal_none_different,
    r#"
if None is 5:  # E: Identity comparison `None is 5` is always False
    pass
if 5 is None:  # E: Identity comparison `5 is None` is always False
    pass
"#,
);

testcase!(
    test_singleton_same_value,
    r#"
# Singletons - always the same object, so warn
if True is True:  # E: Identity comparison `True is True` is always True
    pass
if False is False:  # E: Identity comparison `False is False` is always True
    pass
if True is not True:  # E: Identity comparison `True is not True` is always False
    pass
if False is not False:  # E: Identity comparison `False is not False` is always False
    pass
"#,
);

testcase!(
    test_exhaustive_enum_narrowing,
    r#"
import inspect

def parameter_kind(parameter: inspect.Parameter) -> str:
    if parameter.kind is inspect.Parameter.POSITIONAL_ONLY:
        return "positional-only"
    elif parameter.kind is inspect.Parameter.POSITIONAL_OR_KEYWORD:
        return "positional-or-keyword"
    elif parameter.kind is inspect.Parameter.VAR_POSITIONAL:
        return "args"
    elif parameter.kind is inspect.Parameter.KEYWORD_ONLY:
        return "keyword-only"
    elif parameter.kind is inspect.Parameter.VAR_KEYWORD:
        return "kwargs"
    return "other"
"#,
);

testcase!(
    test_non_singleton_same_value_ok,
    r#"
# Same integer/string literals - no warning (interning varies)
if 1 is 1:  # OK - interning behavior varies
    pass
if "hello" is "hello":  # OK - interning behavior varies
    pass
"#,
);

testcase!(
    test_literal_one_side_ok,
    r#"
def f(x: int):
    if x is 5:  # OK - x is not a literal, only 5 is
        pass
"#,
);

testcase!(
    test_same_class_type_ok,
    r#"
class MyClass:
    pass

def f(obj1: MyClass, obj2: MyClass):
    if obj1 is obj2:  # OK - valid identity check (same type)
        pass
"#,
);

testcase!(
    test_classdef_vs_class_instance,
    r#"
class A: pass
def f(x: int, y: str, z: A):
    if x is A:  # E: Identity comparison between an instance of `int` and class `A` is always False
        pass
    if y is not A:  # E: Identity comparison between an instance of `str` and class `A` is always True
        pass
    if z is A:  # E: Identity comparison between an instance of `A` and class `A` is always False
        pass
"#,
);

testcase!(
    test_classdef_vs_type_ok,
    r#"
class A: pass
def f(x: type):
    if x is A:  # OK - A (a ClassDef) is an instance of type
        pass
"#,
);

testcase!(
    test_classdef_vs_object_ok,
    r#"
class A: pass
def f(x: object):
    if x is A:  # OK - everything is a subtype of object
        pass
"#,
);

testcase!(
    test_classdef_vs_protocol_ok,
    r#"
from collections.abc import Hashable

def f(x: Hashable):
    if x is int:  # OK - class objects may satisfy protocols
        pass
"#,
);

testcase!(
    test_unnecessary_membership,
    r#"
from typing import Any, assert_type

class Equal:
    def __eq__(self, other: object) -> bool:
        return True
class Container:
    def __contains__(self, value: object) -> bool:
        return True

def check(value: int, texts: list[str], names: set[str], keys: dict[str, int],
          fixed: tuple[str, bytes], variable: tuple[str, ...], empty: tuple[()],
          containers: list[str] | set[bytes], mixed: list[str] | list[int],
          unknown: Any, gradual: list[Any], numbers: list[int], equal: Equal,
          custom: Container):
    result = value in texts  # E: Membership test `in` is always False
    assert_type(result, bool)
    if value not in names: pass  # E: Membership test `not in` is always True
    if value in keys: pass  # E: Membership test `in` is always False
    if value in fixed: pass  # E: Membership test `in` is always False
    if value in variable: pass  # E: Membership test `in` is always False
    if value in empty: pass  # E: Membership test `in` is always False
    if value in containers: pass  # E: Membership test `in` is always False
    if value in mixed: pass
    if unknown in texts: pass
    if value in gradual: pass
    if True in numbers: pass
    if 1.0 in numbers: pass
    if equal in texts: pass
    if value in custom: pass
    if "" in "abc": pass
"#,
);

// https://github.com/pyrefly-org/pyrefly/issues/5150
testcase!(
    test_enum_members_with_equal_values,
    r#"
from dataclasses import dataclass
from enum import Enum, IntEnum

@dataclass
class Foo: ...

class X(IntEnum):
    A = 1

class Y(IntEnum):
    B = 1

class E(Enum):
    A = Foo()
    B = Foo()

class F(Enum):
    A = 2.0
    B = 2.0

class G(Enum):
    A = 1
    B = True

class H(Enum):
    A = X.A
    B = Y.B

if E.A is E.B:
    pass
if F.A is F.B:
    pass
if G.A is G.B:
    pass
if H.A is H.B:
    pass
"#,
);

testcase!(
    test_enum_members_with_distinct_values,
    r#"
from enum import Enum

class E(Enum):
    A = 1
    B = 2

if E.A is E.B:  # E: Identity comparison `E.A is E.B` is always False
    pass
if E.A is not E.B:  # E: Identity comparison `E.A is not E.B` is always True
    pass
"#,
);

testcase!(
    test_enum_auto_members,
    r#"
from enum import Enum, auto

class E(Enum):
    X = auto()
    Y = auto()

class F(Enum):
    @staticmethod
    def _generate_next_value_(name: str, start: int, count: int, last_values: list[int]) -> int:
        return 1

    X = auto()
    Y = auto()

if E.X is E.Y:  # E: Identity comparison `E.X is E.Y` is always False
    pass
if F.X is F.Y:
    pass
"#,
);

testcase!(
    test_enum_auto_members_before_3_13,
    TestEnv::new_with_version(PythonVersion::new(3, 12, 0)),
    r#"
from enum import Enum, auto

class E(Enum):
    X = auto()
    S = "s"
    Z = 0
    Y = auto()  # 1 at runtime, the same as `X`

if E.X is E.Y:
    pass
"#,
);
