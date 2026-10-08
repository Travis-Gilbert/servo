# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Bounded downstream component WPTs using an existing, verified CI package.

This does not build Servo, change expectations, promote a consumer pin, or run
Theorem's private Browser Oracle. All selectors come from the public Servo tree.
The manual workflow supplies only full revision/run/artifact identities, through
environment variables. The GitHub token is used only by the verify stage.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import signal
import subprocess
import tarfile
from typing import Any
import urllib.request


REPOSITORY = "Travis-Gilbert/servo"
REPOSITORY_ID = 1327308958
ARTIFACT_NAME = "checked-release-binary-linux"
GROUPS = (
    "IndexedDB",
    "webstorage",
    "web-locks",
    "html/browsers/the-windowproxy-exotic-object",
    "html/browsers/windows/auxiliary-browsing-contexts/named-lookup-noopener.html",
    "html/browsers/windows/auxiliary-browsing-contexts/named-lookup-scoped-to-browsing-context-group.html",
    "html/browsers/windows/browsing-context-names/duplicate-name-order.html",
    "html/browsers/windows/targeting-cross-origin-nested-browsing-contexts.html",
    "WebCryptoAPI/encrypt_decrypt/chacha20_poly1305.tentative.https.any.js",
    "WebCryptoAPI/serialization/chacha20-poly1305.tentative.https.any.js",
    "WebCryptoAPI/sign_verify/ecdsa.https.any.js",
    "WebCryptoAPI/derive_bits_keys/ecdh_bits.https.any.js",
    "WebCryptoAPI/derive_bits_keys/ecdh_keys.https.any.js",
)
# Derived from the public 0b322b0234a1f00f1173b63c299b815d924f3711 manifest.
# A changed cohort requires a separately reviewed update, never truncation.
ROSTER_COUNT = 978
ROSTER_SHA256 = "8356f9b33d9ecf9c12943e00faab873884bb8224e31144e33b5210288f479af8"
MAX_URLS = 1000
MAX_ARCHIVE_BYTES = 2 * 1024**3
MAX_UNPACKED_BYTES = 4 * 1024**3
TIMEOUT_SECONDS = 7200


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024**2):
            digest.update(block)
    return digest.hexdigest()


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def identities() -> dict[str, str]:
    result = {name: os.environ[name] for name in ("ENGINE_SHA", "BUILD_RUN_ID", "ARTIFACT_ID", "ARTIFACT_DIGEST")}
    require(bool(re.fullmatch(r"[0-9a-f]{40}", result["ENGINE_SHA"])), "Engine revision must be a full SHA")
    for name in ("BUILD_RUN_ID", "ARTIFACT_ID"):
        require(bool(re.fullmatch(r"[1-9][0-9]{0,19}", result[name])), "Run/artifact IDs must be positive integers")
    require(
        bool(re.fullmatch(r"sha256:[0-9a-f]{64}", result["ARTIFACT_DIGEST"])), "Require the exact API SHA256 digest"
    )
    require(os.environ.get("GITHUB_REPOSITORY") == REPOSITORY, "Only the designated downstream repository is supported")
    return result


class SafeRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request: Any, fp: Any, code: int, msg: str, headers: Any, url: str) -> Any:
        require(url.startswith("https://"), "Refusing non-HTTPS API redirect")
        redirected = super().redirect_request(request, fp, code, msg, headers, url)
        if redirected is not None:
            # Signed log-download URLs do not need the GitHub credential.
            redirected.remove_header("Authorization")
        return redirected


def github(route: str, token: str, raw: bool = False) -> Any:
    request = urllib.request.Request(
        "https://api.github.com/repos/" + REPOSITORY + "/" + route,
        headers={"Authorization": "Bearer " + token, "Accept": "application/vnd.github+json"},
    )
    with urllib.request.build_opener(SafeRedirect()).open(request, timeout=60) as response:
        data = response.read(16 * 1024**2 + 1)
    require(len(data) <= 16 * 1024**2, "API response exceeded its bound")
    return data.decode() if raw else json.loads(data)


def validate_metadata(ids: dict[str, str], run: dict, artifact: dict, jobs: list[dict]) -> dict:
    require(run["id"] == int(ids["BUILD_RUN_ID"]), "Wrong build run")
    require(run["head_sha"] == ids["ENGINE_SHA"], "Wrong engine SHA")
    require(run["repository"]["id"] == REPOSITORY_ID, "Wrong repository")
    require(run["head_repository"]["id"] == REPOSITORY_ID, "Cross-repository build is forbidden")
    require(run["name"] == "Main" and run["path"] == ".github/workflows/main.yml", "Wrong build workflow")
    require(run["status"] == "completed" and run["conclusion"] == "success", "Main must have completed successfully")
    require(artifact["id"] == int(ids["ARTIFACT_ID"]) and artifact["name"] == ARTIFACT_NAME, "Wrong artifact")
    require(not artifact["expired"], "Artifact expired")
    require(0 < artifact["size_in_bytes"] <= MAX_ARCHIVE_BYTES, "Artifact size is invalid")
    require(artifact["digest"] == ids["ARTIFACT_DIGEST"], "Artifact digest differs from reviewed input")
    linked = artifact["workflow_run"]
    require(linked["id"] == run["id"] and linked["head_sha"] == ids["ENGINE_SHA"], "Artifact run/SHA mismatch")
    require(linked["repository_id"] == REPOSITORY_ID, "Artifact repository mismatch")
    require(linked["head_repository_id"] == REPOSITORY_ID, "Artifact head repository mismatch")
    builds = [job for job in jobs if "Linux Build (x86_64) [" in job["name"]]
    require(len(builds) == 1, "Require exactly one x86_64 Linux build job")
    job = builds[0]
    require(job["status"] == "completed" and job["conclusion"] == "success", "Linux build did not pass")
    require(
        job["started_at"] <= artifact["created_at"] <= job["completed_at"], "Artifact is not from this build attempt"
    )
    return job


def validate_build_source(engine_sha: str, log: str, head: dict, built: dict) -> str:
    revisions = set(re.findall(r"git checkout --progress --force ([0-9a-f]{40})(?:\s|$)", log))
    require(len(revisions) == 1, "Cannot bind the hosted build checkout to one exact commit")
    revision = revisions.pop()
    require(built["sha"] == revision and head["sha"] == engine_sha, "Commit API identity mismatch")
    require(built["tree"]["sha"] == head["tree"]["sha"], "Built merge tree differs from candidate tree")
    require(
        revision == engine_sha or engine_sha in [parent["sha"] for parent in built["parents"]], "Unrelated build commit"
    )
    return revision


def verify(output: Path) -> None:
    ids = identities()
    token = os.environ["GH_TOKEN"]
    run = github("actions/runs/" + ids["BUILD_RUN_ID"], token)
    artifact = github("actions/artifacts/" + ids["ARTIFACT_ID"], token)
    jobs = github("actions/runs/" + ids["BUILD_RUN_ID"] + "/jobs?per_page=100", token)
    require(jobs["total_count"] <= 100, "Job list exceeds bounded verifier capacity")
    job = validate_metadata(ids, run, artifact, jobs["jobs"])
    log = github("actions/jobs/" + str(job["id"]) + "/logs", token, raw=True)
    revisions = set(re.findall(r"git checkout --progress --force ([0-9a-f]{40})(?:\s|$)", log))
    require(len(revisions) == 1, "Ambiguous build checkout")
    build_sha = next(iter(revisions))
    head = github("git/commits/" + ids["ENGINE_SHA"], token)
    built = github("git/commits/" + build_sha, token)
    validate_build_source(ids["ENGINE_SHA"], log, head, built)
    write_json(
        output / "provenance.json",
        {
            "identities": ids,
            "repository": REPOSITORY,
            "build_job_id": job["id"],
            "build_checkout_sha": build_sha,
            "source_tree": head["tree"]["sha"],
            "build_log_sha256": hashlib.sha256(log.encode()).hexdigest(),
            "artifact": artifact,
            "run_attempt": run["run_attempt"],
            "orchestration": {
                "sha": os.environ.get("GITHUB_SHA"),
                "run_id": os.environ.get("GITHUB_RUN_ID"),
                "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
                "workflow_ref": os.environ.get("GITHUB_WORKFLOW_REF"),
                "runner_sha256": sha256(Path(__file__)),
            },
            "scope": "Public downstream component WPT; no product pin or private Oracle acceptance",
        },
    )
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
        stream.write("engine_sha=" + ids["ENGINE_SHA"] + "\n")
        stream.write("artifact_id=" + ids["ARTIFACT_ID"] + "\n")
        stream.write("build_run_id=" + ids["BUILD_RUN_ID"] + "\n")


def select_roster(root: Path) -> tuple[list[str], dict[str, list[str]]]:
    manifest = json.loads((root / "tests/wpt/meta/MANIFEST.json").read_text())["items"]["testharness"]
    selected: list[str] = []
    group_urls: dict[str, list[str]] = {}

    def walk(node: Any, path: str) -> None:
        if isinstance(node, dict):
            for name, child in sorted(node.items()):
                walk(child, path + "/" + name)
        else:
            require((root / "tests/wpt/tests" / path).is_file(), "Manifest source missing: " + path)
            for url, _ in node[1:]:
                selected.append("/" + (url or path).lstrip("/"))

    for group in GROUPS:
        node = manifest
        for part in group.split("/"):
            node = node[part]
        before = len(selected)
        walk(node, group)
        group_urls[group] = sorted(selected[before:])
        require(bool(group_urls[group]), "Empty selected group")
    require(len(selected) == len(set(selected)), "Duplicate manifest URLs")
    selected.sort()
    digest = hashlib.sha256(("\n".join(selected) + "\n").encode()).hexdigest()
    require(len(selected) == ROSTER_COUNT and len(selected) <= MAX_URLS, "Roster count changed or exceeded cap")
    require(digest == ROSTER_SHA256, "Public roster changed; review required")
    return selected, group_urls


def extract_package(archive: Path, destination: Path) -> Path:
    require(archive.is_file(), "Packaged binary archive missing")
    require(0 < archive.stat().st_size <= MAX_ARCHIVE_BYTES, "Packaged archive size invalid")
    require(not destination.exists(), "Package destination must be new")
    with tarfile.open(archive, "r:gz") as tar:
        members = tar.getmembers()
        require(len(members) <= 30000, "Too many package files")
        require(sum(member.size for member in members) <= MAX_UNPACKED_BYTES, "Unpacked package too large")
        for member in members:
            path = PurePosixPath(member.name)
            require(
                not path.is_absolute() and ".." not in path.parts and path.parts[0] == "servo", "Unsafe package path"
            )
            require(member.isfile() or member.isdir(), "Package links/devices are forbidden")
        destination.mkdir()
        tar.extractall(destination, members=members, filter="data")
    binary = destination / "servo/servoshell"
    require(binary.is_file() and os.access(binary, os.X_OK), "Executable servoshell missing")
    require((destination / "servo/resources").is_dir(), "Packaged resources missing")
    return binary


def wpt_command(root: Path, binary: Path, output: Path) -> list[str]:
    return [
        str(root / "mach"),
        "test-wpt",
        "--bin",
        str(binary),
        "--profile",
        "checked-release",
        "--processes",
        "2",
        "--timeout-multiplier",
        "2",
        "--retry-unexpected",
        "0",
        "--no-default-test-types",
        "--test-types",
        "testharness",
        "--pref",
        "dom_indexeddb_enabled=true",
        "--include-file",
        str(output / "include.txt"),
        "--log-raw",
        str(output / "raw.jsonl"),
        "--log-wptreport",
        str(output / "wptreport.json"),
    ]


def validate_binary_version(version: str, build_sha: str) -> None:
    # build.rs uses git rev-parse --short HEAD; bpaf adds "Version: ".
    match = re.fullmatch(r"Version: Servo [^\s]+-([0-9a-f]{7,40})", version)
    require(match is not None and build_sha.startswith(match[1]), "Binary build revision mismatch")


def summarize(raw: Path, selected: list[str], group_urls: dict[str, list[str]]) -> dict:
    require(raw.is_file(), "Raw WPT log missing")
    require(tuple(group_urls) == GROUPS, "Selected group membership missing or reordered")
    members = [url for urls in group_urls.values() for url in urls]
    require(all(group_urls.values()), "Empty selected group membership")
    require(len(members) == len(set(members)), "Duplicate selected group membership")
    require(len(selected) == len(members) and set(selected) == set(members), "Group membership differs from roster")
    membership = {group: set(urls) for group, urls in group_urls.items()}
    completed: set[str] = set()
    statuses: dict[str, int] = {}
    subtests: dict[str, int] = {}
    unexpected = 0
    executed_groups = dict.fromkeys(GROUPS, 0)
    suite_end = False
    with raw.open() as stream:
        for line in stream:
            row = json.loads(line)
            action = row.get("action")
            if action == "suite_end":
                suite_end = True
            if action in ("test_end", "test_status"):
                status = row["status"]
                target = statuses if action == "test_end" else subtests
                target[status] = target.get(status, 0) + 1
                unexpected += int(
                    "expected" in row and status != row["expected"] and status not in row.get("known_intermittent", [])
                )
            if action == "test_end":
                test = "/" + row["test"].lstrip("/")
                require(test not in completed, "Duplicate WPT completion")
                completed.add(test)
                if row["status"] != "SKIP":
                    for group, urls in membership.items():
                        if test in urls:
                            executed_groups[group] += 1
    require(not completed.difference(selected), "WPT executed outside the selected roster")
    return {
        "suite_end": suite_end,
        "completed_count": len(completed),
        "statuses": statuses,
        "subtest_statuses": subtests,
        "functional_subtest_pass_count": subtests.get("PASS", 0),
        "actual_failure_count": sum(count for status, count in statuses.items() if status not in ("OK", "SKIP")),
        "actual_subtest_failure_count": sum(count for status, count in subtests.items() if status != "PASS"),
        "skipped_test_count": statuses.get("SKIP", 0),
        "unexpected_count": unexpected,
        "executed_groups": executed_groups,
        "missing_urls": sorted(set(selected).difference(completed)),
    }


def gate_result(exit_code: int, summary: dict, metadata_unchanged: bool) -> str:
    complete = summary["suite_end"] and not summary["missing_urls"] and all(summary["executed_groups"].values())
    if exit_code == 0 and complete and summary["unexpected_count"] == 0 and metadata_unchanged:
        return "expectation_match"
    return "failed"


def metadata_matches_head(root: Path) -> bool:
    return not bool(
        subprocess.check_output(
            ["git", "diff", "HEAD", "--", "tests/wpt/meta", "tests/wpt/include.ini", "tests/wpt/tests"], cwd=root
        )
    )


def run(root: Path, output: Path) -> None:
    provenance = json.loads((output / "provenance.json").read_text())
    ids = identities()
    require(ids == provenance["identities"], "Verified input identities changed")
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    require(actual == ids["ENGINE_SHA"], "WPT source checkout differs from verified candidate")
    require(
        not subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root),
        "Dirty WPT source",
    )
    selected, group_urls = select_roster(root)
    counts = {group: len(urls) for group, urls in group_urls.items()}
    (output / "include.txt").write_text("\n".join(selected) + "\n")
    binary = extract_package(output / "download/servo-tech-demo.tar.gz", output / "package")
    child_env = {
        name: value
        for name, value in os.environ.items()
        if "TOKEN" not in name.upper() and "SECRET" not in name.upper() and name != "GITHUB_CONTEXT"
    }
    version = subprocess.check_output([str(binary), "--version"], env=child_env, text=True, timeout=30).strip()
    validate_binary_version(version, provenance["build_checkout_sha"])
    command = wpt_command(root, binary, output)
    receipt = {
        "source_sha": actual,
        "binary_sha256": sha256(binary),
        "binary_version": version,
        "package_sha256": sha256(output / "download/servo-tech-demo.tar.gz"),
        "roster_sha256": ROSTER_SHA256,
        "selected_count": len(selected),
        "group_counts": counts,
        "command": command,
        "processes": 2,
        "timeout_seconds": TIMEOUT_SECONDS,
        "expectations": "Unmodified candidate metadata; expected failures are not functional passes",
        "result": "pending",
    }
    write_json(output / "receipt.json", receipt)
    with (output / "console.log").open("w") as log:
        process = subprocess.Popen(
            command, cwd=root, env=child_env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True
        )
        try:
            code = process.wait(timeout=TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            code = 124
    receipt["exit_code"] = code
    receipt["result"] = "failed"
    write_json(output / "receipt.json", receipt)
    summary = summarize(output / "raw.jsonl", selected, group_urls)
    receipt.update(summary)
    receipt["metadata_unchanged"] = metadata_matches_head(root)
    receipt["result"] = gate_result(code, summary, receipt["metadata_unchanged"])
    write_json(output / "receipt.json", receipt)
    require(receipt["metadata_unchanged"], "WPT source/expectations changed during execution")
    require(receipt["result"] == "expectation_match", "WPT incomplete or failed; inspect receipt and raw logs")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stage", choices=("verify", "run", "roster"))
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    if args.stage == "verify":
        verify(output)
    elif args.stage == "run":
        run(args.root.resolve(), output)
    else:
        selected, group_urls = select_roster(args.root.resolve())
        counts = {group: len(urls) for group, urls in group_urls.items()}
        write_json(
            output / "roster.json",
            {"count": len(selected), "sha256": ROSTER_SHA256, "groups": counts, "urls": selected},
        )


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        # HTTP errors can contain signed redirect URLs; do not disclose them.
        print("Downstream WPT refused: " + (str(error) if isinstance(error, ValueError) else type(error).__name__))
        raise SystemExit(2)
