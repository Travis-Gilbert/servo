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
