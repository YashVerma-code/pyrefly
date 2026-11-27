# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# pytest does not collect tests from this module, so only its `unittest.TestCase`
# subclasses are test code.
import unittest

from shapes import area


class ShapesCase(unittest.TestCase):
    def assert_area(self, width: int, height: int, expected: int) -> None:
        self.assertEqual(area(width, height), expected)


class TestNotACase:
    def test_nothing(self) -> None:
        pass


def shape_names() -> list[str]:
    return ["square"]
