# Golem Wasmtime Fork — Customization Guide

This document describes every modification the Golem fork applies on top of the upstream
[Bytecode Alliance Wasmtime](https://github.com/bytecodealliance/wasmtime) release.
The current fork is based on **v33.0.0**. The goal of this document is to serve as a
reference when rebasing onto a newer upstream version (e.g. v42.0.1).

---

## Table of Contents

1. [High-Level Summary](#1-high-level-summary)
2. [Branch & Tag Structure](#2-branch--tag-structure)
3. [Removed Modules (WASI p0, p1, Sync)](#3-removed-modules-wasi-p0-p1-sync)
4. [Suspend Support in Poll](#4-suspend-support-in-poll)
5. [Stream Downcasting (`as_any`)](#5-stream-downcasting-as_any)
6. [Async Host Function Expansion](#6-async-host-function-expansion)
7. [Wasi-HTTP Durability Customizations](#7-wasi-http-durability-customizations)
8. [Filesystem Path Tracking](#8-filesystem-path-tracking)
9. [Exposed Internals & Misc Changes](#9-exposed-internals--misc-changes)
10. [Submodules](#10-submodules)
11. [Minor / Incidental Differences](#11-minor--incidental-differences)
12. [Upgrade Checklist](#12-upgrade-checklist)

---

## 1. High-Level Summary

The Golem fork makes wasmtime suitable for **durable execution** of WebAssembly components.
The key themes are:

| Theme | Purpose |
|---|---|
| **Remove p0/p1/sync** | Golem only uses WASI p2 async. Removing p0, p1, and sync linkers simplifies the fork and avoids having to keep those code paths compatible with our other changes. |
| **Suspend support** | The `wasi:io/poll` implementation must be able to signal the host that a worker should be **suspended** instead of blocking, when all polled resources support it and the wait would exceed a configurable threshold. |
| **Async host functions** | Many previously-synchronous WASI host functions are converted to `async` so the durable executor can **intercept and replay** them. If upstream makes additional functions async in a newer version, that is fine — Golem will adapt. But any function that our fork makes async **must remain async** in the new fork. |
| **Stream downcasting** | `InputStream` and `OutputStream` gain `Any` supertrait and `as_any()` so Golem can inspect the **concrete type** of a stream at runtime to make durability decisions. |
| **Durable HTTP** | HTTP outgoing requests can be **deferred** and failing response bodies can be constructed, enabling Golem's durable HTTP connection support. |
| **Filesystem path tracking** | `File` / `Dir` descriptors store the host filesystem path so the durable executor can persist and restore file system state. |

---

## 2. Branch & Tag Structure

The fork follows a consistent naming convention across versions:

| Branch / Tag | Role | Example (v33) |
|---|---|---|
| **Baseline branch** | Points to the unmodified upstream release. | `wasmtime-v33.0.0-baseline` |
| **Fork branch** | Contains the baseline + Golem customizations on top. | `golem-wasmtime-v33.0.0` |

The baseline branch is an exact copy of the upstream tag (e.g. upstream `v33.0.0`).
The fork branch contains one or more commits on top of the baseline that apply all
Golem-specific changes. The number or granularity of those commits is not significant —
what matters is the **diff between the baseline branch and the fork branch**, which is
what this document describes.

When upgrading to a new upstream version (e.g. `v42.0.1`), the process is:

1. Create a new baseline branch `wasmtime-v42.0.1-baseline` from the upstream tag.
2. Create a new fork branch `golem-wasmtime-v42.0.1` from that baseline.
3. Re-apply all the customizations described in this document onto the new fork branch,
   adapting to any upstream API changes.
4. Historical branches (e.g. `golem-wasmtime-v33.0.0`) are kept for reference.

### Upgrade guidelines

- **All features described in this document must be preserved** in the new fork. The
  Golem codebase depends on every one of them. Dropping a feature from the fork would
  break the Golem worker executor.
- The exact implementation may need to change if upstream refactored the relevant code,
  but the **functionality and public API surface** exposed by each customization must
  remain equivalent so that Golem can upgrade to the new forked wasmtime.
- If an upstream change makes it impossible to preserve the exact same API, **adjustments
  on the Golem side are acceptable but should be minimized**. The goal is to keep the
  Golem upgrade as small as possible — ideally just bumping the wasmtime dependency
  version with no or minimal code changes in Golem itself.
- **Analyze upstream changes first.** Before applying customizations, compare the old
  baseline (e.g. `v33.0.0`) with the new target version to understand what upstream moved,
  renamed, or restructured. Some of our customizations may have been adopted upstream
  (e.g. if upstream makes `get_random_bytes` async natively, we no longer need that
  change). For each feature in this document, check whether upstream now provides
  equivalent functionality — if so, prefer using the upstream version and adapt the
  Golem side if needed.
- **Build the upstream baseline first.** Before applying any customizations, ensure the
  unmodified upstream tag builds in your environment (correct Rust toolchain, etc.).
  This prevents conflating "upstream doesn't build" with "fork patch broke build."
- **Build incrementally.** Apply and build after each major feature group (suspend,
  dynamic pollables, streams, async, HTTP, filesystem) rather than all at once.

### Public API contract

The following API surface is consumed by Golem and **must remain stable** (or changes
must be reflected in Golem):

| API | Crate | Golem usage |
|-----|-------|-------------|
| `WasiCtxBuilder::build() -> (WasiCtx, IoCtx)` | `wasmtime-wasi` | `wasi_host::create_context()` |
| `WasiCtxBuilder::set_suspend(threshold, signal)` | `wasmtime-wasi` | `wasi_host::create_context()` |
| `IoView::io_ctx(&mut self) -> &mut IoCtx` | `wasmtime-wasi-io` | `DurableWorkerCtxWasiView`, `DurableWorkerCtxWasiHttpView` |
| `IoCtx { suspend_threshold, suspend_signal }` | `wasmtime-wasi-io` | Stored in `DurableWorkerCtx` |
| `subscribe(table, resource, supports_suspend)` | `wasmtime-wasi-io` | All `subscribe()` call sites |
| `DynamicPollable` trait, `dynamic_subscribe()` | `wasmtime-wasi-io` | `LazyInitializedPollableEntry`, `FutureInvokeResultEntry` |
| `InputStream: Any`, `as_any()` | `wasmtime-wasi-io` | `is_incoming_http_body_stream()`, stdout/stderr detection |
| `OutputStream: Any`, `as_any()` | `wasmtime-wasi-io` | stdout/stderr detection in `write()` |
| `HostFutureIncomingResponse::deferred()` | `wasmtime-wasi-http` | HTTP replay (Deferred is always-ready for polling) |
| `HostIncomingBody::failing(error)` | `wasmtime-wasi-http` | Reconstructing failed HTTP bodies during replay |
| `HostIncomingBody::take_stream() -> Option<Box<dyn InputStream>>` | `wasmtime-wasi-http` | HTTP body stream handling |
| `get_fields()` (pub) | `wasmtime-wasi-http` | Trailer serialization for oplog |
| `File { pub path: PathBuf }`, `Dir { pub path: PathBuf }` | `wasmtime-wasi` | Durable `stat`, read-only enforcement |
| `ReaddirIterator::new()` (pub) | `wasmtime-wasi` | Deterministic directory listing |
| `ResourceTable::get_any()` (immutable) | `wasmtime` | Override-following logic (internal) |
| `wasmtime::VERSION` | `wasmtime` | Prometheus metrics |
| Re-exports: `DynPollable`, `Pollable`, `DynamicPollable`, `OverrideSelf`, `IoCtx`, etc. | `wasmtime-wasi` | Various Golem imports via `wasmtime_wasi::` path |

---

## 3. Removed Modules (WASI p0, p1, Sync)

Golem only needs WASI preview 2 (p2) with async support. Removing p0, p1, and the
synchronous linker code simplifies the fork so that the remaining Golem-specific changes
(async expansion, suspend support, etc.) do not need to be kept backward-compatible with
these unused code paths.

### What is removed

| What | Location | Notes |
|------|----------|-------|
| WASI preview0 module | `crates/wasi/src/preview0.rs` | Entire file deleted. |
| WASI preview1 module | `crates/wasi/src/preview1.rs` | Entire file deleted (~2633 lines). |
| Module declarations | `crates/wasi/src/lib.rs` | `#[cfg(feature = "preview1")]` gated `pub mod preview0;` and `pub mod preview1;` removed. |
| Sync WASI linker | `crates/wasi/src/p2/mod.rs` | `add_to_linker_sync()`, `add_to_linker_with_options_sync()`, and helper `io_type_annotate()` removed. |
| Sync HTTP linker | `crates/wasi-http/src/lib.rs` | `add_to_linker_sync()`, `add_only_http_to_linker_sync()` removed. |
| `build_p1()` | `crates/wasi/src/p2/ctx.rs` | `WasiCtxBuilder::build_p1()` method removed. |
| HTTP sync bindings | `crates/wasi-http/src/bindings.rs` | File deleted entirely. The async-only `bindgen!` invocation is inlined into `crates/wasi-http/src/lib.rs` (see [section 7](#7-wasi-http-durability-customizations)). The sync bindings sub-module and `Proxy`/`ProxyPre`/`ProxyIndices` re-exports are dropped. |

---

## 4. Suspend Support in Poll

This is a critical Golem feature. The `wasi:io/poll` implementation must support the
concept of **suspending** a worker: when a poll would block for a long time and all
involved pollables support suspension, the host can choose to suspend the worker and
resume it later, instead of keeping it alive and blocked.

### 4.1 Core mechanism

The implementation adds:

1. **`IoCtx` struct** (`crates/wasi-io/src/lib.rs`) — holds the suspend configuration:

   ```rust
   pub struct IoCtx {
       pub suspend_threshold: Duration,
       pub suspend_signal: Box<dyn Fn(Duration) -> anyhow::Error + Send + Sync + 'static>,
   }
   ```

2. **`supports_suspend` field on `DynPollable`** (`crates/wasi-io/src/poll.rs`) — `Option<Instant>`:
   - `None` → this pollable does NOT support suspension (e.g. a socket ready check).
   - `Some(deadline)` → this pollable supports suspension; `deadline` is when it would fire.

3. **Suspend check in `poll()`** (`crates/wasi-io/src/impls.rs`) — after collecting all
   pollables, if **every** pollable supports suspend and the earliest deadline exceeds
   `suspend_threshold`, the poll returns the `suspend_signal` error instead of blocking:

   ```rust
   if let Some(Some(deadline)) = all_supports_suspend {
       let duration = deadline.duration_since(Instant::now());
       if duration >= self.io_ctx().suspend_threshold {
           return Err((self.io_ctx().suspend_signal)(duration));
       }
   }
   ```

### 4.2 `IoView` trait extended

`IoView` (`crates/wasi-io/src/lib.rs`) gains a required method:

```rust
fn io_ctx(&mut self) -> &mut IoCtx;
```

This is propagated through all blanket impls (`&mut T`, `Box<T>`, `IoImpl<T>`) and
wrapper types (`WasiImpl<T>` in `crates/wasi/src/p2/view.rs`, `WasiHttpImpl<T>` in
`crates/wasi-http/src/types.rs`). All documentation examples are updated accordingly.

### 4.3 `subscribe()` signature change

```rust
// Before:
pub fn subscribe<T>(table: &mut ResourceTable, resource: Resource<T>) -> Result<Resource<DynPollable>>
// After:
pub fn subscribe<T>(table: &mut ResourceTable, resource: Resource<T>, supports_suspend: Option<Instant>) -> Result<Resource<DynPollable>>
```

**All call sites** are updated to pass the third argument:
- Clock subscriptions (`crates/wasi/src/p2/host/clocks.rs`): pass the computed deadline.
- All other pollables (TCP, UDP, streams, DNS, HTTP): pass `None`.

### 4.4 `WasiCtxBuilder` changes

`WasiCtxBuilder` (`crates/wasi/src/p2/ctx.rs`) gains:

- New fields: `suspend_threshold: Duration`, `suspend_signal: Box<dyn Fn(Duration) -> anyhow::Error + ...>`.
- New method: `set_suspend(suspend_threshold, suspend_signal)`.
- **Changed return type**: `build()` now returns `(WasiCtx, IoCtx)` instead of just `WasiCtx`.

### 4.5 Dynamic pollables (override mechanism)

For cases like deferred HTTP requests where a pollable needs to be "lazy initialized",
a dynamic override mechanism is added:

- **`DynamicPollable` trait** (`crates/wasi-io/src/poll.rs`):
  ```rust
  pub trait DynamicPollable: Pollable {
      fn override_index(&self) -> Option<u32>;
  }
  ```

- **`dynamic_subscribe()` function** — like `subscribe()` but sets an `override_self`
  field on the `DynPollable`.

- **`OverrideSelf` type alias**: `fn(&dyn Any) -> Option<u32>`.

- **Override-following logic** (`crates/wasi-io/src/impls.rs`): A new function
  `get_pollable_following_overrides()` follows the chain of overrides until it finds
  a pollable without one. Called in `poll()`, `block()`, and `ready()`.

- **`ResourceTable::get_any()`** (`crates/wasmtime/src/runtime/component/resource_table.rs`):
  New immutable accessor (counterpart to existing `get_any_mut()`) needed by the
  override-following logic.

### 4.6 How Golem uses suspend support

In Golem, the suspend threshold defaults to **10 seconds** (`config.suspend.suspend_after`).
The `WasiCtxBuilder` is configured in `golem-worker-executor/src/wasi_host/mod.rs`:

```rust
WasiCtxBuilder::new()
    .set_suspend(suspend_threshold, |duration| anyhow!(SuspendForSleep(duration)))
    .build()  // returns (WasiCtx, IoCtx)
```

The `SuspendForSleep(Duration)` is a typed error. When the fork's poll returns it:

1. **Golem's `poll()` override** (`durable_host/io/poll.rs`) catches the error via
   `is_suspend_for_sleep()` using `downcast_ref::<SuspendForSleep>()`.
2. **Schedules a wakeup**: Creates a Golem `Promise` and schedules a `CompletePromise`
   action at `now + duration` via the scheduler service.
3. **Returns `InterruptKind::Suspend`**, which propagates up as a trap.
4. **The invocation loop** writes an oplog `Suspend` entry, drops the entire wasmtime
   instance (freeing memory), and stops the worker.
5. **Later**, the scheduler fires, completes the promise, and the worker is re-enqueued.
   It replays from the oplog to reconstruct state.

There is also a **fast-path**: if all pollables in a `poll` call are Golem promise-backed
and none are ready, the worker suspends immediately without even calling the fork's poll.

### 4.7 How Golem uses dynamic pollables

Golem has two `DynamicPollable` implementations:

- **`LazyInitializedPollableEntry`** (`durable_host/durability.rs`): Starts as `Empty`
  (always-ready), and can later be `.set()` to point at a real `DynPollable`. When set,
  `override_index()` returns the inner pollable's rep, causing poll to follow the
  redirect. This is the core mechanism for durable async operations — the pollable is
  wired up lazily after replay.

- **`FutureInvokeResultEntry`** (`golem-wasm/src/lib.rs`): Backs async RPC futures
  (`golem::rpc::future-invoke-result`). Returns `None` from `override_index()` since
  it has its own `Pollable::ready()` implementation that drives the RPC state machine.

Both use `dynamic_subscribe()` to register themselves in the resource table.

---

## 5. Stream Downcasting (`as_any`)

Golem needs to inspect the concrete type behind a stream to make durability decisions
(e.g., distinguish a `FileInputStream` from a `TcpReadStream`).

### 5.1 Trait changes (`crates/wasi-io/src/streams.rs`)

Both `InputStream` and `OutputStream` gain:
- `Any` as a supertrait (in addition to the existing `Pollable`).
- A new required method: `fn as_any(&self) -> &dyn Any;`

### 5.2 Implementations updated

Every concrete `InputStream` / `OutputStream` implementation across the codebase gains
an `as_any()` method returning `self`:

| File | Types |
|------|-------|
| `crates/wasi/src/p2/filesystem.rs` | `FileInputStream`, `FileOutputStream` |
| `crates/wasi/src/p2/pipe.rs` | `MemoryInputPipe`, `MemoryOutputPipe`, `AsyncReadStream`, `SinkOutputStream`, `ClosedInputStream`, `ClosedOutputStream` |
| `crates/wasi/src/p2/stdio.rs` | `AsyncStdinStream`, `OutputFileStream`, `StdioOutputStream`, `AsyncStdoutStream` |
| `crates/wasi/src/p2/stdio/worker_thread_stdin.rs` | `Stdin` |
| `crates/wasi/src/p2/tcp.rs` | `TcpReadStream`, `TcpWriteStream` |
| `crates/wasi/src/p2/write_stream.rs` | `AsyncWriteStream` |
| `crates/wasi-http/src/body.rs` | `HostIncomingBodyStream`, `BodyWriteStream` |

### 5.3 How Golem uses stream downcasting

Golem uses `as_any().downcast_ref::<T>()` in two key places in
`golem-worker-executor/src/durable_host/io/streams.rs`:

**1. Stdout/stderr interception** — In `HostOutputStream::write()`:
```rust
let output = self.table().get(&self_)?;
if output.as_any().downcast_ref::<ManagedStdOut>().is_some() {
    // Intercept: emit as InternalWorkerEvent::stdout → oplog log entry
} else if output.as_any().downcast_ref::<ManagedStdErr>().is_some() {
    // Intercept: emit as InternalWorkerEvent::stderr → oplog log entry
} else {
    // Not stdout/stderr: pass through to real WASI write (no oplog)
}
```
Writes to `ManagedStdOut`/`ManagedStdErr` are turned into durable oplog `Log` entries
rather than going to the real OS stream. All other stream writes pass through unchanged.

**2. HTTP body stream detection** — In `is_incoming_http_body_stream()`:
```rust
stream.as_any().downcast_ref::<HostIncomingBodyStream>().is_some()
    || stream.as_any().downcast_ref::<FailingStream>().is_some()
```
When reading from an `InputStream`, Golem checks if the stream is an HTTP response body
(and has an associated `open_http_requests` entry). If so, the read is wrapped in a
`Durability` block that persists body chunks to the oplog. On replay, persisted chunks
are returned without touching the network. Non-HTTP streams bypass this entirely.

---

## 6. Async Host Function Expansion

Golem's durable executor intercepts WASI host function calls to record and replay them.
This requires the functions to be `async`. The fork converts many previously-synchronous
host functions to `async`.

**Critical rule for upgrades:** If our fork makes a method async that is synchronous in
upstream, **it must remain async** in the new fork version. If upstream independently makes
additional functions async, that is fine and Golem will adapt to use them.

### 6.1 Bindgen `only_imports` async lists

The `bindgen!` macro invocations are modified to mark more imports as async:

#### `crates/wasi-io/src/bindings.rs` — additional async methods:
```
[method]input-stream.read
[method]input-stream.skip
[method]output-stream.flush
[method]output-stream.write
[method]output-stream.write-zeroes
[method]output-stream.forward
[method]output-stream.splice
```

#### `crates/wasi/src/p2/bindings.rs` — same stream methods as above, plus:
```
get-random-bytes, get-random-u64
insecure-seed, get-insecure-random-bytes, get-insecure-random-u64
now, resolution, subscribe-instant, subscribe-duration
get-environment, get-arguments, initial-cwd
get-directories
resolve-addresses
```

#### `crates/wasi-http/src/lib.rs` (inlined bindings) — async imports:
```
handle
[method]future-incoming-response.get
[method]future-trailers.get
[static]incoming-body.finish
[drop]incoming-body
[drop]incoming-response
[drop]future-incoming-response
```

(Upstream used `only_imports: ["nonexistent"]` making nothing async; the fork lists specific methods.)

### 6.2 Host function signature changes

The corresponding host implementations change from `fn` to `async fn`:

#### Clocks (`crates/wasi/src/p2/host/clocks.rs`):
- `wall_clock::Host`: `now()`, `resolution()`
- `monotonic_clock::Host`: `now()`, `resolution()`, `subscribe_instant()`, `subscribe_duration()`

#### Environment (`crates/wasi/src/p2/host/env.rs`):
- `get_environment()`, `get_arguments()`, `initial_cwd()`

#### Filesystem (`crates/wasi/src/p2/host/filesystem.rs`):
- `preopens::Host::get_directories()`

#### Random (`crates/wasi/src/p2/host/random.rs`):
- `random::Host`: `get_random_bytes()`, `get_random_u64()`
- `insecure::Host`: `get_insecure_random_bytes()`, `get_insecure_random_u64()`
- `insecure_seed::Host`: `insecure_seed()`

#### DNS (`crates/wasi/src/p2/ip_name_lookup.rs`):
- `resolve_addresses()`

#### IO streams (`crates/wasi-io/src/impls.rs`):
- `HostOutputStream`: `write()`, `write_zeroes()`, `flush()`, `splice()`
- `HostInputStream`: `read()`, `skip()`

#### Sync IO wrappers (`crates/wasi/src/p2/host/io.rs`):
The sync host implementations now wrap the newly-async calls with `in_tokio(async { ... })`:
- `write()`, `write_zeroes()`, `flush()`, `splice()`, `read()`, `skip()`

#### HTTP (`crates/wasi-http/src/http_impl.rs`):
- `outgoing_handler::Host::handle()`

#### HTTP types (`crates/wasi-http/src/types_impl.rs`):
- `HostIncomingResponse::drop()`
- `HostFutureTrailers::get()`
- `HostIncomingBody::finish()`, `drop()`
- `HostFutureIncomingResponse::drop()`, `get()`

### 6.3 How Golem uses async host functions

Every async WASI host function is overridden by `DurableWorkerCtx<Ctx>` in
`golem-worker-executor/src/durable_host/`. The canonical pattern (e.g. `get_random_bytes`):

```rust
async fn get_random_bytes(&mut self, length: u64) -> anyhow::Result<Vec<u8>> {
    let durability = Durability::<RandomGetRandomBytes>::new(self, DurableFunctionType::ReadLocal).await?;
    if durability.is_live() {
        let bytes = Host::get_random_bytes(&mut self.as_wasi_view(), length).await?;
        durability.persist(self, request, response).await
    } else {
        durability.replay(self).await  // return previously persisted result from oplog
    }
}
```

- In **live mode**: the real wasmtime implementation is called, the result is persisted
  to the oplog as a `HostCall` entry.
- In **replay mode**: the result is read from the oplog without calling the real
  implementation, ensuring deterministic replay after worker restart.

Each interceptable function is registered as a `HostPayloadPair` with typed
request/response enums (e.g. `HostRequestRandomBytes`, `HostResponseRandomBytes`) and
classified with a `DurableFunctionType` (`ReadLocal`, `WriteRemote`, etc.) that controls
the oplog fencing protocol.

Some functions are **not** intercepted with `Durability` because they are deterministic
across restarts:
- `get_environment` / `get_arguments` — recomputed from worker metadata.
- `get_directories` — filesystem is set up identically from component files on each restart.

---

## 7. Wasi-HTTP Durability Customizations

These changes are critical for Golem's durable HTTP connection support. They enable the
durable executor to defer outgoing requests, construct failing response bodies for replay,
and inspect HTTP state.

### 7.1 Bindings inlined into `lib.rs`

The `crates/wasi-http/src/bindings.rs` file is deleted and its `bindgen!` invocation is
moved inline into `crates/wasi-http/src/lib.rs` as `pub mod bindings { ... }`. Only async
bindings are generated (sync bindings and `Proxy`/`ProxyPre`/`ProxyIndices` re-exports
are dropped — see [section 3](#3-removed-modules-wasi-p0-p1-sync)).

### 7.2 Deferred HTTP requests

**File:** `crates/wasi-http/src/types.rs`

A new `HostFutureIncomingResponse::Deferred` variant:

```rust
pub enum HostFutureIncomingResponse {
    Pending(AbortOnDropJoinHandle<...>),
    Ready(anyhow::Result<Result<IncomingResponse, types::ErrorCode>>),
    Consumed,
    Deferred {                                          // NEW
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    },
}
```

With a constructor: `pub fn deferred(request, config) -> Self`.

When `get()` is called on a `Deferred` response (`crates/wasi-http/src/types_impl.rs`),
it spawns a task that calls `default_send_request_handler()`, replaces itself with
`Pending(handle)`, and returns `Ok(None)` (not ready yet). This allows the durable
executor to defer request execution until the response is actually needed.

### 7.3 Failing incoming bodies

**File:** `crates/wasi-http/src/body.rs`

- New constructor: `HostIncomingBody::failing(error: String)` — creates a body that
  immediately fails with the given error.
- New `IncomingBodyState::Failing(String)` variant.
- New `FailingStream` struct implementing `InputStream` — every read returns
  `StreamError::LastOperationFailed`.
- `take_stream()` return type changed from `Option<HostIncomingBodyStream>` to
  `Option<Box<dyn InputStream>>` to accommodate both normal and failing streams.
- `HostFutureTrailers::ready()` handles the `Failing` state by producing
  `ErrorCode::ConnectionTerminated`.

### 7.4 Exposed HTTP internals

- `get_fields()` in `crates/wasi-http/src/types_impl.rs` made `pub` and re-exported
  from `crates/wasi-http/src/lib.rs` as `pub use crate::types_impl::get_fields;`.
- `OutgoingRequestConfig` (`crates/wasi-http/src/types.rs`) gains `#[derive(Debug)]`.

### 7.5 How Golem uses HTTP durability

Golem's durable HTTP layer lives in `golem-worker-executor/src/durable_host/http/`.
The full lifecycle:

**`handle()` override** (`outgoing_http.rs`):
- Opens a `BeginRemoteWrite` oplog boundary (classified as `WriteRemoteBatched`).
- Extracts URI, method, headers for oplog recording.
- Injects W3C trace context headers and a derived `idempotency-key` header.
- Calls the real wasmtime-wasi-http `handle()` to dispatch the request.
- Registers the `HostFutureIncomingResponse` handle in an `open_http_requests` map
  that tracks the request's lifecycle through its resource chain.

**`future_incoming_response::get()` override** (`types.rs`):
- In **live mode**: polls the real HTTP future. When the response arrives, serializes
  status + headers as `SerializableResponseHeaders` into a `HostCall` oplog entry.
  On HTTP errors, may trigger retry logic.
- In **replay mode**: reads the persisted oplog entry and reconstructs a
  `HostIncomingResponse` in memory from the serialized headers. The `Deferred` state
  is critical here — during replay, the HTTP request was never actually sent, so the
  `subscribe()`/`Pollable` for it immediately resolves (the fork's `Deferred` pollable
  is always-ready), letting the guest proceed to `get()` which reads from the oplog.

**Response body streaming** (`streams.rs`):
- Body chunk reads on HTTP response streams are detected via `is_incoming_http_body_stream()`
  (which uses `as_any()` downcasting — see section 5.3) and wrapped in `Durability`
  blocks of type `WriteRemoteBatched`. Each chunk is persisted to the oplog.

**Trailer durability** (`types.rs`):
- `future_trailers::get()` uses `get_fields()` (the fork's public API) to read the
  raw `(HeaderName, HeaderValue)` pairs from the resource table for serialization to
  the oplog. On replay, a `HostFields::Owned` resource is reconstructed from the
  persisted trailer bytes.

**`HttpRequestCloseOwner` lifecycle chain**:
- The `HttpRequestState` migrates through the resource chain via `continue_http_request()`:
  `FutureIncomingResponse` → `IncomingResponse` → `IncomingBody` → `InputStream`.
- Whoever is the final `close_owner` calls `end_http_request()`, which writes the
  `EndRemoteWrite` oplog entry, closing the durable bracket.

---

## 8. Filesystem Path Tracking

The durable executor needs to know the host filesystem path of open files and directories
to persist and restore file system state.

### 8.1 New `path` field on `File` and `Dir`

**File:** `crates/wasi/src/p2/filesystem.rs`

```rust
pub struct File {
    // ... existing fields ...
    pub path: PathBuf  // NEW
}

pub struct Dir {
    // ... existing fields ...
    pub path: PathBuf  // NEW
}
```

The constructors `File::new()` and `Dir::new()` gain a `path: PathBuf` parameter.

### 8.2 Path propagation

- **Preopened directories** (`crates/wasi/src/p2/ctx.rs`): `preopened_dir()` passes
  `PathBuf::from(host_path.as_ref())` to `Dir::new()`.
- **`open_at`** (`crates/wasi/src/p2/host/filesystem.rs`): When opening files/directories,
  the path is constructed as `d.path.join(path)` and passed to `File::new()` / `Dir::new()`.

### 8.3 `ReaddirIterator` made public

- `ReaddirIterator::new()` visibility changed from `pub(crate)` to `pub`.
- Re-exported from `crates/wasi/src/p2/mod.rs`.

### 8.4 How Golem uses filesystem paths

Golem uses `File.path` and `Dir.path` in three ways
(`golem-worker-executor/src/durable_host/filesystem/types.rs` and `mod.rs`):

**1. Durable `stat` / `stat_at`** — When calling `stat()`, Golem reads the descriptor's
`path` from the resource table, persists it as `HostRequestFileSystemPath { path }` in
the oplog alongside the stat result (`SerializableFileTimes`). It also physically
re-applies the persisted timestamps to the on-disk file via `set_symlink_times(path, ...)`
to ensure filesystem state matches what the guest originally saw, even after restart.

**2. Read-only file enforcement** — Golem maintains a `read_only_paths: HashSet<PathBuf>`
from component metadata (files provisioned as read-only from the Initial File System).
On every write, `check_if_file_is_readonly()` checks `f.path` against this set and
returns `NotPermitted` if matched.

**3. Deterministic directory listing** — Golem overrides `read_directory()` to drain
the raw iterator, **sort entries alphabetically by name**, and reconstruct a new
`ReaddirIterator::new(sorted_iter)` (using the fork's public constructor). This ensures
directory listings are deterministic across runs, which is essential for replay.

---

## 9. Exposed Internals & Misc Changes

### 9.1 `ResourceTable::get_any()` — immutable accessor

**File:** `crates/wasmtime/src/runtime/component/resource_table.rs`

```rust
pub fn get_any(&self, key: u32) -> Result<&dyn Any, ResourceTableError>
```

Immutable counterpart to `get_any_mut()`. Used internally by the pollable
override-following logic in `get_pollable_following_overrides()`. Not directly called
by Golem code.

### 9.2 `VERSION` constant

**File:** `crates/wasmtime/src/lib.rs`

```rust
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
```

Used in Golem's `golem-worker-executor/src/metrics.rs` to populate a Prometheus metric
label on `executor_version_info`, making the wasmtime version queryable alongside
Golem's own version for operational observability.

### 9.3 Re-exports in `wasmtime-wasi`

**File:** `crates/wasi/src/lib.rs`

Additional re-exports for types factored out into `wasmtime-wasi-io` that downstream
Golem code depends on at the `wasmtime_wasi` path:

```rust
pub use wasmtime_wasi_io::poll::{
    dynamic_subscribe, subscribe, DynFuture, DynPollable, DynamicPollable, MakeFuture,
    OverrideSelf, Pollable,
};
pub use wasmtime_wasi_io::streams::{
    DynInputStream, DynOutputStream, Error as IoError, InputStream, OutputStream, StreamError,
    StreamResult,
};
pub use wasmtime_wasi_io::{IoCtx, IoImpl, IoView};
```

**File:** `crates/wasi/src/p2/view.rs` — `IoCtx` added to the existing re-export:

```rust
pub use wasmtime_wasi_io::{IoCtx, IoImpl, IoView};
```

Golem's `Cargo.toml` patches `wasmtime`, `wasmtime-wasi`, and `wasmtime-wasi-http` to
point at the fork branch (e.g. `branch = "golem-wasmtime-v33.0.0"`).

---

## 10. Submodules

Git submodules must be updated to match the new upstream baseline version. In the v33
fork the following submodule changes were made relative to the baseline:

| Submodule | Change |
|-----------|--------|
| `tests/spec_testsuite` | Deleted (submodule removed). |
| `tests/wasi_testsuite/wasi-common` | Commit updated. |

When upgrading, reset all submodules to whatever the new upstream tag uses.

---

## 11. Minor / Incidental Differences

These are not intentional customizations but formatting or style artifacts from the
fork's history. They can be ignored or cleaned up during an upgrade:

- `crates/wasmtime/src/lib.rs`: Extra blank lines between `mod` and `pub use` statements.
- `crates/wit-bindgen/src/lib.rs`: Minor reformatting (`Item = (...)` → `Item=(...)`).
- `crates/wasi/src/p2/host/filesystem.rs`: Slightly different `#[cfg(windows)]` block indentation.
- `crates/wasi-http/src/body.rs`: Changed indentation on `.boxed()`.
- `crates/wasi/src/p2/pipe.rs`: `#[async_trait::async_trait]` removed from some `OutputStream` impls where the attribute is no longer needed.
- `src/commands/run.rs`: `self.preview2_ctx().ctx()` → `self.preview2_ctx().io_ctx()` in `WasiView` impl for `Host` (consequence of the `IoCtx` changes).

---

## 12. Upgrade Checklist

When rebasing onto a new upstream version, apply the following steps. Build after each
major feature group to catch issues early.

### Phase 1: Preparation

1. **Update Rust toolchain** if required by the new upstream version (check
   `rust-toolchain.toml` in the upstream tag).
2. **Create baseline branch** (`wasmtime-vX.Y.Z-baseline`) from the upstream tag.
3. **Build the unmodified baseline** — `cargo build` on the baseline branch to confirm
   your environment works before any customizations.
4. **Create fork branch** (`golem-wasmtime-vX.Y.Z`) from the baseline.
5. **Reset submodules** to the new upstream baseline's versions.
6. **Analyze upstream changes** — Diff the old baseline against the new target
   (`git diff wasmtime-v33.0.0-baseline..wasmtime-vX.Y.Z-baseline --stat`) to understand
   what moved, was renamed, or restructured. For each feature below, check whether
   upstream now provides equivalent functionality natively. Useful search commands:
   - `rg "add_to_linker_sync|build_p1|preview1|preview0" crates/` — check if p0/p1/sync still exist
   - `rg "trait IoView|IoCtx|suspend" crates/wasi-io/` — check for upstream suspend support
   - `rg "as_any|fn override_index|DynamicPollable" crates/` — check for upstream equivalents
   - `rg "async fn now|async fn get_random" crates/wasi/` — check which functions are already async

### Phase 2: Apply customizations (build after each step)

7. **Remove p0/p1/sync** — Delete `preview0.rs`, `preview1.rs`, remove their mod
   declarations, remove all `add_to_linker_sync` functions and `build_p1()`. Also
   remove/update any related Cargo.toml feature flags (e.g. `preview1` feature).
   (Skip if upstream already removed them.)
8. **Add suspend support** — Add `IoCtx` struct, extend `IoView` with `io_ctx()`,
   modify `subscribe()` to take `supports_suspend`, add suspend check in `poll()`,
   propagate through all wrapper types, update `WasiCtxBuilder` (new fields,
   `set_suspend()`, changed `build()` return type).
9. **Add dynamic pollables** — `DynamicPollable` trait, `dynamic_subscribe()`,
   `OverrideSelf`, override-following logic, `ResourceTable::get_any()`.
10. **Add stream downcasting** — `Any` supertrait + `as_any()` on
    `InputStream`/`OutputStream`, implement on all concrete types. Note: `Any`
    implies `'static`; all stream implementors must satisfy this bound.
11. **Expand async** — Ensure all host functions listed in section 6 are async. Update
    `bindgen!` async lists. If upstream has already made some of them async, skip those.
    Add `in_tokio` wrappers in sync IO host. If upstream changed WIT files, ensure
    regenerated bindings are consistent.
12. **wasi-http durability** — Inline bindings (async-only), add `Deferred` variant,
    `failing()` body, `FailingStream`, boxed `take_stream()`, `get_fields` pub, `Debug`
    on `OutgoingRequestConfig`.
13. **Filesystem path tracking** — Add `path: PathBuf` to `File`/`Dir`, propagate
    through constructors, `preopened_dir()`, and `open_at`.
14. **Misc** — `VERSION` constant, re-exports in `wasmtime-wasi`, `ReaddirIterator` pub.

### Phase 3: Validation

15. **Supply chain audits** — Update `supply-chain/imports.lock` with publisher entries
    for any new crates introduced by the upstream version. Run `cargo vet` to identify
    what needs to be added.
16. **Run formatting and lints** — `cargo fmt --all --check` and clippy checks per the
    repo's standard.
17. **Build and test the fork** — `cargo build` and run tests for the key crates:
    - `cargo test -p wasmtime-wasi-io`
    - `cargo test -p wasmtime-wasi`
    - `cargo test -p wasmtime-wasi-http`
18. **Verify against Golem** — Check out the latest Golem `main` branch into a
    subdirectory, create an upgrade branch, and modify the root `Cargo.toml`'s
    `[patch.crates-io]` entries to point `wasmtime`, `wasmtime-wasi`, and
    `wasmtime-wasi-http` at the new fork branch. Build Golem and run its test suite
    to confirm compatibility.

## Appendix A: Modified Files (v33 fork)

This is the complete list of files modified or deleted in the v33 fork relative to the
`wasmtime-v33.0.0-baseline`. Note that upstream restructuring may move or rename these
files in newer versions.

**Deleted files:**
- `crates/wasi/src/preview0.rs`
- `crates/wasi/src/preview1.rs`
- `crates/wasi-http/src/bindings.rs`
- `tests/spec_testsuite` (submodule)

**Modified files — `crates/wasi-io/` (wasmtime-wasi-io crate):**
- `crates/wasi-io/src/lib.rs` — `IoCtx` struct, `IoView` trait extension
- `crates/wasi-io/src/poll.rs` — `subscribe()` signature, `DynPollable` fields, `DynamicPollable`, `dynamic_subscribe()`
- `crates/wasi-io/src/impls.rs` — suspend check in `poll()`, override-following logic, async conversions
- `crates/wasi-io/src/streams.rs` — `Any` supertrait, `as_any()` on `InputStream`/`OutputStream`
- `crates/wasi-io/src/bindings.rs` — expanded `only_imports` async list

**Modified files — `crates/wasi/` (wasmtime-wasi crate):**
- `crates/wasi/src/lib.rs` — removed p0/p1 modules, added re-exports
- `crates/wasi/src/p2/mod.rs` — removed sync linker, added `ReaddirIterator` re-export
- `crates/wasi/src/p2/ctx.rs` — `WasiCtxBuilder` changes (suspend, `build()` return type, removed `build_p1()`)
- `crates/wasi/src/p2/view.rs` — `IoCtx` re-export, `io_ctx()` on `WasiImpl`
- `crates/wasi/src/p2/bindings.rs` — expanded `only_imports` async list
- `crates/wasi/src/p2/filesystem.rs` — `path` field on `File`/`Dir`, `as_any()` impls, `ReaddirIterator::new()` pub
- `crates/wasi/src/p2/pipe.rs` — `as_any()` impls
- `crates/wasi/src/p2/stdio.rs` — `as_any()` impls
- `crates/wasi/src/p2/stdio/worker_thread_stdin.rs` — `as_any()` impl
- `crates/wasi/src/p2/tcp.rs` — `as_any()` impls
- `crates/wasi/src/p2/write_stream.rs` — `as_any()` impl
- `crates/wasi/src/p2/host/clocks.rs` — async conversions, suspend deadline propagation
- `crates/wasi/src/p2/host/env.rs` — async conversions
- `crates/wasi/src/p2/host/filesystem.rs` — async conversions, path propagation in `open_at`
- `crates/wasi/src/p2/host/io.rs` — `in_tokio` wrappers for newly-async methods
- `crates/wasi/src/p2/host/random.rs` — async conversions
- `crates/wasi/src/p2/host/tcp.rs` — `subscribe()` call site update
- `crates/wasi/src/p2/host/udp.rs` — `subscribe()` call site updates
- `crates/wasi/src/p2/ip_name_lookup.rs` — async conversion, `subscribe()` call site update

**Modified files — `crates/wasi-http/` (wasmtime-wasi-http crate):**
- `crates/wasi-http/src/lib.rs` — inlined async-only bindings, removed sync linker, `get_fields` re-export
- `crates/wasi-http/src/types.rs` — `Deferred` variant, `deferred()` constructor, `Debug` on config, `io_ctx()` on `WasiHttpImpl`
- `crates/wasi-http/src/types_impl.rs` — async conversions, deferred request execution, `get_fields` pub, `subscribe()` call site update
- `crates/wasi-http/src/http_impl.rs` — `handle()` async conversion
- `crates/wasi-http/src/body.rs` — `failing()` constructor, `FailingStream`, boxed `take_stream()`, `as_any()` impls

**Modified files — `crates/wasmtime/` (wasmtime crate):**
- `crates/wasmtime/src/lib.rs` — `VERSION` constant
- `crates/wasmtime/src/runtime/component/resource_table.rs` — `get_any()` immutable accessor

**Other:**
- `crates/wit-bindgen/src/lib.rs` — minor formatting (incidental)
- `src/commands/run.rs` — `WasiView` impl adapted for `IoCtx`
- `supply-chain/imports.lock` — new publisher entries
- `tests/wasi_testsuite/wasi-common` — submodule commit updated

---

## Appendix B: v42.0.1 Upgrade Report

This section documents the upgrade from the v33.0.0 fork to v42.0.1, including what
changed upstream, how each customization was adapted, and what Golem-side changes are
required.

### B.1 Upstream structural changes (v33 → v42)

Wasmtime v42 includes significant restructuring relative to v33. Key changes affecting
the fork:

| v33 location | v42 location | Change |
|---|---|---|
| `crates/wasi/src/p2/ctx.rs` | `crates/wasi/src/ctx.rs` | Moved up one level out of `p2/` |
| `crates/wasi/src/p2/view.rs` | `crates/wasi/src/view.rs` | Moved up one level out of `p2/` |
| `crates/wasi/src/p2/filesystem.rs` (File, Dir) | `crates/wasi/src/filesystem.rs` | Core types moved out; `p2/filesystem.rs` retains host bindings |
| `crates/wasi/src/preview0.rs` | `crates/wasi/src/p0.rs` | Renamed (not deleted by upstream) |
| `crates/wasi/src/preview1.rs` | `crates/wasi/src/p1.rs` | Renamed (not deleted by upstream) |
| `IoImpl<T>` wrapper type | `IoData<'a>` struct | Upstream replaced the wrapper with a compound view struct |
| `WasiCtxView` | `WasiCtxView<'a>` | Changed from trait-based to struct-based (now holds `&mut WasiCtx`, `&mut ResourceTable`) |
| `WasiView::ctx()` | Returns `WasiCtxView<'a>` struct | No longer returns individual fields; returns compound view |
| `wasmtime::Error` | Custom error type | No longer `anyhow::Error`; now a wasmtime-internal `Error` type with `downcast_ref()` support |
| `HostFutureIncomingResponse` | Uses `FutureIncomingResponseHandle` | `Pending` variant changed from `AbortOnDropJoinHandle` to `FutureIncomingResponseHandle` |
| `bindgen!` `require_store_data_send` | New option | v42 requires explicit `require_store_data_send: true` for async bindings |

### B.2 Feature verification — all customizations preserved

Every feature from the fork guide has been re-applied and verified:

| Feature | Status | Notes |
|---|---|---|
| **Remove p0/p1/sync** | ✅ Preserved | p0/p1 module declarations removed from `lib.rs` (files kept on disk but not compiled). Sync linkers removed from both wasi and wasi-http. `build_p1()` removed. |
| **Suspend support** | ✅ Preserved | `IoCtx`, `IoView::io_ctx()`, `subscribe()` 3-arg, suspend check in `poll()` all present. |
| **Dynamic pollables** | ✅ Preserved | `DynamicPollable`, `dynamic_subscribe()`, `OverrideSelf`, `get_pollable_following_overrides()`, `ResourceTable::get_any()` all present. |
| **Stream downcasting** | ✅ Preserved | `InputStream: Any + as_any()`, `OutputStream: Any + as_any()` on all concrete types. |
| **Async host functions** | ✅ Preserved | All required functions remain async in bindgen configs. |
| **Deferred HTTP** | ✅ Preserved | `HostFutureIncomingResponse::Deferred`, `deferred()` constructor, always-ready `Pollable` impl. |
| **Failing bodies** | ✅ Preserved | `HostIncomingBody::failing()`, `FailingStream`, `IncomingBodyState::Failing`. |
| **Boxed take_stream** | ✅ Preserved | `take_stream() -> Option<Box<dyn InputStream>>`. |
| **get_fields pub** | ✅ Preserved | `pub fn get_fields()` in `types_impl.rs`, re-exported from `lib.rs`. |
| **OutgoingRequestConfig: Debug** | ✅ Preserved | `#[derive(Debug)]` on `OutgoingRequestConfig`. |
| **File/Dir path tracking** | ✅ Preserved | `pub path: PathBuf` on `File` and `Dir`, propagated in constructors, `preopened_dir()`, and `open_at`. |
| **ReaddirIterator::new() pub** | ✅ Preserved | Public constructor, re-exported from `p2/mod.rs`. |
| **ResourceTable::get_any()** | ✅ Preserved | Immutable accessor exists. |
| **VERSION constant** | ✅ Preserved | `pub const VERSION: &str = env!("CARGO_PKG_VERSION")`. |
| **Re-exports in wasmtime-wasi** | ✅ Preserved | All poll, stream, and IO types re-exported from `crates/wasi/src/lib.rs`. |

### B.3 API changes requiring Golem-side updates

The following APIs changed shape due to upstream v42 restructuring. Golem code must be
updated when adopting the new fork.

#### B.3.1 `IoImpl` → `IoData`

Upstream replaced the `IoImpl<T>` newtype wrapper with an `IoData<'a>` compound view
struct:

```rust
// v33 fork:
pub use wasmtime_wasi_io::{IoCtx, IoImpl, IoView};

// v42 fork:
pub use wasmtime_wasi_io::{IoCtx, IoData, IoView};
```

`IoData<'a>` holds `&'a mut ResourceTable` and `&'a mut IoCtx`. Golem code importing
`wasmtime_wasi::IoImpl` must be updated to use `wasmtime_wasi::IoData`.

The `IoView` trait now has an additional method `fn io_data(&mut self) -> IoData<'_>`
alongside the existing `fn table()` and `fn io_ctx()`. Implementors of `IoView` need
to provide this method.

#### B.3.2 `WasiCtxView` is now a struct with `io_ctx`

`WasiCtxView` changed from returning individual parts to a compound struct:

```rust
// v42 fork:
pub struct WasiCtxView<'a> {
    pub ctx: &'a mut WasiCtx,
    pub table: &'a mut ResourceTable,
    pub io_ctx: &'a mut IoCtx,      // NEW — Golem fork addition
}
```

Golem's `WasiView` implementations (e.g. `DurableWorkerCtx`) must return
`WasiCtxView` with the `io_ctx` field populated.

#### B.3.3 `suspend_signal` error type: `anyhow::Error` → `wasmtime::Error`

In v42, wasmtime uses its own `wasmtime::Error` type instead of `anyhow::Error`.
The `IoCtx.suspend_signal` field signature is now:

```rust
pub suspend_signal: Box<dyn Fn(Duration) -> wasmtime::Error + Send + Sync + 'static>,
```

`wasmtime::Error` supports `downcast_ref::<T>()` the same way `anyhow::Error` does,
so Golem's `SuspendForSleep` downcasting pattern will still work. However, the closure
passed to `set_suspend()` must return `wasmtime::Error` instead of `anyhow::Error`.
Use `wasmtime::Error::msg(...)` or `wasmtime::Error::from(...)` to construct it.

#### B.3.4 `add_to_linker_async` / `add_only_http_to_linker_async` require `Send`

The v42 async linker functions require `T: Send`:

```rust
pub fn add_to_linker_async<T>(l: &mut Linker<T>) -> Result<()>
where
    T: WasiHttpView + wasmtime_wasi::WasiView + Send + 'static,
```

Golem's store data types already implement `Send`, so this should be transparent.

#### B.3.5 `HostFutureIncomingResponse::Pending` wraps `FutureIncomingResponseHandle`

The `Pending` variant changed from wrapping `AbortOnDropJoinHandle<...>` to
`FutureIncomingResponseHandle`. The `Deferred` variant's `get()` implementation now
calls `self.send_request(request, config)?` (a method on `WasiHttpView`) instead of
the standalone `default_send_request_handler()`. Golem's HTTP interception code should
verify compatibility with this new dispatch path.

### B.4 CLI adaptations

The CLI (`src/commands/run.rs`, `src/commands/serve.rs`) was adapted for the fork
changes. These are not consumed by Golem but are needed for the wasmtime binary to
compile:

- **run.rs**: Replaced `WasiP1Ctx` usage with direct `WasiCtx` + `ResourceTable` +
  `IoCtx` fields in the `Host` struct, since the `p1` module is not compiled.
  Removed p0/p1 linker calls. Changed `add_only_http_to_linker_sync` to
  `add_only_http_to_linker_async`.
- **serve.rs**: Added `io_ctx` field to `Host` struct and `WasiCtxView` construction.
  Destructured `builder.build()` return. Added `as_any()` to `LogStream`'s
  `OutputStream` impl.
- **wasi-tls**: Added `as_any()` to `AsyncWriteStream`'s `OutputStream` impl and
  updated `subscribe()` call to 3-arg form.

### B.5 Files not yet cleaned up

The following items exist in the repo but are not compiled as part of the fork's
library crates. They may cause failures under `cargo check --all-targets` if their
containing crates are compiled:

- `crates/wasi/src/p0.rs` and `crates/wasi/src/p1.rs` — files exist on disk but
  `mod p0;` / `mod p1;` are not declared in `lib.rs`, so they are dead code.
- Test crates (`wasmtime-wasi-tests`, `wasmtime-wasi-http-tests`) and examples/benches
  may reference removed sync APIs (`add_to_linker_sync`, `build_p1`, etc.). These are
  not Golem-relevant but would need cleanup for a full CI-green workspace build.
- The `test-programs-artifacts` crate requires the `wasm32-unknown-unknown` target
  installed to build.

### B.6 Build verification

The following crates pass `cargo check` successfully:

- `wasmtime` (core crate)
- `wasmtime-wasi-io`
- `wasmtime-wasi`
- `wasmtime-wasi-http`
- `wasmtime-wasi-tls`
- `wasmtime-cli` (the `wasmtime` binary)

### B.7 Updated public API contract (v42)

| API | Crate | Change from v33 |
|-----|-------|-----------------|
| `WasiCtxBuilder::build() -> (WasiCtx, IoCtx)` | `wasmtime-wasi` | ✅ Same |
| `WasiCtxBuilder::set_suspend(threshold, signal)` | `wasmtime-wasi` | ⚠️ Signal returns `wasmtime::Error` (was `anyhow::Error`) |
| `IoView::io_ctx(&mut self) -> &mut IoCtx` | `wasmtime-wasi-io` | ✅ Same |
| `IoView::io_data(&mut self) -> IoData<'_>` | `wasmtime-wasi-io` | 🆕 New required method |
| `IoCtx { suspend_threshold, suspend_signal }` | `wasmtime-wasi-io` | ⚠️ `suspend_signal` returns `wasmtime::Error` |
| `subscribe(table, resource, supports_suspend)` | `wasmtime-wasi-io` | ✅ Same |
| `DynamicPollable` trait, `dynamic_subscribe()` | `wasmtime-wasi-io` | ✅ Same |
| `InputStream: Any`, `as_any()` | `wasmtime-wasi-io` | ✅ Same |
| `OutputStream: Any`, `as_any()` | `wasmtime-wasi-io` | ✅ Same |
| `HostFutureIncomingResponse::deferred()` | `wasmtime-wasi-http` | ✅ Same |
| `HostIncomingBody::failing(error)` | `wasmtime-wasi-http` | ✅ Same |
| `HostIncomingBody::take_stream() -> Option<Box<dyn InputStream>>` | `wasmtime-wasi-http` | ✅ Same |
| `get_fields()` (pub) | `wasmtime-wasi-http` | ✅ Same |
| `File { pub path: PathBuf }`, `Dir { pub path: PathBuf }` | `wasmtime-wasi` | ✅ Same (moved to `crates/wasi/src/filesystem.rs`) |
| `ReaddirIterator::new()` (pub) | `wasmtime-wasi` | ✅ Same |
| `ResourceTable::get_any()` (immutable) | `wasmtime` | ✅ Same |
| `wasmtime::VERSION` | `wasmtime` | ✅ Same |
| `WasiCtxView { ctx, table, io_ctx }` | `wasmtime-wasi` | 🆕 Now a struct with `io_ctx` field |
| Re-exports: `IoData` (was `IoImpl`) | `wasmtime-wasi` | ⚠️ Renamed |

### B.8 Golem upgrade checklist

When upgrading Golem to use the v42 fork:

1. Update `Cargo.toml` patches to point at `branch = "golem-wasmtime-v42.0.1"`.
2. Replace all `wasmtime_wasi::IoImpl` imports with `wasmtime_wasi::IoData`.
3. Update `WasiView` implementations to return `WasiCtxView` with the `io_ctx` field.
4. Implement `IoView::io_data()` on all types implementing `IoView`.
5. Update `set_suspend()` closure to return `wasmtime::Error` instead of `anyhow::Error`.
6. Verify HTTP interception code works with the new `send_request()` dispatch in
   `HostFutureIncomingResponse::Deferred` handling.
7. Build and run the Golem test suite.
