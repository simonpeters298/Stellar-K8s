#!/usr/bin/env python3
# Copyright 2024 Stellar-K8s Contributors
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
"""Tests for the observability contract CI lint (issue #1481)."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

_SPEC = importlib.util.spec_from_file_location(
    "lint_observability_contract",
    Path(__file__).resolve().parents[1] / "ci" / "lint-observability-contract.py",
)
lint = importlib.util.module_from_spec(_SPEC)
assert _SPEC.loader is not None
_SPEC.loader.exec_module(lint)

SCHEMA = (
    Path(__file__).resolve().parents[2]
    / "schemas"
    / "observability"
    / "resource-attributes.v1.json"
)


class LintObservabilityContract(unittest.TestCase):
    def test_repo_scan_passes(self):
        self.assertEqual(lint.main([]), 0)

    def test_unknown_attribute_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src"
            src.mkdir()
            (src / "rogue.rs").write_text(
                'let _ = KeyValue::new("k8s.unknown.attr", "nope");\n',
                encoding="utf-8",
            )
            violations = lint.lint(SCHEMA, [root])
            self.assertTrue(
                any("k8s.unknown.attr" in row for row in violations),
                violations,
            )

    def test_catalogued_attribute_is_allowed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            src = root / "src"
            src.mkdir()
            (src / "ok.rs").write_text(
                'let _ = KeyValue::new("k8s.pod.name", "pod");\n',
                encoding="utf-8",
            )
            self.assertEqual(lint.lint(SCHEMA, [root]), [])


if __name__ == "__main__":
    unittest.main()
