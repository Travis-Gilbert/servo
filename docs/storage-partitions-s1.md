# S1: Servo storage partitions implementation record

Date: 2026-10-26
Branch: `Travis-Gilbert/storage-partitions`
Base: `5c8325a389555f3b1adc476106d8529791afe13e`
Design: `/Volumes/SSD Samsung/var-18/Frame/arc-browser-platinum-planning-be32e2/docs/plans/servo-storage-partition.md`, section 3; acceptance risks in section 5.

Status: S1 passes all user-run suites: storage 25/25, net 441/441, libservo site-data/WebView 49/49, Vello CPU/GPU web-font tests 2/2, and Frame binary tests 78/78. After the final alias/entry cleanup, a full rerun covered the three suites under nextest (515/515), Frame's binary tests (78/78), and `cargo clippy --no-deps` over every crate S1 touched, with no warning on an S1-added line. The auth TODO remains upstream's, and service-worker fetch-event interception remains unverified. No cargo or mach build was run by this agent, no target directory was created, and nothing was committed or pushed. The `servo-mse` checkout was not modified.

## Public embedder API

Exports: `StoragePartitionId`, `StoragePartition`, `StoragePartitionError`.

- `Servo::default_storage_partition() -> StoragePartition`: preserves the default selection from `opts.config_dir`; `temporary_storage` makes that selection ephemeral.
- `Servo::storage_partition(StoragePartitionId) -> Option<StoragePartition>`: retrieve an already registered owner.
- `Servo::create_storage_partition(root: impl AsRef<Path>) -> Result<StoragePartition, StoragePartitionError>`: create/validate and canonicalize a persistent root, initialize its owners, and wait for constellation registration. A second owner for the same canonical root is rejected, including symlink aliases. Reuse a profile by cloning its handle.
- `Servo::create_ephemeral_storage_partition() -> Result<StoragePartition, StoragePartitionError>`: fresh temporary registry/IDB/Cache Storage root, nonpersistent Web Storage and network state, no HTTP disk cache.
- `StoragePartition::{id, root, site_data_manager, network_manager}`: inspect identity/canonical persistent root and obtain administration bound to that owner. `root()` is `None` for ephemeral partitions.
- `WebViewBuilder::storage_partition(StoragePartition) -> Self`: select immutable ownership. `try_build() -> Result<WebView, StoragePartitionError>` checks Servo ownership and popup inheritance. Existing `build() -> WebView` is retained and panics on an invalid selection.
- `WebView::storage_partition() -> StoragePartition`: retrieve the owning handle.

Errors: `Io(std::io::Error)`, `DuplicateRoot(PathBuf)`, `Registration(String)`, `WrongServo`, `PopupPartitionConflict`, `ShuttingDown`. `Io` retains its source error. Initialization/registration failure never selects another profile. Lazy database-open errors still belong to the existing IndexedDB operation protocol.

Handles follow the existing embedder-thread `Rc` model. Servo retains every registered partition until shutdown; dropping a handle neither unloads nor deletes its owner. There is no partition relabeling or unload API. Existing `Servo::{site_data_manager, network_manager}` remain default-partition facades. Cookie operation IDs and callbacks are shared across all facades to avoid reply collisions.

```rust
let personal = servo.create_storage_partition(profile_directory)?;
let private = servo.create_ephemeral_storage_partition()?;
let view = WebViewBuilder::new(&servo, rendering_context)
    .storage_partition(personal.clone())
    .try_build()?;
personal.site_data_manager().clear_cookies(None);
```

## Ownership and lifecycle

Each partition owns one resource core, HttpState, HTTP client/cache, FileManager, request interception/cancellation/preload state and service-worker mediator map; each also owns a registry, IndexedDB manager, Web Storage thread and Cache Storage thread. Existing factories and SQLite engines are reused. `StorageEngines` custom factories receive distinct supplied paths and must honor them rather than sharing a singleton backend.

Persistent roots keep existing network JSON and storage subtree formats. When the disk-cache preference is nonempty, Servo creates `http-cache/cache.sqlite3` beneath each persistent root, including the default root. The preference is an enable switch for the partition API, not a shared filename. This follows the design's explicit path policy; no migration from an old separately configured HTTP cache file is performed. Legacy low-level network constructors retain their explicit preference-path compatibility for existing tests.

WebViews, browsing context groups/contexts, pipelines and event loops retain immutable IDs. Resource/storage bundles carry matching IDs through documents, workers, worklets and debugger globals. Script envelopes carry that identity; diagnostic-only messages have no storage authority. Creator ownership metadata remains until shutdown for worker messages after document closure. BroadcastChannel and Web Locks registries are partition-selected, storage-event targets are filtered, service-worker managers are keyed by `(partition, origin)`, and SharedWorker matching includes the partition.

Auxiliary builders inherit the opener. Conflicting overrides fail. About:blank/srcdoc, sandboxed and cross-origin frames, history and noopener groups retain ownership. While waiting for popup creation, constellation processes embedder controls so synchronous partition registration from the popup callback cannot deadlock.

Shutdown joins script agents, stops and acknowledges service-worker managers after worker joins, then shuts down each partition's storage consumers before its registry and flushes that partition's network JSON. Global infrastructure and the async runtime stop once. Ephemeral directory guards are released on ShutdownComplete, even if the embedder retains a partition handle. Existing acknowledgement semantics are preserved: they do not guarantee that every blocked request or asynchronous SQLite/cache batch drains before shutdown.

CoreText and both Vello canvas backends bypass shared caches for URL-keyed Web font identifiers using their existing font construction paths. Local/system and uniquely identified array-buffer fonts retain caching.

## Change-map departures

All 31 mapped production files were changed. Additional files and reasons:

1. `components/script/messaging.rs`: the actual `ScriptThreadSenders` declaration lives here; it must carry the owner ID and use the updated sender alias.
2. `components/servo/webview_delegate.rs`: `CreateNewWebViewRequest` must carry the inherited handle into its builder.
3. `components/storage/lib.rs`: export the single-partition storage factory for libservo.
4. `components/storage/client_storage.rs` and `components/storage/cache_storage.rs`: replace `let _ = temp_dir` with a named binding in the thread closure so temporary roots are actually captured and retained. Backend algorithms are unchanged.
5. `components/servo/Cargo.toml` and `Cargo.lock`: add the already existing workspace `tempfile` dependency to Servo for owner-scoped temporary directory guards. No new package/version or engine was introduced.
6. The four test suites explicitly requested in the task, outside the production change map.
7. This implementation record, stored in the writable checkout; the external planning document was not edited.
8. `components/constellation/tracing.rs` (Round 2): the exhaustive embedder-message log-target match also needs the new registration variant.

Within the mapped files, Web Locks selection and cleanup live in constellation; the registry algorithm remains unchanged. Compatibility wrappers for the old public/private factories remain for existing low-level callers and tests, while Servo itself creates only the one default owner and explicitly registered partitions. No custom database engine, parser, HTTP client or cache algorithm was added. New tests use the existing test server, storage fixtures and tungstenite dependency.

## Base-fork fixes (separate from S1)

- `components/net/tests/interception.rs`: remove obsolete `::<_, ()>` arguments from `spawn_blocking_task`. Fork commit `d6428dcbcb7` used the former signature after upstream `7773388cbf0` removed a type parameter; this failure already existed at base `5c8325a`.
- `Cargo.lock`: retain the user's offline resolution of the `gstreamer-mse` packages missing at base `5c8325a`. The lockfile has 25 added lines overall: the missing `gstreamer-mse`/`gstreamer-mse-sys` package entries and media dependency account for 24; the remaining `tempfile` dependency line belongs to S1's existing-workspace dependency addition described above. No new media feature was introduced by S1.

## Acceptance coverage (user-run results recorded below)

| Risk | Added coverage |
| --- | --- |
| HTTP cache/state sharing | Different cookie-derived bodies at the same URL without Vary; same-owner channel reuse; redirects; memory validators/304 revalidation; concurrent requests; disk cache eviction, reopen and stale-entry revalidation; seeded auth-cache isolation, ephemeral absence and auth/HSTS persistence; WebSocket handshake cookies; preload response ownership. |
| Shared files/registry/IDB | Distinct canonical roots and registry bottle paths; duplicate/symlink root rejection; same-origin localStorage and real IDB values; fresh-process persistent reopen of localStorage, cookies and IDB; custom factory paths; IDB blocked upgrade versus another partition; queued IDB opens before registry exit. |
| Active communication | Positive same-partition and negative different-partition storage events, BroadcastChannel, Web Locks and SharedWorker identity; SharedWorker messaging/routing after creator closure. |
| Service workers | Identical scope URLs with distinct scripts and postMessage, checked again after both registrations exist; worker outgoing cookie-bearing fetches and Cache Storage ownership; persistent A and ephemeral B; live workers with blocked IndexedDB upgrades at shutdown. Service-worker fetch-event interception is unverified because this revision never activates installed workers; see Round 4. |
| Browsing context inheritance | about:blank popup storage inheritance and cookies after network navigation, srcdoc storage and cookie-bearing fetch, opaque sandbox iframe cookie fetch, conflicting popup override, noopener, cross-origin iframe/navigation, reload and back/forward. Navigation commands precede close commands on the ordered constellation stream. |
| Web fonts | Different existing WPT font bytes served at one cookie-dependent URL; canvas fill/stroke output differs between profiles and agrees within one profile. CPU and feature-gated GPU Vello cases; macOS also exercises CoreText. |
| Administration | Interleaved asynchronous cookie mutations/queries, partition-local cookie clearing, populated HTTP cache and local/session storage clearing with other-profile controls. |
| Ephemeral files | Ephemeral network owner ignores configured disk-cache path; temporary legacy storage ignores a persistent sentinel directory; ephemeral creation leaves persistent registry bytes unchanged; ephemeral APIs exercised alongside persistent partitions. A temporary-default plus explicit-ephemeral integration test exercises cookies, local/session storage, IDB, Cache Storage and fetch, then checks sentinel bytes and complete directory listings of a configured persistent profile after shutdown. |
| Shutdown | Per-partition storage ACK order preserved; queued registry work/IDB opens acknowledged; live service workers with blocked IDB upgrades in two partitions; durable completed writes checked in fresh processes. Tests cannot establish stronger draining guarantees than the existing storage/runtime contract. |
| Frame callback persistence | Outside this Servo checkout: Frame must still route delayed history/title/auth persistence using the originating tab's profile. |

Cache Storage in this fork exposes namespace operations and `Cache.keys()`, not `Cache.put()`/`match()`; its default backend is in-memory even with a persistent root. Service-worker registrations remain in memory, and the base manager does not activate installed workers. S1 isolates these existing implementations and supplies rooted paths to custom factories; it does not add missing persistence or a service-worker activation lifecycle. The tests exercise the available implementation. The manager's existing clear-site-data categories still do not promise complete IndexedDB/Cache Storage/service-worker deletion.

## Final validation

These execution results were supplied by the user:

| Suite | Command | Result |
| --- | --- | --- |
| Storage | `cargo nextest run -p servo-storage --test main` | 25/25 pass |
| Network | `cargo nextest run -p servo-net --test main` | 441/441 pass |
| Site-data and WebView | `cargo nextest run -p servo --test site_data_manager --test webview` | 49/49 pass |
| Vello web fonts, CPU and GPU | `cargo nextest run -p servo --test webview --features vello` with the web-fonts tests selected | 2/2 pass |
| Frame lane | Frame binary tests against this Servo tree | 78/78 pass |

The general Servo suite command is `cargo nextest run -p <crate> --test <name>`. Nextest is required for libservo's tests because each test needs its own process; plain cargo test shares process-global Opts and fails with "Opts already initialized".

The user also ran `cargo clippy --no-deps` over the S1-touched crates, filtered to added lines, and reported two warnings in constellation. Final cleanup introduces a nearby `ScriptMessage` alias for the script channel payload and uses the map entry API when registering a partition. This preserves duplicate rejection and existing ownership/shutdown checks. No warning suppression was added.

Agent validation: edition-2024 rustfmt on changed Rust code and `git diff --check` passed. No cargo, nextest, clippy or mach commands were run by the agent; the final alias/entry cleanup has source and formatting verification, not a fresh compiler/clippy run. All 31 production change-map entries are covered. The user will commit.

Scope limits remain explicit: Fetch step 17's auth-cache population TODO #33616 stays upstream's, avoiding new plaintext credential persistence; the auth test seeds only synthetic existing state. Service-worker fetch-event interception is unverified because this Servo revision never activates installed workers. The passing worker tests cover registration, messages, outgoing fetches, storage ownership and shutdown, not interception.

## S1 files changed by component

### Embedder API

- `components/servo/Cargo.toml`
- `components/servo/lib.rs`
- `components/servo/network_manager.rs`
- `components/servo/servo.rs`
- `components/servo/site_data_manager.rs`
- `components/servo/tests/site_data_manager.rs`
- `components/servo/tests/webview.rs`
- `components/servo/webview.rs`
- `components/servo/webview_delegate.rs`

### Constellation

- `components/constellation/broadcastchannel.rs`
- `components/constellation/browsingcontext.rs`
- `components/constellation/constellation.rs`
- `components/constellation/constellation_webview.rs`
- `components/constellation/event_loop.rs`
- `components/constellation/logging.rs`
- `components/constellation/pipeline.rs`
- `components/constellation/tracing.rs`
- `components/constellation/web_locks.rs`

### Script and workers

- `components/script/dom/globalscope/globalscope.rs`
- `components/script/dom/serviceworker/serviceworker_manager.rs`
- `components/script/dom/workers/sharedworker.rs`
- `components/script/event_loop/script_thread.rs`
- `components/script/event_loop/script_window_proxies.rs`
- `components/script/messaging.rs`

### Shared contracts

- `components/shared/base/id.rs`
- `components/shared/constellation/from_script_message.rs`
- `components/shared/constellation/lib.rs`
- `components/shared/embedder/lib.rs`
- `components/shared/net/lib.rs`
- `components/shared/storage/lib.rs`

### Network

- `components/net/disk_cache.rs`
- `components/net/http_cache.rs`
- `components/net/resource_thread.rs`
- `components/net/tests/resource_thread.rs`

### Storage

- `components/storage/cache_storage.rs`
- `components/storage/client_storage.rs`
- `components/storage/lib.rs`
- `components/storage/storage_thread.rs`
- `components/storage/tests/storage_thread.rs`

### Font and canvas caches

- `components/canvas/vello_backend.rs`
- `components/canvas/vello_cpu_backend.rs`
- `components/fonts/platform/macos/core_text_font_cache.rs`

### Workspace dependency and record

- `Cargo.lock`
- `docs/storage-partitions-s1.md`

## Historical build follow-ups

The intermediate results below are superseded by Final validation above.

### Round 2

User-reported build: Frame lane `/Volumes/SSD Samsung/frame-worktrees/storage-partitions`, using this checkout through `deps/servo-partitions/components/servo`; features `webcrypto`, `webgl`, `brotli-compression-stream`, `media-gstreamer`. The user runs builds. Their Round 1 documentation additions to `HttpCache::with_disk_cache` and `try_with_disk_cache` were preserved unchanged.

Changes in this follow-up:

- `components/constellation/tracing.rs`: add `RegisterStoragePartition { .. } => target!("RegisterStoragePartition")` to fix E0004.
- `components/constellation/constellation.rs`: remove S1's unused `GenericSend` import. The base's resource shutdown called `ResourceThreads::send` through this trait; S1 calls the core channel directly.
- `components/script/messaging.rs`: remove S1's unused `WebViewId` import. The base spelled out the sender tuple here; S1 uses the shared sender alias.
- This record: capture the build evidence, base comparison and shutdown/API audit.

The four remaining constellation storage-message warning groups (`CacheStorageThreadMessage`, `ClientStorageThreadMessage`, `IndexedDBThreadMsg`/`SyncOperation`, `WebStorageThreadMsg`) are import-only in base `5c8325a` too. That revision already delegates storage shutdown to `StorageThreads::exit()`. They were intentionally left unchanged under the request to preserve pre-existing warnings. The other five S1-touched script files were checked against the base; their semantic changes add used ownership fields, validation and message envelopes, and no further S1-introduced warning source was identified. The complete 25-warning build log was not supplied, so this is source review rather than a claim that a fresh warning-free build ran.

Shutdown rechecked at both send and receive sides: constellation iterates every registered partition and calls `StorageThreads::exit()`. That method sends and awaits IndexedDB exit, Web Storage exit, Cache Storage exit, then client-storage exit; client storage is the registry owner, not a separate fifth thread. All four receivers handle and acknowledge their exit messages. A failed consumer exit does not skip later exit attempts. Service-worker joins/acknowledgements precede the partition loop, network JSON flush/exit follows each storage group, and global runtime shutdown remains outside the loop. Failed registration/initialization also uses the same storage-exit method. No exit path needed restoration.

Look-ahead review found no additional concrete libservo compilation failure. Frame's `src/page.rs` builder chain and `src/engine.rs` cookie query remain compatible. `WebViewBuilder::new(&Servo, Rc<dyn RenderingContext>) -> Self`, `build(self) -> WebView`, `Servo::site_data_manager(&self) -> &SiteDataManager`, and `Servo::network_manager(&self) -> Ref<NetworkManager>` retain their signatures; default-partition selection is internal. No Frame file was changed.

Validation: edition-2024 rustfmt on the three Rust files edited in this follow-up and `git diff --check`. No cargo, mach, compilation or tests were run by this agent. Subsequent user builds and the final suites confirmed these fixes.

### Round 3

User-reported evidence: Frame builds and all 78 binary tests pass. The Servo storage integration suite reports 23 passes and two failures, `webstorage::no_storage_type_conflict` and `webstorage::origin_descriptors_local`. Both pass in the user's clean base worktree, whose storage suite reports 21 passes. All four new S1 storage tests pass. The net integration target reports two compilation errors.

Root cause: S1 changed the first element of the legacy `new_storage_threads` pair to use `config_dir = None` and `ephemeral = true` unconditionally. `WebStorageTest` uses that first element, including after restart. Consequently its local data and origin descriptors were in memory, despite passing a persistent root. This was not a missing shutdown flush: Web Storage commits SQLite mutations and saves local origin metadata eagerly.

Changes:

- `components/storage/storage_thread.rs`: restore the configured root for both legacy groups when `temporary_storage` is false. Explicitly temporary legacy groups still ignore the supplied persistent directory. Explicit partition creation in libservo continues to call `new_partition_storage_threads` directly, so its separate roots and ephemeral behavior are unaffected. The legacy tuple order and existing test expectations are unchanged.
- `components/net/tests/resource_thread.rs`: clone each `ResourceThreads` handle before moving it into its request thread, then borrow that owned clone inside the thread. Both partitions still issue concurrent requests; no shared `&ResourceThreads` must implement `Send`.
- `components/net/tests/interception.rs`: remove the obsolete `::<_, ()>` from `spawn_blocking_task`. This is a pre-existing fork fix outside the S1 change map: fork commit `d6428dcbcb7` used the former signature after upstream `7773388cbf0` removed a type parameter.
- This record: update build evidence and document these fixes. The user's 25-line `Cargo.lock` resolution diff is preserved untouched.

Validation: edition-2024 rustfmt on the three edited Rust files and `git diff --check`. No cargo or tests were run by this agent. The existing two restart tests cover the production regression without changing their helper or assertions. No additional definite compile failure was identified by source review. One runtime concern to watch in the net suite is process-global preference mutation: existing cache/file-manager tests and the new resource-owner tests set preferences, so parallel execution can interfere with cache capacity or disk-path assumptions. The final nextest results above supersede this intermediate source-only status.

### Round 4

User-reported results: storage 25/25; net 439/441; site-data 11/11; WebView 35/38 (two failures, one timeout). Every pre-existing test passes. The user runs Servo suites through nextest, one process per test, as mach does. This resolves the process-global preference concern noted in Round 3; plain cargo test is unsuitable for libservo tests because Opts initializes once per process.

Root causes and fixes:

1. HTTP authentication: **test assumption outside S1's scope**. The prompted navigation succeeded, but the test then assumed it would populate the cache. Upstream Fetch step 17 is still TODO #33616. The initial Round 4 production implementation was reverted at the user's request: populating that cache would add a feature and cause successful Basic-auth credentials to be written in plaintext to each persistent root's `auth_cache.json` at shutdown. Frame stores credentials in the macOS Keychain. The renamed test now seeds the existing startup JSON format with distinct synthetic credentials for the same origin, checks each persistent partition's complete Authorization header, and checks that an ephemeral partition sends none. No production cache-population behavior is added.
2. WebSocket handshake: **test setup**. The test passed an `http:` request URL to `websocket_connect`, whose contract accepts `ws:`/`wss:` and converts them to HTTP Fetch schemes. Its non-`ws` branch changed this to HTTPS, then the loader upgraded the original `ws:` URL to `wss:`. The test's plaintext tungstenite server consequently parsed a TLS preamble as HTTP. Pass `ws_url` as the request URL and retain the HTTP document origin. Cookie/header contents were not the cause.
3. Web fonts: **test setup**. `dom_fontface_enabled` defaults to false. Enable it in the shared font test helper, covering Vello CPU, feature-gated Vello GPU and their macOS CoreText paths.
4. Inheritance/cookies: **test expectation**. The failing assertion was the srcdoc iframe, before popup creation. `Document::is_cookie_averse()` treats non-network document URLs as cookie-averse, so `about:srcdoc` returns an empty `document.cookie` even with the correct partition. Keep inherited localStorage coverage and assert cookie ownership using a fetch from the srcdoc child. Likewise, check initial about:blank popup local/session storage, then its cookies after network navigation. Parent same-origin cookie assertions and sandboxed network iframe checks remain. No ownership fallback or cookie-set ordering defect was found.
5. Service workers: **test assumption about an unavailable base lifecycle**. The pref was already enabled, and localhost is trustworthy. The base manager installs a worker, assigns it to waiting, but never calls the Active transition; DOM worker state also stays Installing. Waiting for activated could therefore never finish. Use the installed worker's supported message path and verify both same-scope registrations again after both exist. Each worker checks its own and the other profile's cache namespace and performs an outgoing cookie-bearing fetch. Shutdown now retains two live workers with explicitly blocked IndexedDB upgrades, rather than relying on unimplemented activation/interception. This does not claim fetch-event interception coverage or implement a new activation lifecycle.

The partition async helper now bounds page loading, evaluation callbacks and promise polling with an eight-second deadline. Service-worker setup asserts secure context/API/worker availability, message replies have a four-second timeout, and worker failures are returned to the assertion. These limits precede the reported 20-second nextest timeout and identify the stalled operation.

Files changed this round: `components/net/http_loader.rs`, `components/net/tests/resource_thread.rs`, `components/servo/tests/webview.rs`, and this record. Edition-2024 rustfmt was applied to the changed Rust code; unrelated formatting in the newly touched HTTP loader was preserved. `git diff --check` passed. No cargo, nextest or mach commands ran; the user's 25-line lockfile diff remains untouched. The final nextest rerun passed after the auth scope correction below.


### Round 4 auth scope correction

Restored `components/net/http_loader.rs` exactly to its pre-Round-4 S1 state (identical to base for this file), including Fetch step 17's `TODO(#33616)`. S1 isolates existing auth state; it does not implement successful-retry cache population or introduce automatic plaintext password persistence. The other four Round 4 fixes are retained.

Renamed the test to `test_partition_seeded_auth_cache_is_isolated_and_ephemeral_has_none`. It writes different synthetic username/password entries under the same origin key into two temporary persistent profile roots before constructing their resource owners, using Servo's existing `AuthCache` serialization. The HTTP server echoes all Authorization header values; repeated interleaved requests must match only the respective profile's header. An ephemeral owner must send no Authorization header before and after those requests. Existing startup loading and cache lookup support this without production changes.

Service-worker fetch-event interception remains **unverified**: this Servo revision never activates installed workers. Registration, message routing, outgoing worker fetches, Cache Storage ownership and shutdown coverage do not establish fetch-event interception.

Files changed in this correction: `components/net/http_loader.rs` (reverted; no remaining git diff), `components/net/tests/resource_thread.rs`, and this record. Edition-2024 rustfmt applied to the rewritten test; `git diff --check` passed. No cargo, mach, nextest or commit commands ran. The lockfile and the other Round 4 fixes were left unchanged.
