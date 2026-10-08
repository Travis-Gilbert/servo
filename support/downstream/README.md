# Downstream Servo queue

This ledger belongs to the shared Servo fork. Consumers select independent exact
pins; this document does not promote a consumer pin or establish engine acceptance.

`queue.json` covers every public downstream commit after upstream Servo v0.5.0
`1d44e5dd6a8b64c02f9dbf7fcbdf4ebdd0740019` through
`0b322b0234a1f00f1173b63c299b815d924f3711`, in application order. Its 63 entries
include the 56-commit frozen baseline and seven forward cleanup/maintenance commits.
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
and held-message FIFO/barrier/failure/abort lifetime handling in the script lint repair.
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
