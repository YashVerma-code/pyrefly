# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the MIT license found in the
# LICENSE file in the root directory of this source tree.

# pytest loads fixtures from this module, so all of it is test code.
import pytest


@pytest.fixture
def side() -> int:
    return 3
