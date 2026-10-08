#!/usr/bin/env python3

# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Check downstream metadata coverage; never build or execute listed oracles."""

import argparse
import fnmatch
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
from typing import Any


class QueueError(ValueError):
    pass


def git(root: Path, *args: str) -> str:
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=False)
    if result.returncode:
        raise QueueError(result.stderr.strip() or "Git command failed")
    return result.stdout.strip()


def require_text(record: object, key: str) -> str:
    if not isinstance(record, dict):
        raise QueueError("metadata record must be an object")
    value = record.get(key)
    if not isinstance(value, str) or not value.strip():
        raise QueueError(f"missing nonempty metadata: {key}")
    return value


def require_list(record: object, key: str) -> list[Any]:
    if not isinstance(record, dict):
        raise QueueError("metadata record must be an object")
    value = record.get(key)
    if not isinstance(value, list):
        raise QueueError(f"missing list metadata: {key}")
    return value


def full_sha(value: object) -> str:
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value):
        raise QueueError(f"expected full commit SHA: {value!r}")
    return value


def safe_path(value: object) -> str:
    if not isinstance(value, str) or not value or "\\" in value:
        raise QueueError(f"invalid repository path: {value!r}")
    path = PurePosixPath(value)
    if path.is_absolute() or ".." in path.parts or value in (".", "./"):
        raise QueueError(f"path leaves repository: {value!r}")
    return value


def require_existing(root: Path, value: str) -> None:
    safe_path(value)
    resolved = (root / value).resolve()
    if not resolved.is_relative_to(root.resolve()) or not resolved.exists():
        raise QueueError(f"absent or external regression path: {value}")


def check(root: Path, ledger: dict[str, Any], revision: str = "HEAD") -> dict[str, Any]:
    root = Path(root)
    if not isinstance(ledger, dict):
        raise QueueError("ledger must be an object")
    if ledger.get("schema_version") != 1:
        raise QueueError("unsupported schema_version")
    base = full_sha(require_text(ledger, "upstream_revision"))
    through = full_sha(require_text(ledger, "coverage_revision"))
    require_text(ledger, "upstream_repository")
    require_text(ledger, "toolchain")
    git(root, "merge-base", "--is-ancestor", base, through)
    git(root, "merge-base", "--is-ancestor", through, revision)
    expected = git(root, "rev-list", "--reverse", "--topo-order", f"{base}..{through}").splitlines()
    entries = require_list(ledger, "entries")
    seen_ids, seen_commits = set(), set()
    declared = []
    oracles = {}
    for oracle in require_list(ledger, "regressions"):
        oracle_id = require_text(oracle, "id")
        if oracle_id in oracles:
            raise QueueError(f"duplicate regression id: {oracle_id}")
        if require_text(oracle, "evidence_status") != "pending":
            raise QueueError("this metadata-only ledger permits no passed evidence claims")
        if require_text(oracle, "kind") not in ("behavioral", "build", "lint", "format"):
            raise QueueError(f"invalid regression kind: {oracle_id}")
        require_text(oracle, "command")
        paths = require_list(oracle, "paths")
        if not paths:
            raise QueueError(f"regression has no source paths: {oracle_id}")
        for path in paths:
            require_existing(root, path)
        oracles[oracle_id] = oracle
    gaps = []
    for entry in entries:
        entry_id = require_text(entry, "id")
        if entry_id in seen_ids:
            raise QueueError(f"duplicate entry id: {entry_id}")
        for field in ("purpose", "retention_reason", "oracle_note"):
            require_text(entry, field)
        if entry.get("upstream_revision") != base:
            raise QueueError(f"entry base differs: {entry_id}")
        if entry.get("upstream_equivalence_status") != "pending":
            raise QueueError(f"unreviewed equivalence claim: {entry_id}")
        if entry.get("evidence_status") != "pending":
            raise QueueError(f"unreviewed evidence claim: {entry_id}")
        if entry.get("classification") not in ("engine", "build", "test", "maintenance"):
            raise QueueError(f"invalid entry classification: {entry_id}")
        for dependency in require_list(entry, "depends_on"):
            if dependency not in seen_ids:
                raise QueueError(f"dependency missing or out of order: {entry_id} -> {dependency}")
        commits = require_list(entry, "commits")
        if not commits:
            raise QueueError(f"entry has no commits: {entry_id}")
        for commit in commits:
            full_sha(commit)
            if commit in seen_commits:
                raise QueueError(f"duplicate covered commit: {commit}")
            seen_commits.add(commit)
            declared.append(commit)
        regression_ids = require_list(entry, "regression_ids")
        if not regression_ids:
            raise QueueError(f"entry has no regression mapping: {entry_id}")
        for oracle_id in regression_ids:
            if oracle_id not in oracles:
                raise QueueError(f"unknown regression: {entry_id} -> {oracle_id}")
        status = entry.get("oracle_status")
        if status not in ("mapped_pending", "behavioral_pending"):
            raise QueueError(f"invalid oracle status: {entry_id}")
        if status == "behavioral_pending":
            gaps.append(entry_id)
        seen_ids.add(entry_id)
    if declared != expected:
        missing = sorted(set(expected) - set(declared))
        extra = sorted(set(declared) - set(expected))
        raise QueueError(f"commit coverage/order mismatch; uncovered={missing}; extra={extra}")

    rules = require_list(ledger, "post_coverage_classifications")
    for rule in rules:
        classification = require_text(rule, "classification")
        allowed = {
            "documentation": {
                "support/downstream/README.md",
                "support/downstream/queue.json",
                "support/storage-defaults.md",
            },
            "queue_tooling": {"support/downstream/check_queue.py", "support/downstream/test_queue.py"},
            "ci": {
                ".github/workflows/**",
                "support/downstream/run_wpt.py",
                "support/downstream/run_wpt_tests.py",
            },
        }
        if classification not in allowed:
            raise QueueError("invalid post-coverage classification")
        require_text(rule, "reason")
        patterns = require_list(rule, "paths")
        if not patterns:
            raise QueueError("empty classification rule")
        for pattern in patterns:
            safe_path(pattern)
            # Avoid a blanket exemption that could conceal source changes.
            if pattern not in allowed[classification]:
                raise QueueError(f"classification pattern is too broad: {pattern}")
    classified = []
    later = git(root, "rev-list", "--reverse", "--topo-order", f"{through}..{revision}").splitlines()
    for commit in later:
        paths = git(root, "diff-tree", "--root", "--no-commit-id", "--name-only", "-r", "-m", commit).splitlines()
        if not paths:
            raise QueueError(f"uncovered empty commit: {commit}")
        classes = set()
        for path in paths:
            matches = [
                rule["classification"]
                for rule in rules
                if any(fnmatch.fnmatchcase(path, pattern) for pattern in rule["paths"])
            ]
            if not matches:
                raise QueueError(f"uncovered post-coverage commit: {commit}: {path}")
            classes.update(matches)
        classified.append({"commit": commit, "classifications": sorted(classes), "paths": sorted(set(paths))})
    return {
        "covered_commits": len(declared),
        "coverage_revision": through,
        "checked_revision": git(root, "rev-parse", f"{revision}^{{commit}}"),
        "post_coverage_commits": classified,
        "behavioral_oracle_gaps": gaps,
        "meaning": "metadata coverage only; no engine acceptance or regression execution",
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--ledger", type=Path)
    parser.add_argument("--revision", default="HEAD")
    args = parser.parse_args()
    try:
        ledger = json.loads((args.ledger or args.root / "support/downstream/queue.json").read_text())
        print(json.dumps(check(args.root, ledger, args.revision), indent=2))
    except (QueueError, OSError, json.JSONDecodeError, TypeError) as error:
        parser.exit(1, f"downstream queue check failed: {error}\n")


if __name__ == "__main__":
    main()
