# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# The pytest configuration names this module, so all of it is test code.
from test_shapes import area


def check_area() -> None:
    assert area(2, 3) == 6
