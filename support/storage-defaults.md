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

Hosted lint run `37706099233` at `5920dcccede120f5570496cbc4f25114a9993177`
then exposed an inherited `clone_on_copy` in constellation's random-pipeline
stress path, unchanged at the same line in frozen `e92cdaa`. Passing the
already-`Copy` pipeline ID directly preserves the stress behavior and removes
the redundant clone. This is a separate maintenance commit; the hosted gates
must be rerun for its exact head before promotion.
