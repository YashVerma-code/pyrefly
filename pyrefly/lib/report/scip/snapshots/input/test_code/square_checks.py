# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# This test case derives from `unittest.TestCase` through a base in another module.
from shapes_cases import ShapesCase


class SquareCase(ShapesCase):
    def test_square(self) -> None:
        self.assert_area(2, 2, 4)


def unused() -> None:
    pass
