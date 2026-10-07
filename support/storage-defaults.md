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
