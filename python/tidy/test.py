# Copyright 2013 The Servo Project Developers. See the COPYRIGHT
# file at the top-level directory of this distribution.
#
# Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
# http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
# <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

import logging
import contextlib
import io
import os
import subprocess
import tempfile
from collections.abc import Iterable
import unittest
from unittest.mock import patch

from . import tidy


BASE_PATH = "python/tidy/tests/"


def test_file_path(name):
    return os.path.join(BASE_PATH, name)


def iterFile(name):
    return iter([test_file_path(name)])


class CheckTidiness(unittest.TestCase):
    @contextlib.contextmanager
    def coauthors_repository(self):
        with tempfile.TemporaryDirectory(prefix="servo-coauthors-") as directory:
            with (
                contextlib.chdir(directory),
                patch.dict(os.environ, {"GITHUB_EVENT_NAME": "", "CI_PULL_REQUEST_BODY": ""}),
                patch.dict(tidy.config, {"coauthors-history-base": "", "disallowed-coauthors": ["llm@example.com"]}),
            ):
                self.coauthors_git("init", "--quiet")
                self.coauthors_git("config", "user.name", "Contributor")
                self.coauthors_git("config", "user.email", "contributor@example.net")
                yield

    def coauthors_git(self, *args):
        return subprocess.check_output(["git", *args], text=True, stderr=subprocess.STDOUT).strip()

    def coauthors_commit(self, message):
        self.coauthors_git(
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            message,
        )
        return self.coauthors_git("rev-parse", "HEAD")

    def coauthors_result(self):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            result = tidy.run_coauthors_check()
        return result, output.getvalue()

    def test_coauthors_history_default_preserves_upstream_policy(self):
        with self.coauthors_repository():
            inherited = self.coauthors_commit("Historical change\n\nCo-authored-by: LLM <llm@example.com>")
            self.coauthors_commit("New change")
            result, output = self.coauthors_result()
            self.assertEqual(result, 1)
            self.assertIn(inherited, output)

    def test_coauthors_history_base_preserves_inherited_commits(self):
        with self.coauthors_repository():
            inherited = self.coauthors_commit("Historical change\n\nCo-authored-by: LLM <llm@example.com>")
            self.coauthors_commit("New change")
            tidy.config["coauthors-history-base"] = inherited
            self.assertEqual(self.coauthors_result()[0], 0)
            self.assertIn(
                "Co-authored-by: LLM <llm@example.com>", self.coauthors_git("show", "-s", "--format=%B", inherited)
            )

    def test_coauthors_history_base_rejects_new_disallowed_commit(self):
        with self.coauthors_repository():
            tidy.config["coauthors-history-base"] = self.coauthors_commit("Historical change")
            new_commit = self.coauthors_commit("New change\n\nAssisted-by: LLM <llm@example.com>")
            result, output = self.coauthors_result()
            self.assertEqual(result, 1)
            self.assertIn(new_commit, output)

    def test_coauthors_history_base_rejects_disallowed_pr_body(self):
        with self.coauthors_repository():
            tidy.config["coauthors-history-base"] = self.coauthors_commit("Historical change")
            with patch.dict(
                os.environ,
                {"GITHUB_EVENT_NAME": "pull_request", "CI_PULL_REQUEST_BODY": "Co-authored-by: LLM <llm@example.com>"},
            ):
                result, output = self.coauthors_result()
            self.assertEqual(result, 1)
            self.assertIn("Pull request body has", output)

    def test_coauthors_history_base_refuses_bad_or_missing_sha(self):
        with self.coauthors_repository():
            self.coauthors_commit("Historical change")
            for base in ("HEAD", "a" * 39, "a" * 41, "g" * 40, "0" * 40, None, 0):
                with self.subTest(base=base):
                    tidy.config["coauthors-history-base"] = base
                    self.assertEqual(self.coauthors_result()[0], 1)

    def test_coauthors_history_base_refuses_nonancestor(self):
        with self.coauthors_repository():
            base = self.coauthors_commit("Historical change")
            sibling = self.coauthors_commit("Sibling change")
            self.coauthors_git("checkout", "--quiet", "--detach", base)
            self.coauthors_commit("Current change")
            tidy.config["coauthors-history-base"] = sibling
            self.assertEqual(self.coauthors_result()[0], 1)

    def test_coauthors_history_base_refuses_annotated_tag_object(self):
        with self.coauthors_repository():
            self.coauthors_commit("Historical change")
            self.coauthors_git("-c", "tag.gpgsign=false", "tag", "-a", "baseline", "-m", "Historical tag")
            tidy.config["coauthors-history-base"] = self.coauthors_git("rev-parse", "refs/tags/baseline")
            self.assertEqual(self.coauthors_result()[0], 1)

    def assertNoMoreErrors(self, errors):
        with self.assertRaises(StopIteration):
            next(errors)

    def test_tidy_config(self):
        errors = tidy.check_config_file(os.path.join(BASE_PATH, "servo-tidy.toml"), print_text=False)
        self.assertEqual("invalid config key 'key-outside'", next(errors)[2])
        self.assertEqual("invalid config key 'wrong-key'", next(errors)[2])
        self.assertEqual("invalid config table [wrong]", next(errors)[2])
        self.assertEqual("ignored file './fake/file.html' doesn't exist", next(errors)[2])
        self.assertEqual("ignored directory './fake/dir' doesn't exist", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_directory_checks(self):
        dirs = {
            os.path.join(BASE_PATH, "dir_check/webidl_plus"): ["webidl", "test"],
            os.path.join(BASE_PATH, "dir_check/only_webidl"): ["webidl"],
        }
        errors = tidy.check_directory_files(dirs, print_text=False)
        error_dir = os.path.join(BASE_PATH, "dir_check/webidl_plus")
        self.assertEqual(
            "Unexpected extension found for test.rs. We only expect files with webidl, "
            + f"test extensions in {error_dir}",
            next(errors)[2],
        )
        self.assertEqual(
            "Unexpected extension found for test2.rs. We only expect files with webidl, "
            + f"test extensions in {error_dir}",
            next(errors)[2],
        )
        self.assertNoMoreErrors(errors)

    def test_spaces_correctnes(self):
        errors = tidy.collect_errors_for_files(iterFile("wrong_space.rs"), [], [tidy.check_by_line], print_text=False)
        self.assertEqual("trailing whitespace", next(errors)[2])
        self.assertEqual("no newline at EOF", next(errors)[2])
        self.assertEqual("tab on line", next(errors)[2])
        self.assertEqual("CR on line", next(errors)[2])
        self.assertEqual("no newline at EOF", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_empty_file(self):
        errors = tidy.collect_errors_for_files(iterFile("empty_file.rs"), [], [tidy.check_by_line], print_text=False)
        self.assertEqual("file is empty", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_whatwg_link(self):
        errors = tidy.collect_errors_for_files(iterFile("whatwg_link.rs"), [], [tidy.check_by_line], print_text=False)
        self.assertEqual(
            "link to WHATWG may break in the future, use this format instead: https://html.spec.whatwg.org/multipage/#dom-context-2d-putimagedata",
            next(errors)[2],
        )
        self.assertEqual(
            "links to WHATWG single-page url, change to multi page: https://html.spec.whatwg.org/multipage/#typographic-conventions",
            next(errors)[2],
        )
        self.assertNoMoreErrors(errors)

    def test_license(self):
        errors = tidy.collect_errors_for_files(
            iterFile("incorrect_license.rs"), [], [tidy.check_license], print_text=False
        )
        self.assertEqual("incorrect license", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_shebang_license(self):
        errors = tidy.collect_errors_for_files(
            iterFile("shebang_license.py"), [], [tidy.check_license], print_text=False
        )
        self.assertEqual("missing blank line after shebang", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_shell(self):
        errors = tidy.collect_errors_for_files(iterFile("shell_tidy.sh"), [], [tidy.check_shell], print_text=False)
        self.assertEqual('script does not have shebang "#!/usr/bin/env bash"', next(errors)[2])
        self.assertEqual('script is missing options "set -o errexit", "set -o pipefail"', next(errors)[2])
        self.assertEqual("script should not use backticks for command substitution", next(errors)[2])
        self.assertEqual('variable substitutions should use the full "${VAR}" form', next(errors)[2])
        self.assertEqual("script should use `[[` instead of `[` for conditional testing", next(errors)[2])
        self.assertEqual("script should use `[[` instead of `[` for conditional testing", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_apache2_incomplete(self):
        errors = tidy.collect_errors_for_files(
            iterFile("apache2_license.rs"), [], [tidy.check_license], print_text=False
        )
        self.assertEqual("incorrect license", next(errors)[2])

    def test_rust(self):
        errors = tidy.collect_errors_for_files(iterFile("rust_tidy.rs"), [], [tidy.check_rust], print_text=False)
        self.assertEqual("Comments starting with `//` should also include a space", next(errors)[2])
        self.assertEqual("use &T instead of &Root<T>", next(errors)[2])
        self.assertEqual("use &T instead of &DomRoot<T>", next(errors)[2])
        self.assertEqual("Comments starting with `//` should also include a space", next(errors)[2])
        self.assertEqual("Comments starting with `//` should also include a space", next(errors)[2])
        self.assertNoMoreErrors(errors)

        ban_errors = tidy.collect_errors_for_files(iterFile("ban.rs"), [], [tidy.check_rust], print_text=False)
        self.assertEqual("Banned type Cell<JSVal> detected. Use MutDom<JSVal> instead", next(ban_errors)[2])
        self.assertNoMoreErrors(ban_errors)

        ban_errors = tidy.collect_errors_for_files(
            iterFile("ban-domrefcell.rs"), [], [tidy.check_rust], print_text=False
        )
        self.assertEqual("Banned type DomRefCell<Dom<T>> detected. Use MutDom<T> instead", next(ban_errors)[2])
        self.assertNoMoreErrors(ban_errors)

    def test_spec_link(self):
        tidy.SPEC_BASE_PATH = BASE_PATH
        errors = tidy.collect_errors_for_files(iterFile("speclink.rs"), [], [tidy.check_spec], print_text=False)
        self.assertEqual("method declared in webidl is missing a comment with a specification link", next(errors)[2])
        self.assertEqual("method declared in webidl is missing a comment with a specification link", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_webidl(self):
        errors = tidy.collect_errors_for_files(iterFile("spec.webidl"), [tidy.check_webidl_spec], [], print_text=False)
        self.assertEqual("No specification link found.", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_toml(self):
        errors = tidy.collect_errors_for_files(iterFile("Cargo.toml"), [], [tidy.check_toml], print_text=False)
        self.assertEqual("found asterisk instead of minimum version number", next(errors)[2])
        self.assertEqual(".toml file should contain a valid license.", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_toml_path_dependencies_use_workspace(self):
        errors = tidy.collect_errors_for_files(
            iterFile("path_dependency/Cargo.toml"), [], [tidy.check_toml], print_text=False
        )
        self.assertEqual(
            "path dependencies must be declared in the root Cargo.toml and referenced with workspace = true",
            next(errors)[2],
        )
        self.assertNoMoreErrors(errors)

    def test_toml_path_dependencies_self_reference(self):
        errors = tidy.collect_errors_for_files(
            iterFile("path_dependency/self-reference.Cargo.toml"), [], [tidy.check_toml], print_text=False
        )
        self.assertNoMoreErrors(errors)

    def test_modeline(self):
        errors = tidy.collect_errors_for_files(iterFile("modeline.txt"), [], [tidy.check_modeline], print_text=False)
        self.assertEqual("vi modeline present", next(errors)[2])
        self.assertEqual("vi modeline present", next(errors)[2])
        self.assertEqual("vi modeline present", next(errors)[2])
        self.assertEqual("emacs file variables present", next(errors)[2])
        self.assertEqual("emacs file variables present", next(errors)[2])
        self.assertNoMoreErrors(errors)

    def test_file_list(self):
        file_path = os.path.join(BASE_PATH, "test_ignored")
        file_list = tidy.FileList(file_path, only_changed_files=False, exclude_dirs=[], progress=False)
        lst = list(file_list)
        self.assertEqual(
            [os.path.join(file_path, "whee", "test.rs"), os.path.join(file_path, "whee", "foo", "bar.rs")], lst
        )
        file_list = tidy.FileList(
            file_path, only_changed_files=False, exclude_dirs=[os.path.join(file_path, "whee", "foo")], progress=False
        )
        lst = list(file_list)
        self.assertEqual([os.path.join(file_path, "whee", "test.rs")], lst)

    def test_feature_annotation(self):
        errors = tidy.check_feature_annotation(
            "prefs.rs",
            [
                b"// feature:",
                b"// feature: a | #a | a |",
                b"// feature: | 123 |",
            ],
        )
        self.assertEqual("Feature annotation has too few | separators", next(errors)[1])
        self.assertEqual("Feature annotation has too many | separators", next(errors)[1])
        self.assertEqual("Feature annotation issue number is not a number", next(errors)[1])
        self.assertEqual("Feature annotation name is missing", next(errors)[1])
        self.assertEqual("Feature annotation issue number must start with #", next(errors)[1])
        self.assertEqual("Feature annotation URL path is missing", next(errors)[1])

    def test_raw_url_in_rustdoc(self):
        def assert_has_a_single_rustdoc_error(errors: Iterable[tuple[int, str]]):
            self.assertEqual(tidy.ERROR_RAW_URL_IN_RUSTDOC, next(errors)[1])
            self.assertNoMoreErrors(errors)

        errors = tidy.check_for_raw_urls_in_rustdoc("file.rs", 3, b"/// https://google.com")
        assert_has_a_single_rustdoc_error(errors)

        errors = tidy.check_for_raw_urls_in_rustdoc("file.rs", 3, b"//! (https://google.com)")
        assert_has_a_single_rustdoc_error(errors)

        errors = tidy.check_for_raw_urls_in_rustdoc("file.rs", 3, b"/// <https://google.com>")
        self.assertNoMoreErrors(errors)

        errors = tidy.check_for_raw_urls_in_rustdoc("file.rs", 3, b"/// [hi]: https://google.com")
        self.assertNoMoreErrors(errors)

        errors = tidy.check_for_raw_urls_in_rustdoc("file.rs", 3, b"/// [hi](https://google.com)")
        self.assertNoMoreErrors(errors)

    def test_check_coauthors(self):
        for _ in tidy.check_config_file(os.path.join(BASE_PATH, "servo-tidy.toml"), print_text=False):
            ...
        self.assert_(tidy.config["disallowed-coauthors"])

        git_log = (
            "commit 1111\n"
            "Author: Contributor <contributor@example.net>\n"
            "Committer: Contributor <contributor@example.net>\n"
            "commit 2222\n"
            "Author: Contributor <contributor@example.net>\n"
            "Committer: Contributor <contributor@example.net>\n"
            "Co-authored-by: LLM <llm@example.com>\n"
            "assisted-by: LLM <llm@example.com>\n"
            "commit 3333\n"
            "Author: LLM <llm@example.com>\n"
            "Committer: Contributor <contributor@example.net>\n"
            "commit 4444\n"
            "Author: Contributor <contributor@example.net>\n"
            "Committer: LLM <llm@example.com>\n"
        )
        errors = tidy.check_coauthors(pull_request_body="", git_log=git_log, verbose=False)
        self.assertEqual(
            next(errors),
            "Commit 2222 has `Co-authored-by: LLM <llm@example.com>`. "
            "Contributions must not include content generated by large language models "
            "or other probabilistic tools. "
            "See https://book.servo.org/contributing/getting-started.html#ai-contributions",
        )
        self.assertEqual(
            next(errors),
            "Commit 2222 has `assisted-by: LLM <llm@example.com>`. "
            "Contributions must not include content generated by large language models "
            "or other probabilistic tools. "
            "See https://book.servo.org/contributing/getting-started.html#ai-contributions",
        )
        self.assertEqual(
            next(errors),
            "Commit 3333 has `Author: LLM <llm@example.com>`. "
            "Contributions must not include content generated by large language models "
            "or other probabilistic tools. "
            "See https://book.servo.org/contributing/getting-started.html#ai-contributions",
        )
        self.assertEqual(
            next(errors),
            "Commit 4444 has `Committer: LLM <llm@example.com>`. "
            "Contributions must not include content generated by large language models "
            "or other probabilistic tools. "
            "See https://book.servo.org/contributing/getting-started.html#ai-contributions",
        )
        self.assertNoMoreErrors(errors)

        pr_body = "Something something\nAssisted-by: LLM <llm@example.com>\nCo-authored-by: LLM <llm@example.com>"
        errors = tidy.check_coauthors(pull_request_body=pr_body, git_log="", verbose=False)
        self.assertEqual(
            next(errors),
            "Pull request body has `Assisted-by: LLM <llm@example.com>`. "
            "Contributions must not include content generated by large language models "
            "or other probabilistic tools. "
            "See https://book.servo.org/contributing/getting-started.html#ai-contributions",
        )
        self.assertEqual(
            next(errors),
            "Pull request body has `Co-authored-by: LLM <llm@example.com>`. "
            "Contributions must not include content generated by large language models "
            "or other probabilistic tools. "
            "See https://book.servo.org/contributing/getting-started.html#ai-contributions",
        )
        self.assertNoMoreErrors(errors)


def run_tests():
    verbosity = 1 if logging.getLogger().level >= logging.WARN else 2
    suite = unittest.TestLoader().loadTestsFromTestCase(CheckTidiness)
    return unittest.TextTestRunner(verbosity=verbosity).run(suite).wasSuccessful()
