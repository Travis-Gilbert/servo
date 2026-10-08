# Default storage candidate

This forward cleanup is based on downstream Servo
`e92cdaa790797479c1821c33470c64e0d166feb2`. The baseline and earlier storage
patch history remain intact. It does not upgrade Servo or change a product pin.

## Scope and compatibility

`StorageEngines`, `ServoBuilder::storage_engines`, and optional injected factory
arguments on storage-thread constructors are removed. Storage managers construct
their built-in factories directly. Internal engine and factory traits, IndexedDB
transaction and conformance repairs, SQLite schemas and migration logic, and
normal storage directory selection are retained.

The default registry and IndexedDB use SQLite. LocalStorage keeps its existing
origin-derived SQLite path; sessionStorage keeps its in-memory map scoped to a
webview and origin. The injected-only session engine, disk path, and clone/clear
branches are removed. Profile options and temporary-storage policy are unchanged.

**CacheStorage is still the baseline dummy implementation.** Its only backend
operation, `has_cache`, returns false. The default-selection regression records
this limitation; it does not establish working cache persistence or Cache API
conformance. A functional CacheStorage backend remains a separate obligation.

This is a source-breaking candidate for embedders using the removed API. Turvo's
current public `with_storage_engines` adapter and the historical Theorem
RustyRed-storage integration require coordinated adaptation before consuming it.
Neither consumer is repinned here, and their historical branches are preserved.

## Verification obligations

The integration test target is named `main`; running only `--lib` misses the
storage-thread and WebStorage coverage. Run both targets with Rust 1.95.0:

```sh
cargo +1.95.0 nextest run --locked -p servo-storage --lib --test main
```

The new default-selection checks cover registry-to-IndexedDB open/upgrade/reply,
independent SQLite readback and reopening, durable localStorage and origin
isolation, ephemeral sessionStorage and view isolation, and the dummy cache
baseline. A volatile replacement for localStorage or IndexedDB fails the disk
and reopen assertions; sharing session state across views fails isolation.

Existing backend/conformance regressions are retained. Two tests solely for the
removed custom factory API are replaced by these default-selection checks.

The candidate still requires successful compilation and execution of both test
targets, selected IndexedDB/WebStorage/Cache API Web Platform Tests, and the
supported embedding/native journeys before a consumer pin is changed. Static
source inspection and formatting checks alone are not behavioral acceptance.

## Retained patch maintenance after hosted CI

Run `37703067978` checked candidate head
`9f7137440f0c0b541261db4c043eb49a7ce6f57d` through merge revision
`2979ef5f337e3d57e4adfe896e4b8cd2d6aba179`. It exposed inherited IndexedDB,
Web Locks, and WebStorage lint defects in files unchanged from the frozen
`e92cdaa790797479c1821c33470c64e0d166feb2` baseline. Their repair is a
separate forward patch; the storage-cleanup commits and prior history remain.

- IndexedDB source parameters use `RootedTraceableBox<RequestSource>` through
  structured cloning and native request allocation, then copy into the traced
  request while the temporary root is still alive. This retains object-store,
  index, and cursor source identity without allowing unrooted DOM edges on the
  stack. The unused `execute_async_failure` helper had no callers and is removed;
  the live callback-creation failure path still queues its asynchronous error.
- Web Locks maps use ordinary `HashMap<u64, V>` inside the existing traced
  `DomRefCell` fields. Primitive keys have empty tracing; values still trace
  promises and callbacks. No value or whole map is marked `no_trace`.
- `WebStorageEngine::is_empty` defaults to the fallible `len` result. Its SQLite
  regression covers empty/populated/cleared stores and propagates database
  errors rather than converting them to successful emptiness.

Reproduce the hosted compiler/linter gates with the pinned Rust 1.95.0, unique
target, and required Linux dependencies:

```sh
cargo install --path support/crown
RUSTFLAGS='-D warnings' ./mach build --use-crown --locked --profile checked-release
./mach clippy --locked --github-annotations -- -- --deny warnings
./mach test-tidy --no-progress --all --github-annotations
```

Then rerun the storage targets above and the existing IndexedDB request-source,
cursor update/delete, clone-ordering and key-generator WPTs, plus Web Locks
acquire/query/held/signal/steal coverage. Ordinary compilation without crown
does not verify the rooted-parameter requirement. GC zeal additionally requires
a `debugmozjs` build; setting zeal preferences on an ordinary build is a no-op.
These are required checks, not a claim they passed for this forward repair.

### Script-level lint repair after run 37716625601

At head `9dc122d0bb5ef6bdf4203d367290d0461a98c041`, completed lint job
`113114569500` exposed five more inherited script-level Clippy failures.
All five failing constructs are present in frozen `e92cdaa790797479c1821c33470c64e0d166feb2`.
The assumption that the inherited baseline was Clippy-clean was false: earlier
compiler and lint failures had prevented this layer from being checked. No
baseline CI pass or successful full candidate gate is established by these
repairs.

- Cursor source deletion and multi-entry key deduplication use let chains with
  the same evaluation and exception ordering. Failed key conversions remain
  ignored in the multi-entry algorithm; duplicate keys remain omitted.
- Request transaction identity drops needless references around the same
  dereferenced operands. Both `DomRoot` owners stay in scope throughout response
  handling; completion remains attributed to the retained transaction.
- `HeldOutbound::Message` boxes its existing IPC message, reducing the Linux
  enum's inline payload from at least 296 bytes to a pointer. Only held messages
  are boxed. Drain moves the message out before sending; FIFO, barriers, failure
  handling, and abort queue clearing are unchanged. The queue gains no raw DOM
  edges. `RequestListener` continues to retain request/transaction `Trusted`
  handles through the in-process callback's `Arc` or the registered IPC router
  callback. Their lifetime and the existing tracing policy are unchanged.
- A private record groups WindowProxy's browsing-context and webview IDs in
  `new_inherited`. It contains no DOM references. The two public constructor
  signatures, same-origin name precedence, and dissimilar-origin empty name
  remain unchanged; no lint suppression is added.

Rerun the exact Clippy, checked-release/crown, and Tidy commands above on the
resulting head. Existing `idbobjectstore_createIndex`, `idbindex-multientry`,
request/source, abort-ordering, cursor exception-order, and WindowProxy origin
regressions remain necessary behavioral coverage. Formatting and a source
review do not establish those WPT results. CacheStorage's dummy baseline and
the pending native/product acceptance requirements remain as documented above.

Hosted lint run `37706099233` at `5920dcccede120f5570496cbc4f25114a9993177`
then exposed an inherited `clone_on_copy` in constellation's random-pipeline
stress path, unchanged at the same line in frozen `e92cdaa`. Passing the
already-`Copy` pipeline ID directly preserves the stress behavior and removes
the redundant clone. This is a separate maintenance commit; the hosted gates
must be rerun for its exact head before promotion.


### Coherent inherited-baseline repair after run 37707697068

At `ab88d40e8efd51e3a3fd9e141e20014cda44a7f6`, the Linux checked-release
build with Crown and the smoke/script gates passed. The unit gate ran 1,037 tests:
1,034 passed and three failed on all three attempts. Clippy reported four
`collapsible_if` errors. Tidy and unit doc tests did not run past those failures;
the missing merged timing artifact was secondary to the failed unit gate.

The earlier working assumption that repairing the first reported compiler/lint
errors would restore a validated inherited baseline was falsified. These later
failures were hidden behind those gates. All four nested-if blocks and the full
preference/test files involved are unchanged from frozen `e92cdaa`; this source
comparison establishes inherited scope, not a successful baseline CI run.

The repair preserves the fork's production defaults and the original assertions:

- `test_preferences_change` assumed grid was initially disabled, while the fork's
  `Preferences::const_default` enables it. Its first assertion expected empty
  declarations but actually received `1` and `3`. Explicitly disable the
  preference before opening the page, then retain the false-to-true transition,
  reload, and both assertions. This tests runtime changes without relying on an
  obsolete default. It is not evidence of a preference-propagation engine defect.
- The generic URL helper assumed DuckDuckGo but constructed default shell
  preferences, whose documented fork default is the RustyWeb search page. Supply
  the helper's explicit DuckDuckGo fixture to both command-line parser calls;
  keep every existing expected URL and input. A separate default-search regression
  exercises both command-line fallback and location-bar parsing against the
  fork's actual `http://theorem.local/search?q=%s` default. No parser or default
  production behavior changes.
- Collapse only the nested trigger-install, transaction commit/rollback, and
  localStorage deletion conditions into let chains. Preserve short-circuit order,
  errors, callbacks, and registry recovery; do not suppress the lints.

Rerun the exact hosted build, Clippy, Tidy, and full unit/doc gates for the new
head. The full unit and doc commands are:

```sh
./mach test-unit --profile checked-release --nextest-profile ci
./mach test-unit --profile checked-release --doc
```

Fast source/format checks do not replace these hosted gates, WPTs, or native
acceptance. CacheStorage remains the dummy baseline described above.


The repository Tidy route requires whole-workspace formatting with Servo's extra
rustfmt options (`binop_separator=Back`, `imports_granularity=Module`, and
`group_imports=StdExternalCrate`). The five Rust files in this repair now match
those options. Only the two already-affected IDB implementation files needed
inherited formatting adjustments; their pre-repair formatting hunks are preserved
separately in the execution evidence. Other repository files were not reformatted.

Local real Tidy was attempted through a private path alias because Mach rejects
spaces. It stopped at the missing `cargo-deny` executable before reaching its
formatting stage. An independent exact workspace formatting check identified
additional differences in 25 other files; full output and frozen-baseline
comparison are recorded in Theorem's execution evidence. These remain integration
obligations, not a successful full-Tidy result. No local engine build or test was
run for this repair.


### Bounded retained-patch formatting pass

The subsequent mechanical commit formats exactly the 25 outstanding files
identified by the exact workspace check. All belong to the retained downstream
patch range from upstream `1d44e5dd6a8b64c02f9dbf7fcbdf4ebdd0740019` to
frozen `e92cdaa790797479c1821c33470c64e0d166feb2`; upstream copies have no
formatter differences under these options. No whole-repository format rewrite
was applied. Functional repair and mechanical formatting are separate commits.

The exact workspace format check now passes. Fresh storage tests passed all 37
cases across `--lib --test main`, with no skipped tests. The working source diff
was unchanged before and after the test run. Provenance, bounded preview and
full logs are preserved in Theorem's migration execution evidence. This does
not establish a full Tidy pass: local `cargo-deny` remains unavailable. Hosted
Crown, Clippy, Tidy, unit/doc and required native/WPT gates must pass for the
new head before promotion; consumer pins remain unchanged.
