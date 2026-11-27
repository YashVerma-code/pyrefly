# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

_FAKE_JAX = """
import os
import sys

sys.fake_jax_attempts = getattr(sys, "fake_jax_attempts", 0) + 1
if os.environ.get("FAKE_JAX_FAIL_ONCE") and sys.fake_jax_attempts == 1:
    raise RuntimeError("fake jax initialization failed")

class Array:
    pass

if os.environ.get("FAKE_JAX_UNPATCHABLE"):
    Array = int

fully_imported = True
"""


class JaxPatchTest(unittest.TestCase):
    """Exercise the real import machinery with a small stand-in JAX package."""

    def setUp(self) -> None:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        package = Path(directory.name) / "jax"
        package.mkdir()
        (package / "__init__.py").write_text(textwrap.dedent(_FAKE_JAX))
        spec = importlib.util.find_spec("shape_extensions")
        assert spec is not None and spec.origin is not None
        root = Path(spec.origin).parent.parent
        self.python_path = os.pathsep.join([directory.name, str(root)])

    def run_script(self, script: str, **env: str) -> None:
        result = subprocess.run(
            [sys.executable, "-c", textwrap.dedent(script)],
            env={**os.environ, **env, "PYTHONPATH": self.python_path},
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_annotation_import_does_not_load_jax(self) -> None:
        self.run_script(
            """
            import sys
            import typing
            from shape_extensions import IntVar

            def identity[N: IntVar](value: N) -> N:
                return value

            assert "jax" not in sys.modules
            assert "torch" not in sys.modules
            assert identity.__type_params__[0].__bound__ is IntVar
            assert typing.get_type_hints(identity)["return"] is identity.__type_params__[0]
            """
        )

    def test_jax_imported_first_is_patched(self) -> None:
        self.run_script(
            """
            import jax
            import shape_extensions

            assert jax.fully_imported
            assert jax.Array[2, 3] is jax.Array
            """
        )

    def test_jax_imported_later_is_patched(self) -> None:
        self.run_script(
            """
            import shape_extensions
            import jax

            assert jax.fully_imported
            assert jax.Array[2, 3] is jax.Array
            descriptor = jax.Array.__dict__["__class_getitem__"]
            import jax
            assert jax.Array.__dict__["__class_getitem__"] is descriptor
            """
        )

    def test_existing_subscription_is_preserved(self) -> None:
        self.run_script(
            """
            import jax

            jax.Array.__class_getitem__ = classmethod(lambda cls, args: args)
            descriptor = jax.Array.__dict__["__class_getitem__"]
            import shape_extensions

            assert jax.Array.__dict__["__class_getitem__"] is descriptor
            assert jax.Array[2, 3] == (2, 3)
            """
        )

    def test_annotation_import_without_frameworks(self) -> None:
        self.run_script(
            """
            import sys

            class Unavailable:
                def find_spec(self, fullname, path, target=None):
                    if fullname in ("jax", "torch"):
                        raise ModuleNotFoundError(fullname)

            sys.meta_path.insert(0, Unavailable())
            from shape_extensions import IntVar

            assert IntVar("N").name == "N"
            assert "jax" not in sys.modules
            assert "torch" not in sys.modules
            """
        )

    def test_jax_patch_failure(self) -> None:
        for imported_first in ("0", "1"):
            self.run_script(
                """
                import os
                import sys
                import warnings

                if os.environ["JAX_IMPORTED_FIRST"] == "1":
                    import jax
                with warnings.catch_warnings(record=True) as caught:
                    warnings.simplefilter("always")
                    import shape_extensions
                    import jax

                assert jax.fully_imported
                assert sys.modules["jax"] is jax
                assert len(caught) == 1
                assert issubclass(caught[0].category, RuntimeWarning)
                assert "Could not patch jax" in str(caught[0].message)
                """,
                FAKE_JAX_UNPATCHABLE="1",
                JAX_IMPORTED_FIRST=imported_first,
            )

    def test_jax_initialization_failure_and_retry(self) -> None:
        self.run_script(
            """
            import sys
            import shape_extensions

            assert "jax" not in sys.modules
            try:
                import jax
            except RuntimeError as error:
                assert str(error) == "fake jax initialization failed"
            else:
                raise AssertionError("expected JAX initialization to fail")
            assert "jax" not in sys.modules

            import jax

            assert sys.fake_jax_attempts == 2
            assert jax.Array[2, 3] is jax.Array
            """,
            FAKE_JAX_FAIL_ONCE="1",
        )
