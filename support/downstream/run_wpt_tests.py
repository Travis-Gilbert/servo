# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Synthetic verifier/refusal tests; these do not execute WPT or build Servo."""

import copy
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import urllib.request

import run_wpt as runner


class ProvenanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.sha = "a" * 40
        self.ids = {
            "ENGINE_SHA": self.sha,
            "BUILD_RUN_ID": "12",
            "ARTIFACT_ID": "34",
            "ARTIFACT_DIGEST": "sha256:" + "d" * 64,
        }
        self.run = {
            "id": 12,
            "head_sha": self.sha,
            "repository": {"id": runner.REPOSITORY_ID},
            "head_repository": {"id": runner.REPOSITORY_ID},
            "name": "Main",
            "path": ".github/workflows/main.yml",
            "status": "completed",
            "conclusion": "success",
        }
        self.artifact = {
            "id": 34,
            "name": runner.ARTIFACT_NAME,
            "expired": False,
            "size_in_bytes": 100,
            "digest": self.ids["ARTIFACT_DIGEST"],
            "created_at": "2026-10-08T02:00:01Z",
            "workflow_run": {
                "id": 12,
                "head_sha": self.sha,
                "repository_id": runner.REPOSITORY_ID,
                "head_repository_id": runner.REPOSITORY_ID,
            },
        }
        self.job = {
            "id": 56,
            "name": "Linux / Linux Build (x86_64) [unique]",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-10-08T02:00:00Z",
            "completed_at": "2026-10-08T02:00:02Z",
        }

    def validate(self) -> dict:
        return runner.validate_metadata(self.ids, self.run, self.artifact, [self.job])

    def test_matching_identities_pass(self) -> None:
        self.assertEqual(self.validate()["id"], 56)

    def test_wrong_run_sha_repo_workflow_or_status_refused(self) -> None:
        original = copy.deepcopy(self.run)
        for key, value in (
            ("id", 13),
            ("head_sha", "b" * 40),
            ("repository", {"id": 1}),
            ("head_repository", {"id": 1}),
            ("name", "Other"),
            ("path", "other.yml"),
            ("status", "in_progress"),
            ("conclusion", "failure"),
        ):
            with self.subTest(key=key):
                self.run = copy.deepcopy(original)
                self.run[key] = value
                with self.assertRaises(ValueError):
                    self.validate()

    def test_wrong_digest_artifact_run_repo_expiry_or_attempt_refused(self) -> None:
        original = copy.deepcopy(self.artifact)
        mutations = [
            ("digest", "sha256:" + "e" * 64),
            ("id", 35),
            ("expired", True),
            ("name", "debug-binary-linux"),
            ("size_in_bytes", 0),
            ("created_at", "2026-10-07T02:00:00Z"),
        ]
        for key, value in mutations:
            with self.subTest(key=key):
                self.artifact = copy.deepcopy(original)
                self.artifact[key] = value
                with self.assertRaises(ValueError):
                    self.validate()
        for key, value in (("id", 13), ("head_sha", "b" * 40), ("repository_id", 1), ("head_repository_id", 1)):
            with self.subTest(linked_key=key):
                self.artifact = copy.deepcopy(original)
                self.artifact["workflow_run"][key] = value
                with self.assertRaises(ValueError):
                    self.validate()

    def test_missing_duplicate_or_failed_build_refused(self) -> None:
        for jobs in ([], [self.job, self.job], [dict(self.job, conclusion="failure")]):
            with self.subTest(jobs=jobs), self.assertRaises(ValueError):
                runner.validate_metadata(self.ids, self.run, self.artifact, jobs)

    def test_shell_strings_and_abbreviated_revision_refused(self) -> None:
        env = dict(self.ids, GITHUB_REPOSITORY=runner.REPOSITORY)
        for key, value in (
            ("ENGINE_SHA", "main"),
            ("BUILD_RUN_ID", "12; echo bad"),
            ("ARTIFACT_ID", "-1"),
            ("ARTIFACT_DIGEST", "d" * 64),
            ("GITHUB_REPOSITORY", "other/servo"),
        ):
            with self.subTest(key=key), patch.dict("os.environ", dict(env, **{key: value}), clear=True):
                with self.assertRaises(ValueError):
                    runner.identities()

    def test_merge_tree_must_equal_head_and_have_head_parent(self) -> None:
        merged = "b" * 40
        log = "git checkout --progress --force " + merged + "\n"
        head = {"sha": self.sha, "tree": {"sha": "c" * 40}}
        build = {"sha": merged, "tree": {"sha": "c" * 40}, "parents": [{"sha": self.sha}]}
        self.assertEqual(runner.validate_build_source(self.sha, log, head, build), merged)
        for wrong in (dict(build, tree={"sha": "d" * 40}), dict(build, parents=[]), dict(build, sha=self.sha)):
            with self.subTest(build=wrong), self.assertRaises(ValueError):
                runner.validate_build_source(self.sha, log, head, wrong)
        with self.assertRaises(ValueError):
            runner.validate_build_source(
                self.sha, log + "git checkout --progress --force " + self.sha + "\n", head, build
            )

    def test_signed_log_redirect_does_not_receive_github_token(self) -> None:
        request = urllib.request.Request(
            "https://api.github.com/example", headers={"Authorization": "Bearer synthetic"}
        )
        redirected = runner.SafeRedirect().redirect_request(request, None, 302, "Found", {}, "https://example.com/log")
        self.assertIsNone(redirected.get_header("Authorization"))
        with self.assertRaises(ValueError):
            runner.SafeRedirect().redirect_request(request, None, 302, "Found", {}, "http://example.com/log")

    def test_binary_version_must_match_the_whole_available_prefix(self) -> None:
        runner.validate_binary_version("Version: Servo 0.5.0-" + self.sha[:7], self.sha)
        runner.validate_binary_version("Version: Servo 0.5.0-" + self.sha[:11], self.sha)
        for wrong in ("Version: Servo 0.5.0-nogit", "Version: Servo 0.5.0-" + self.sha[:7] + "b", "wrong"):
            with self.subTest(version=wrong), self.assertRaises(ValueError):
                runner.validate_binary_version(wrong, self.sha)


class RosterAndExecutionTests(unittest.TestCase):
    def test_streaming_sha256_matches_known_digest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            fixture = Path(temporary) / "data"
            fixture.write_bytes(b"abc")
            self.assertEqual(runner.sha256(fixture), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")

    def test_staged_expectation_change_is_detected(self) -> None:
        # An unstaged-only diff would incorrectly accept this candidate.
        # This isolated fixture never changes the engine checkout's Git index.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            subprocess.run(["git", "init", "--quiet"], cwd=root, check=True)
            metadata = root / "tests/wpt/meta/regression.ini"
            metadata.parent.mkdir(parents=True)
            metadata.write_text("expected: PASS\n")
            subprocess.run(["git", "add", "tests"], cwd=root, check=True)
            subprocess.run(
                [
                    "git",
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "Initial fixture",
                ],
                cwd=root,
                check=True,
            )
            self.assertTrue(runner.metadata_matches_head(root))
            metadata.write_text("expected: FAIL\n")
            subprocess.run(["git", "add", "tests"], cwd=root, check=True)
            self.assertEqual(subprocess.check_output(["git", "diff", "--", "tests"], cwd=root), b"")
            self.assertFalse(runner.metadata_matches_head(root))

    def test_public_candidate_roster_has_exact_count_and_hash(self) -> None:
        root = Path(__file__).resolve().parents[2]
        urls, counts = runner.select_roster(root)
        self.assertEqual(len(urls), 968)
        self.assertEqual(list(counts.values()), [817, 54, 85, 8, 1, 1, 1, 1])
        with patch.object(runner, "ROSTER_SHA256", "0" * 64), self.assertRaises(ValueError):
            runner.select_roster(root)
        with patch.object(runner, "MAX_URLS", 967), self.assertRaises(ValueError):
            runner.select_roster(root)

    def test_missing_archive_or_executable_refused(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaises(ValueError):
                runner.extract_package(root / "missing", root / "out")
            archive = root / "package.tar.gz"
            with tarfile.open(archive, "w:gz"):
                pass
            with self.assertRaisesRegex(ValueError, "Executable servoshell missing"):
                runner.extract_package(archive, root / "out")

    def test_tar_traversal_and_links_refused(self) -> None:
        for name, symlink in (("../escape", False), ("/absolute", False), ("servo/link", True)):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "package.tar.gz"
                with tarfile.open(archive, "w:gz") as tar:
                    member = tarfile.TarInfo(name)
                    member.size = 1
                    if symlink:
                        member.type = tarfile.SYMTYPE
                        member.linkname = "../../escape"
                        member.size = 0
                    tar.addfile(member, io.BytesIO(b"x"))
                with self.assertRaises(ValueError):
                    runner.extract_package(archive, root / "out")

    def test_safe_package_keeps_binary_executable_and_resources(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "package.tar.gz"
            with tarfile.open(archive, "w:gz") as tar:
                directory = tarfile.TarInfo("servo/resources")
                directory.type = tarfile.DIRTYPE
                tar.addfile(directory)
                binary = tarfile.TarInfo("servo/servoshell")
                binary.mode = 0o755
                binary.size = 7
                tar.addfile(binary, io.BytesIO(b"fixture"))
            binary = runner.extract_package(archive, root / "out")
            self.assertEqual(binary.read_bytes(), b"fixture")

    def test_command_can_only_run_capped_selected_wpt(self) -> None:
        command = runner.wpt_command(Path("/engine"), Path("/binary"), Path("/evidence"))
        self.assertEqual(command[1], "test-wpt")
        self.assertIn("--include-file", command)
        self.assertIn("--no-default-test-types", command)
        for forbidden in (
            "build",
            "package",
            "install",
            "--always-succeed",
            "--update-expectations",
            "--manifest-update",
        ):
            self.assertNotIn(forbidden, command)
        self.assertEqual(command[command.index("--processes") + 1], "2")

    def test_raw_results_distinguish_expected_failure_and_incomplete_execution(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            raw = Path(temporary) / "raw.jsonl"
            rows = [
                {"action": "test_end", "test": "/IndexedDB/a", "status": "ERROR", "expected": "ERROR"},
                {
                    "action": "test_status",
                    "test": "/IndexedDB/a",
                    "status": "FAIL",
                    "expected": "PASS",
                    "known_intermittent": ["FAIL"],
                },
                {"action": "suite_end"},
            ]
            raw.write_text("\n".join(json.dumps(row) for row in rows) + "\n")
            result = runner.summarize(raw, ["/IndexedDB/a", "/webstorage/b"])
            self.assertEqual(result["unexpected_count"], 0)
            self.assertEqual(result["statuses"], {"ERROR": 1})
            self.assertEqual(result["missing_urls"], ["/webstorage/b"])
            self.assertEqual(result["executed_groups"]["webstorage"], 0)
            self.assertEqual(result["functional_subtest_pass_count"], 0)
            self.assertEqual(result["actual_failure_count"], 1)
            self.assertEqual(runner.gate_result(0, result, True), "failed")
            with self.assertRaises(ValueError):
                runner.summarize(raw, ["/different"])

    def test_expectation_match_is_not_a_functional_pass_and_drift_is_failure(self) -> None:
        summary = {
            "suite_end": True,
            "missing_urls": [],
            "executed_groups": dict.fromkeys(runner.GROUPS, 1),
            "unexpected_count": 0,
            "functional_subtest_pass_count": 0,
            "actual_failure_count": 968,
        }
        self.assertEqual(runner.gate_result(0, summary, True), "expectation_match")
        self.assertEqual(summary["functional_subtest_pass_count"], 0)
        self.assertEqual(runner.gate_result(0, summary, False), "failed")
        self.assertEqual(runner.gate_result(124, summary, True), "failed")


if __name__ == "__main__":
    unittest.main()
