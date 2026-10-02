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
"""Block resource attributes that are not in the published observability schema.

Issue #1481: CI lint so new resource-attribute keys cannot land without a
schema change (versioned JSON Schema is the only catalog).

Usage:
    python3 scripts/ci/lint-observability-contract.py
    python3 scripts/ci/lint-observability-contract.py --schema path --scan path
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_SCHEMA = (
    REPO_ROOT / "schemas" / "observability" / "resource-attributes.v1.json"
)

# Resource-attribute catalogs we accept as declarations.
RESOURCE_KEY_RE = re.compile(
    r"""(?:KeyValue::new\s*\(|resource_attr\s*\(|attributes\[)"""
    r"""\s*[\"']"""
    r"""((?:k8s|service|stellar\.observability|host|deployment|telemetry\.sdk)"""
    r"""\.[A-Za-z0-9._-]+)[\"']"""
)

# Span / event attributes that share a prefix but are not resource identity.
SPAN_ALLOWLIST = {
    "service.name",  # also a resource key
}

SCAN_GLOBS = (
    "src/**/*.rs",
    "tests/**/*.rs",
    "charts/stellar-operator/templates/*.yaml",
    "scripts/generate-observability-instrumentation.py",
)


def load_allowed(schema_path: Path) -> set[str]:
    data = json.loads(schema_path.read_text(encoding="utf-8"))
    props = data.get("properties") or {}
    return set(props.keys())


def scan_file(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8", errors="replace")
    return RESOURCE_KEY_RE.findall(text)


def lint(schema_path: Path, roots: list[Path]) -> list[str]:
    allowed = load_allowed(schema_path)
    violations: list[str] = []
    files: list[Path] = []
    for root in roots:
        if root.is_file():
            files.append(root)
            continue
        for glob in SCAN_GLOBS:
            files.extend(root.glob(glob))
            # pathlib.glob('**') can miss a single-segment path on some versions
            files.extend(root.rglob("*.rs"))
            files.extend(root.rglob("*.yaml"))
            files.extend(root.rglob("*.py"))
    seen: set[Path] = set()
    for path in files:
        resolved = path.resolve()
        if resolved in seen:
            continue
        seen.add(resolved)
        if "resource-attributes.v1.json" in path.name:
            continue
        if any(part in {"target", ".git", "node_modules", "vendor"} for part in path.parts):
            continue
        if path.name == "test_lint_observability_contract.py":
            continue
        for key in scan_file(path):
            if key in allowed or key in SPAN_ALLOWLIST:
                continue
            violations.append(f"{path}: unknown resource attribute '{key}'")
    return violations


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--schema", type=Path, default=DEFAULT_SCHEMA)
    parser.add_argument(
        "--scan",
        type=Path,
        action="append",
        default=None,
        help="Root to scan (repeatable). Defaults to the repository root.",
    )
    args = parser.parse_args(argv)
    roots = args.scan or [REPO_ROOT]
    if not args.schema.is_file():
        print(f"ERROR: schema not found: {args.schema}", file=sys.stderr)
        return 2
    violations = lint(args.schema, roots)
    if violations:
        print("Observability contract lint failed:", file=sys.stderr)
        for row in violations:
            print(f"  {row}", file=sys.stderr)
        print(
            "Add the key to schemas/observability/resource-attributes.v1.json "
            "(and bump contractVersion if needed).",
            file=sys.stderr,
        )
        return 1
    print(
        f"Observability contract lint passed "
        f"({len(load_allowed(args.schema))} catalog keys)."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
