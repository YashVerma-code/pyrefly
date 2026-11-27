/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Method descriptor binding and partial application of ordinary functions.

use pyrefly_python::sys_info::PythonVersion;

use crate::functools_testcase;
use crate::test::util::TestEnv;
use crate::testcase;

functools_testcase!(
    test_partialmethod_generic_class,
    r#"
from functools import partialmethod
from typing import assert_type
class C[T]:
    def method(self, value: T) -> T: ...
    specialized = partialmethod(method)
assert_type(C[int]().specialized(1), int)
assert_type(C[int].specialized(C[int](), 1), int)
assert_type(C.specialized(C[int](), 1), int)
"#,
);

functools_testcase!(
    test_partialmethod_self_receiver,
    r#"
from functools import partialmethod
from typing import Self, assert_type
class C:
    def method(self, value: int) -> str: ...
    specialized = partialmethod(method)
    def use(self) -> str:
        return self.specialized(1)
    def clone(self) -> Self: ...
    cloned = partialmethod(clone)
    def use_clone(self) -> Self:
        return self.cloned()
class Child(C): ...
assert_type(Child().cloned(), Child)
"#,
);

// Signature-local receiver generics deliberately use the stub's gradual return.
functools_testcase!(
    test_partialmethod_generic_receiver,
    r#"
from functools import partialmethod
from typing import reveal_type
def method[T](self: T, value: int) -> T: ...
class C:
    specialized = partialmethod(method, 1)
reveal_type(C().specialized())  # E: revealed type: Unknown
"#,
);

functools_testcase!(
    test_partialmethod_get,
    r#"
from functools import partialmethod
from typing import reveal_type
class C:
    def method(self, value: int) -> str: ...
p = partialmethod(C.method)
reveal_type(p.__get__(None, C))  # E: revealed type: (self: C, /, value: int) -> str
reveal_type(p.__get__(C(), C))  # E: revealed type: (value: int) -> str
p.__get__(None, C)(self=C(), value=1)  # E: Expected argument `self` to be positional
p.__get__(C(), C)("bad")  # E: Argument `Literal['bad']` is not assignable to parameter `value` with type `int`
"#,
);

functools_testcase!(
    test_partialmethod_nested,
    r#"
from functools import partialmethod
from typing import assert_type
class C:
    def method(self, value: int, text: str, *, flag: bool = False) -> str: ...
    first = partialmethod(method, 1)
    second = partialmethod(first, "ok", flag=True)
assert_type(C().second(), str)
C().second(flag=1)  # E: Argument `Literal[1]` is not assignable to parameter `flag` with type `bool`
"#,
);

functools_testcase!(
    test_partialmethod_signatures,
    r#"
from functools import partialmethod
from typing import reveal_type
class C:
    def method(self, value: int, text: str) -> str: ...
    specialized = partialmethod(method, 1)
reveal_type(C.specialized)  # E: revealed type: (self: C, /, text: str) -> str
reveal_type(C().specialized)  # E: revealed type: (text: str) -> str
C.specialized(C(), "ok")
C().specialized("ok")
C().specialized(1)  # E: Argument `Literal[1]` is not assignable to parameter `text` with type `str`
C().specialized()  # E: Missing argument `text`
"#,
);

functools_testcase!(
    test_partialmethod_bound_arguments,
    r#"
from functools import partialmethod
class C:
    def method(self, value: int, text: str) -> str: ...
    valid = partialmethod(method, 1)
    invalid = partialmethod(method, "bad")  # E: Argument `Literal['bad']` is not assignable to parameter `value` with type `int`
    too_many = partialmethod(method, 1, "ok", 3)  # E: Expected 2 positional arguments, got 3
"#,
);

functools_testcase!(
    test_partialmethod_keywords,
    r#"
from functools import partialmethod
class C:
    def method(self, value: int, text: str, *, flag: bool = False) -> str: ...
    specialized = partialmethod(method, text="bound", flag=True)
    invalid = partialmethod(method, flag=1)  # E: Argument `Literal[1]` is not assignable to parameter `flag` with type `bool`
C().specialized(1)
C().specialized(1, text="override", flag=False)
C().specialized(1, text=2)  # E: Argument `Literal[2]` is not assignable to parameter `text` with type `str`
C().specialized(1, "positional")  # E: Expected 1 positional argument, got 2
"#,
);

// https://github.com/pyrefly-org/pyrefly/issues/5138
functools_testcase!(
    test_partialmethod_class_assignment,
    r#"
from functools import partialmethod
from typing import assert_type
def pprint(arg: object, /, *, options: dict) -> str:
    return "dummy"
class Bar: ...
Bar.__repr__ = partialmethod(pprint, options={})
assert_type(repr(Bar()), str)
"#,
);

functools_testcase!(
    test_partialmethod_raw_descriptor,
    r#"
from functools import partialmethod
from typing import Any, Callable, assert_type
def method(self: object, value: int) -> str: ...
p = partialmethod(method, 1)
assert_type(p, partialmethod[str])
p(object())  # E: Expected a callable, got `partialmethod[str]`
f: Callable[[object], str] = p  # E: `partialmethod[str]` is not assignable to `(object) -> str`
assert_type(p.args, tuple[Any, ...])
assert_type(p.keywords, dict[str, Any])
"#,
);

functools_testcase!(
    test_partialmethod_inheritance,
    r#"
from functools import partialmethod
from typing import assert_type
class Base:
    def method(self, value: int, *, flag: bool = False) -> str: ...
    specialized = partialmethod(method, flag=True)
class Child(Base): ...
assert_type(Child().specialized(1), str)
Child().specialized("bad")  # E: Argument `Literal['bad']` is not assignable to parameter `value` with type `int`
"#,
);

functools_testcase!(
    test_partialmethod_generic,
    r#"
from functools import partialmethod
from typing import assert_type
class C:
    def method[T](self, value: T, count: int) -> list[T]: ...
    specialized = partialmethod(method, count=1)
assert_type(C().specialized(1), list[int])
assert_type(C().specialized("ok"), list[str])
"#,
);

functools_testcase!(
    test_partialmethod_descriptor_fallback,
    r#"
from functools import partialmethod
from typing import assert_type
class Descriptor:
    def __get__(self, obj: object, owner: type | None = None) -> object: ...
class C:
    arbitrary = partialmethod(Descriptor())
    @staticmethod
    def static(value: int) -> str: ...
    @classmethod
    def class_method(cls, value: int) -> str: ...
    static_partial = partialmethod(static, 1)
    class_partial = partialmethod(class_method, 1)
assert_type(C().static_partial(), str)
C().class_partial()
C().arbitrary()
"#,
);

functools_testcase!(
    test_partialmethod_overloads,
    r#"
from functools import partialmethod
from typing import reveal_type, overload
class C:
    @overload
    def method(self, value: int) -> int: ...
    @overload
    def method(self, value: str) -> str: ...
    def method(self, value: int | str) -> int | str: ...
    specialized = partialmethod(method)
reveal_type(C().specialized(1))  # E: revealed type: int
reveal_type(C().specialized("ok"))  # E: revealed type: str
"#,
);

testcase!(
    test_partialmethod_python313,
    TestEnv::new_with_version(PythonVersion::new(3, 13, 0)),
    r#"
from functools import partialmethod
from typing import assert_type
class C:
    def method(self, value: int) -> str: ...
    specialized = partialmethod(method)
assert_type(C().specialized(1), str)
C().specialized("bad")  # E: Argument `Literal['bad']` is not assignable to parameter `value` with type `int`
"#,
);

functools_testcase!(
    test_partialmethod_missing_target,
    r#"
from functools import partialmethod
partialmethod()  # E: No matching overload found
partialmethod(*())  # E: No matching overload found
"#,
);

testcase!(
    test_partialmethod_synthetic_receiver,
    TestEnv::new()
        .enable_explicit_any_use_error()
        .enable_unknown_argument_type_error(),
    r#"
from functools import partialmethod
from typing import Any
class C:
    def method(self, value: Any, extra: Any) -> Any:
        return value
    specialized = partialmethod(method)
"#,
);

testcase!(
    test_partialmethod_python314,
    TestEnv::new_with_version(PythonVersion::new(3, 14, 0)),
    r#"
from functools import partialmethod
from typing import assert_type
class C:
    def method(self, value: int) -> str: ...
    specialized = partialmethod(method)
assert_type(C().specialized(1), str)
C().specialized("bad")  # E: Argument `Literal['bad']` is not assignable to parameter `value` with type `int`
"#,
);
