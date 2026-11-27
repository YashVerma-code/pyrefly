# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# pytest collects tests from this module, so all of it is test code.
from shapes import area


def make_square(side: int) -> tuple[int, int]:
    return side, side


def test_area(side: int) -> None:
    assert area(*make_square(side)) == side * side
