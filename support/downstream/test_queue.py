# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Metadata failures tested against real, disposable Git histories."""

import copy
from pathlib import Path
import subprocess
import tempfile
import unittest
from typing import Any

from check_queue import QueueError, check


class QueueTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.run_git("init", "-q")
        self.run_git("config", "user.name", "Queue Test")
        self.run_git("config", "user.email", "queue@example.invalid")
        self.write("tests/regression.rs", "independent fixture\n")
        self.base = self.commit("base")
        self.write("components/engine.rs", "retained behavior\n")
        self.first = self.commit("first engine patch")
        self.write("components/engine.rs", "second retained behavior\n")
        self.second = self.commit("second engine patch")
        self.ledger = {
            "schema_version": 1,
            "upstream_repository": "https://example.invalid/upstream",
            "upstream_revision": self.base,
            "coverage_revision": self.second,
            "toolchain": "1.95.0",
            "regressions": [
                {
                    "id": "behavior",
                    "kind": "behavioral",
                    "command": "test command",
                    "paths": ["tests/regression.rs"],
                    "evidence_status": "pending",
                }
            ],
            "post_coverage_classifications": [
                {
                    "classification": "documentation",
                    "paths": ["support/downstream/README.md"],
                    "reason": "Explicit follow-up metadata only",
                }
            ],
            "entries": [self.entry("first", self.first, []), self.entry("second", self.second, ["first"])],
        }

    def run_git(self, *args: str) -> str:
        return subprocess.check_output(["git", "-C", str(self.root), *args], text=True).strip()

    def write(self, path: str, text: str) -> None:
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message: str) -> str:
        self.run_git("add", ".")
        self.run_git("commit", "-qm", message)
        return self.run_git("rev-parse", "HEAD")

    def entry(self, entry_id: str, commit: str, dependencies: list[str]) -> dict[str, Any]:
        return {
            "id": entry_id,
            "commits": [commit],
            "upstream_revision": self.base,
            "purpose": "Retain independently specified engine behavior",
            "classification": "engine",
            "depends_on": dependencies,
            "retention_reason": "Upstream equivalence has not been verified",
            "regression_ids": ["behavior"],
            "oracle_status": "mapped_pending",
            "oracle_note": "Mapping is not execution",
            "upstream_equivalence_status": "pending",
            "evidence_status": "pending",
        }

    def rejects(self, ledger: dict[str, Any], message: str) -> None:
        with self.assertRaisesRegex(QueueError, message):
            check(self.root, ledger)

    def test_complete_ordered_history_is_metadata_only(self) -> None:
        result = check(self.root, self.ledger)
        self.assertEqual(result["covered_commits"], 2)
        self.assertEqual(result["checked_revision"], self.second)
        self.assertIn("no engine acceptance", result["meaning"])

    def test_duplicate_commit_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"][1]["commits"] = [self.first]
        self.rejects(ledger, "duplicate covered commit")

    def test_uncovered_commit_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"].pop()
        self.rejects(ledger, "uncovered=.*" + self.second)

    def test_reordered_commits_are_rejected_even_without_dependencies(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"].reverse()
        for entry in ledger["entries"]:
            entry["depends_on"] = []
        self.rejects(ledger, "coverage/order mismatch")

    def test_forward_dependency_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"][0]["depends_on"] = ["second"]
        self.rejects(ledger, "dependency missing or out of order")

    def test_missing_retention_metadata_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        del ledger["entries"][0]["retention_reason"]
        self.rejects(ledger, "retention_reason")

    def test_missing_regression_file_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["regressions"][0]["paths"] = ["tests/nonexistent.rs"]
        self.rejects(ledger, "absent or external regression path")

    def test_external_path_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["regressions"][0]["paths"] = ["../outside"]
        self.rejects(ledger, "path leaves repository")

    def test_unknown_regression_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"][0]["regression_ids"] = ["invented-oracle"]
        self.rejects(ledger, "unknown regression")

    def test_equivalence_or_pass_claim_requires_a_separate_protocol(self) -> None:
        for field in ("evidence_status", "upstream_equivalence_status"):
            ledger = copy.deepcopy(self.ledger)
            ledger["entries"][0][field] = "passed"
            self.rejects(ledger, "unreviewed")

    def test_unmapped_behavior_remains_visible_after_success(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"][0]["oracle_status"] = "behavioral_pending"
        self.assertEqual(check(self.root, ledger)["behavioral_oracle_gaps"], ["first"])

    def test_explicit_documentation_classification_is_reported(self) -> None:
        self.write("support/downstream/README.md", "new documentation\n")
        documentation = self.commit("queue documentation")
        result = check(self.root, self.ledger)
        self.assertEqual(
            result["post_coverage_commits"],
            [
                {
                    "commit": documentation,
                    "classifications": ["documentation"],
                    "paths": ["support/downstream/README.md"],
                }
            ],
        )

    def test_uncovered_later_engine_change_is_rejected(self) -> None:
        self.write("components/engine.rs", "unrecorded behavior\n")
        self.commit("uncovered engine patch")
        self.rejects(self.ledger, "uncovered post-coverage commit.*components/engine.rs")

    def test_ci_workflow_and_runner_are_explicitly_classified(self) -> None:
        self.write(".github/workflows/downstream-wpt.yml", "workflow fixture\n")
        self.write("support/downstream/run_wpt.py", "runner fixture\n")
        self.write("support/downstream/run_wpt_tests.py", "test fixture\n")
        ci_commit = self.commit("CI tooling")
        ledger = copy.deepcopy(self.ledger)
        ledger["post_coverage_classifications"].append(
            {
                "classification": "ci",
                "paths": [
                    ".github/workflows/**",
                    "support/downstream/run_wpt.py",
                    "support/downstream/run_wpt_tests.py",
                ],
                "reason": "Explicit runner/workflow metadata classification",
            }
        )
        result = check(self.root, ledger)
        self.assertEqual(result["post_coverage_commits"][0]["commit"], ci_commit)
        self.assertEqual(result["post_coverage_commits"][0]["classifications"], ["ci"])

    def test_ci_cannot_hide_a_mixed_engine_commit(self) -> None:
        self.write(".github/workflows/downstream-wpt.yml", "workflow fixture\n")
        self.write("components/engine.rs", "unrecorded engine behavior\n")
        self.commit("mixed CI and engine patch")
        ledger = copy.deepcopy(self.ledger)
        ledger["post_coverage_classifications"].append(
            {
                "classification": "ci",
                "paths": [".github/workflows/**"],
                "reason": "Explicit CI-only rule",
            }
        )
        self.rejects(ledger, "uncovered post-coverage commit.*components/engine.rs")

    def test_documentation_cannot_hide_a_mixed_engine_commit(self) -> None:
        self.write("support/downstream/README.md", "documentation\n")
        self.write("components/engine.rs", "hidden behavior\n")
        self.commit("mixed patch")
        self.rejects(self.ledger, "uncovered post-coverage commit.*components/engine.rs")

    def test_blanket_classification_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["post_coverage_classifications"][0]["paths"] = ["**"]
        self.rejects(ledger, "classification pattern is too broad")

    def test_short_commit_sha_is_rejected(self) -> None:
        ledger = copy.deepcopy(self.ledger)
        ledger["entries"][0]["commits"] = [self.first[:10]]
        self.rejects(ledger, "expected full commit SHA")

    def test_new_engine_patch_can_extend_explicit_coverage(self) -> None:
        self.write("components/engine.rs", "third retained behavior\n")
        third = self.commit("third engine patch")
        ledger = copy.deepcopy(self.ledger)
        ledger["coverage_revision"] = third
        ledger["entries"].append(self.entry("third", third, ["second"]))
        self.assertEqual(check(self.root, ledger)["covered_commits"], 3)


if __name__ == "__main__":
    unittest.main()
