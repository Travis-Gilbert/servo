# Storage-engine selection commit adaptation

The replayed fork commit `a6537f17c1f8` removes the public `StorageEngines` and
embedder factory seams. That removal is intentionally not carried into this
Servo v0.7 candidate. The browser upgrade must preserve the four-operation
object-safe CacheStorage contract and optional factory injection so embedders
can keep one authoritative backend; the built-in v0.7 in-memory CacheStorage
engine remains the default when no factory is supplied. This also preserves the
existing IndexedDB and WebStorage factory seams and their independent state
ownership.

The commit therefore remains partially adapted: changes that only rewrite
constructor call sites to remove injection are omitted, and its dummy-cache
claims do not describe this candidate. The default factory and custom-factory
four-operation regression oracles remain in
`components/storage/tests/storage_thread.rs`. Constructor updates that require
the removed API are reverted alongside the source changes. No engine tests or
builds have been run during source replay.

Before any consumer update, compile the focused storage crate, run the default
and custom factory tests, run the storage integration target, and execute the
selected IndexedDB, WebStorage, and Cache API WPTs. A source replay or diff check
alone is not behavioral acceptance.

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
