# Downstream Servo queue

This ledger belongs to the shared Servo fork. Consumers select independent exact
pins; this document does not promote a consumer pin or establish engine acceptance.

`queue.json` covers every public downstream commit after upstream Servo v0.5.0
`1d44e5dd6a8b64c02f9dbf7fcbdf4ebdd0740019` through
`86e6da7e5db02b7bcce2e4e61e53a099bafcdeaf`, in application order. Its 71 entries
include the 56-commit frozen baseline and 15 forward cleanup/maintenance commits.
Each entry names its purpose, exact upstream base, retention rationale, regression
mapping, and pending evidence/equivalence status. The conservative dependency chain
records cumulative replay order; it does not claim every predecessor is a semantic
dependency. Do not reorder commits based only on that distinction.

## Historical archive and complete lineage

Turvo's public `patches/servo/` directory preserves nine digest-pinned historical
artifacts at baseline `e92cdaa790797479c1821c33470c64e0d166feb2`. Those artifacts
cover 43 commits: seven individual patches, 33 IndexedDB commits, and three script
commits. They are not a complete upstream-to-tip replay queue. The 13 uncovered
earlier commits implement the document snapshot/hit-test seam, sandbox diagnostics
and lifecycle fixes, Promise generator correction, and v0.5 adaptation. This ledger
includes all of them. Preserve the artifacts and previous branches as history;
do not overwrite their digests or reinterpret reverse-application as full replay.

The original factory patch also introduced compatibility defaults and operation
contracts. The forward storage cleanup removes injection, while retaining useful
storage implementation and IndexedDB conformance changes. Its public API removal
is source-breaking. Historical factory-selection tests are superseded by built-in
backend round-trip tests; the ledger preserves both commits rather than pretending
the original patch disappeared. CacheStorage remains the dummy baseline described
in [storage-defaults.md](../storage-defaults.md).

## Run the metadata gate

From the Servo repository:

```sh
python3 support/downstream/check_queue.py
python3 -m unittest discover -s support/downstream -p 'test_*.py'
```

The checker reads Git and source paths; it does not execute commands from JSON or
build Servo. It rejects missing/duplicate commits, wrong order, missing metadata,
forward/missing dependencies, unknown regressions, and absent/external paths.
It also checks every commit after the coverage revision through `HEAD`. Explicit
path rules classify later ledger documentation, queue tooling, and the named WPT runner/workflow-only changes and print
each classified commit. Any other or mixed source change requires extending exact
coverage. This avoids silently ignoring later commits without creating a recursive
requirement to record the ledger commit's own SHA. Review those narrow path rules
when changing the checker; they are metadata classifications, not CI acceptance.

All listed engine regression evidence is **pending** here. A command and an
existing test path establish a mapping, not execution, nonzero selection, passing
behavior, or a fresh baseline comparison. `behavioral_pending` explicitly marks
patches whose complete behavioral oracle is still unmapped. Build/lint commands
remain useful for those patches but cannot discharge the missing behavior.

Current gaps include snapshot/hit-test semantics; multiprocess profiler identity,
diagnostic forwarding and closure recovery; response-handle sendability/lifecycle;
shared-worker owner teardown; compatibility defaults; and backend reply-loss and
structural failure injection beyond existing happy-path/storage conformance tests;
held-message FIFO/barrier/failure/abort lifetime handling in the script lint repair;
and advisory-specific HTTP2/TLS attack handling and platform-specific crypto backend
correctness after the dependency updates.
The broad IndexedDB and Web Locks WPT mappings must be run with the actual selected
tests, runner configuration and baseline before acceptance. Compiler-only fixes
use compiler/Crown/lint oracles without claiming behavioral equivalence.

## Maintaining the fork

Append engine commits; retain history. For each new change, extend exact coverage
and add a narrow purpose, retention reason, dependencies, executable regression
mapping, and any explicit oracle gap. Record execution receipts separately with
the exact engine revision, environment, selected tests, results, and failures.
Do not convert this coverage ledger into an acceptance checklist.

For an upstream release, create an isolated candidate from the new exact base.
Check whether upstream provides each retained behavior. Replay or adapt only the
still-required commits, compare the resulting tree with the intended candidate,
and rerun the associated regressions and required consumer/native journeys. Drop
a patch only after equivalence is demonstrated. Preserve the previous queue and
receipts as historical references. No Servo 0.7 upgrade is combined with this
storage cleanup, and this fork's changes are not submitted upstream.

## Selected WPTs with an existing checked-release artifact

`run_wpt.py` and `.github/workflows/downstream-wpt.yml` provide a bounded public
engine component check. They neither build Servo nor repin a consumer. The corpus
comes exclusively from the public candidate manifest: all IndexedDB (817 URLs),
WebStorage (54), Web Locks (85), WindowProxy exotic-object tests (8), and four
retained named-window regressions (4), plus ChaCha20-Poly1305 encryption and
serialization, ECDSA, and ECDH bit/key derivation (10 window/worker URLs).
Its 978 URLs have SHA-256
`8356f9b33d9ecf9c12943e00faab873884bb8224e31144e33b5210288f479af8`.
The cap is 1,000 URLs; a changed roster refuses execution pending review.
This is separate from Theorem's frozen private Browser Oracle and its exact
101-source compatibility cohort, which remain pending acceptance.

The verifier requires a completed successful same-repository `Main` run for the
full engine SHA, one successful x86_64 Linux build job, its unexpired immutable
`checked-release-binary-linux` artifact ID, and the exact reviewed API digest.
It rejects artifacts from another run, repository, head, or build attempt.
PR builds use synthetic merge commits: the build job's checkout log binds that
commit, its complete Git tree must equal the requested head's tree, and a merge
must have that head as a parent. Both identities are recorded; they are not
silently equated. The binary's `--version` must contain the actual build commit's
available short-SHA prefix. `ports/servoshell/build.rs` supplies `git rev-parse
--short HEAD`; `lib.rs` and bpaf print `Version: Servo <version>-<short-SHA>`.
Provenance separately records the orchestration run/revision/workflow reference
and runner source SHA-256, rather than treating engine and runner revisions as one.

The [download action's v8 inputs](https://raw.githubusercontent.com/actions/download-artifact/v8/action.yml)
support immutable `artifact-ids`, cross-run `run-id`, and `digest-mismatch: error`.
The verifier first compares the API ZIP digest with the reviewed input; the action
then validates the downloaded ZIP against that API digest and fails on mismatch.
The runner records separate inner tarball and binary SHA-256 values. It does not
mislabel either inner hash as the outer artifact digest. Plainly copying an
unverified tarball into its output directory is not a supported provenance route.

Only the existing ephemeral `github.token` is used, with `contents: read` and
`actions: read`; no new secret or private repository access is required. The
verification token is confined to its step, redirect requests strip it, checkout
credentials are not persisted, and child WPT processes exclude token/secret
environment variables. Downloaded packages reject traversal, links, and devices.

The workflow runs one Ubuntu 22.04 job with a 160-minute maximum, two WPT workers,
and a 7,200-second runner deadline. Dependencies use the existing Linux WPT
bootstrap route, bounded to 30 minutes. That installs pinned Python/uv, system
packages, and the pinned Rust toolchain on the disposable hosted runner; it skips
Cargo lint/test tool installation and compiles no engine. Network/package/service
availability and sufficient runner disk remain prerequisites. No full-suite WPT,
upstream sync, expectation update, intermittent-dashboard secret, or
`--always-succeed` path is selected.

Receipts retain every URL, actual harness/subtest status counts, skipped tests,
missing completions, and unexpected results. Every selected URL must be accounted
for, with non-skipped execution in each group. A successful exit means
`expectation_match`, not that expected failures passed functionally. Actual
failures and functional subtest passes remain explicit. Expectation/source drift,
zero execution, missing results, and timeouts fail the gate. CacheStorage's dummy
baseline, persistent profile/restart behavior, native embedding acceptance, and
the private product Oracle remain separate obligations.

Before dispatch, parent review must select a successful run and its immutable
artifact/digest. A new directly dispatched workflow must first be registered on
the default branch; this change does not merge it or relax Main. The already
registered `Try` workflow (ID `340367112`) offers a branch-ref route: explicitly
set `reuse-artifact: true` plus the four identities. Only that manual mode skips
Try's decision/build/result jobs and calls the one-job reusable WPT workflow.
Normal Try pushes and manual defaults preserve their build path. Invalid or
missing identities fail verification rather than falling back to a build.

Prepare a reviewed JSON input file with string-valued inputs: set
`reuse-artifact` to `"true"`, plus `engine-sha`, `build-run-id`, `artifact-id`,
and `artifact-digest`. The gh CLI requires strings here even for a boolean
workflow input; GitHub parses `"true"` into the declared boolean.
Then, after the successful artifact exists and dispatch is authorized:

```sh
gh workflow run 340367112 --repo Travis-Gilbert/servo --ref REVIEWED_BRANCH --json < reuse-inputs.json
```

[GitHub's manual-run documentation](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/manually-run-a-workflow)
supports branch-ref dispatch and up to 25 inputs on GitHub.com; Try's 12 fit that
limit. The reusable workflow resolves from the same reviewed caller revision.
The direct Python `run` stage can also execute on provisioned Linux with the same
verified provenance and action-validated package, but standalone authenticated
artifact acquisition is not implemented here; an unverified local copy does not
establish the required digest chain.

Local component checks (no WPT execution or engine build):

```sh
python3 -B -m unittest discover -s support/downstream -p 'run_wpt_tests.py'
python3 -B support/downstream/run_wpt.py roster --output /ABSOLUTE/EVIDENCE_DIRECTORY
```

## Downstream preserved-history coauthor boundary

This maintained downstream fork preserves the original source, patch history,
and attribution. Its frozen baseline is
`e92cdaa790797479c1821c33470c64e0d166feb2`. The optional
`configs.coauthors-history-base` in `servo-tidy.toml` restricts the coauthor
history scan to `BASE..HEAD`. It requires an exact 40-character commit SHA that
identifies a commit object and is an ancestor of HEAD; malformed, missing, tag, or unrelated boundaries
fail the check. Existing trailers are neither removed nor rewritten. New
commits, author/committer identities, and pull request bodies retain the existing
disallowed-coauthor checks and list.

The empty/unset option retains Servo upstream's complete-history behavior. This
is an explicit downstream policy for preserved inherited history, authorized by
the maintained-fork scope; it does not establish eligibility for upstream
submission or change Servo's upstream contribution rules. Updating the frozen
boundary requires a separate reviewed policy decision, not a routine CI repair.

The regression tests use temporary Git repositories: the upstream default
rejects historical disallowed attribution, the explicit baseline preserves it,
new disallowed attribution and PR bodies still fail, and invalid or nonancestor
boundaries fail closed. No project history is altered by those tests.
