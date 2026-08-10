use crate::prelude::*;

/// Whether a growing heap backs a WebAssembly linear memory or one of
/// Wasmtime's internal GC heaps.
///
/// This is passed to [`ResourceLimiter::memory_growing`] and
/// [`ResourceLimiter::memory_grown`] so that embedders which account for guest
/// memory separately from runtime overhead can tell the two apart. GC heap
/// capacity is an implementation detail of Wasmtime's garbage collector rather
/// than memory the guest module declared.
///
/// # When `GcHeap` can be observed
///
/// A store only ever allocates a GC heap if the `gc` crate feature is compiled
/// in *and* [`Config::wasm_gc`](crate::Config::wasm_gc) (or another
/// GC-dependent proposal) is enabled *and* something in the store actually
/// allocates a GC object. An embedder that leaves GC disabled will never see
/// `GcHeap` and can treat every callback as `LinearMemory`. Embedders that do
/// enable GC must not bill GC heap capacity as guest linear memory: the two are
/// separate pools, and the GC heap grows on the collector's schedule rather
/// than in response to a guest `memory.grow`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MemoryKind {
    /// A WebAssembly linear memory declared or imported by a guest module.
    LinearMemory,
    /// A heap that Wasmtime's garbage collector allocates GC objects out of.
    GcHeap,
}

impl From<wasmtime_environ::MemoryKind> for MemoryKind {
    fn from(kind: wasmtime_environ::MemoryKind) -> MemoryKind {
        match kind {
            wasmtime_environ::MemoryKind::LinearMemory => MemoryKind::LinearMemory,
            wasmtime_environ::MemoryKind::GcHeap => MemoryKind::GcHeap,
        }
    }
}

/// Value returned by [`ResourceLimiter::instances`] default method
pub const DEFAULT_INSTANCE_LIMIT: usize = 10000;
/// Value returned by [`ResourceLimiter::tables`] default method
pub const DEFAULT_TABLE_LIMIT: usize = 10000;
/// Value returned by [`ResourceLimiter::memories`] default method
pub const DEFAULT_MEMORY_LIMIT: usize = 10000;

/// Used by hosts to limit resource consumption of instances.
///
/// This trait is used in conjunction with the
/// [`Store::limiter`](crate::Store::limiter) to synchronously limit the
/// allocation of resources within a store. As a store-level limit this means
/// that all creation of instances, memories, and tables are limited within the
/// store. Resources limited via this trait are primarily related to memory and
/// limiting CPU resources needs to be done with something such as
/// [`Config::consume_fuel`](crate::Config::consume_fuel) or
/// [`Config::epoch_interruption`](crate::Config::epoch_interruption).
///
/// Note that this trait does not limit 100% of memory allocated via a
/// [`Store`](crate::Store). Wasmtime will still allocate memory to track data
/// structures and additionally embedder-specific memory allocations are not
/// tracked via this trait. This trait only limits resources allocated by a
/// WebAssembly instance itself.
///
/// This trait is intended for synchronously limiting the resources of a module.
/// If your use case requires blocking to answer whether a request is permitted
/// or not and you're otherwise working in an asynchronous context the
/// [`ResourceLimiterAsync`] trait is also provided to avoid blocking an OS
/// thread while a limit is determined.
pub trait ResourceLimiter: Send {
    /// Notifies the resource limiter that an instance's linear memory has been
    /// requested to grow.
    ///
    /// * `current` is the current size of the linear memory in bytes.
    /// * `desired` is the desired size of the linear memory in bytes.
    /// * `maximum` is either the linear memory's maximum or a maximum from an
    ///   instance allocator, also in bytes. A value of `None`
    ///   indicates that the linear memory is unbounded.
    /// * `kind` distinguishes a guest linear memory from one of Wasmtime's
    ///   internal GC heaps. Embedders that attribute memory to the guest should
    ///   check this before counting the request as guest memory.
    ///
    /// The `current` and `desired` amounts are guaranteed to always be
    /// multiples of the WebAssembly page size, 64KiB.
    ///
    /// This function is not invoked when the requested size doesn't fit in
    /// `usize`. Additionally this function is not invoked for shared memories
    /// at this time. Otherwise even when `desired` exceeds `maximum` this
    /// function will still be called.
    ///
    /// ## Return Value
    ///
    /// If `Ok(true)` is returned from this function then the growth operation
    /// is allowed. This means that the wasm `memory.grow` instruction will
    /// return with the `desired` size, in wasm pages. Note that even if
    /// `Ok(true)` is returned, though, if `desired` exceeds `maximum` then the
    /// growth operation will still fail.
    ///
    /// If `Ok(false)` is returned then this will cause the `memory.grow`
    /// instruction in a module to return -1 (failure), or in the case of an
    /// embedder API calling [`Memory::new`](crate::Memory::new) or
    /// [`Memory::grow`](crate::Memory::grow) an error will be returned from
    /// those methods.
    ///
    /// If `Err(e)` is returned then the `memory.grow` function will behave
    /// as if a trap has been raised. Note that this is not necessarily
    /// compliant with the WebAssembly specification but it can be a handy and
    /// useful tool to get a precise backtrace at "what requested so much memory
    /// to cause a growth failure?".
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
        kind: MemoryKind,
    ) -> Result<bool>;

    /// Notifies the resource limiter that growing a heap has failed.
    ///
    /// Note that this method is not called if `memory_growing` returns an
    /// error. It *can* be called without a preceding `memory_growing`, when the
    /// request is rejected before the limiter is ever consulted — for instance
    /// a growth that the memory's own type cannot represent.
    ///
    /// Reasons for failure include: the growth exceeds the `maximum` passed to
    /// `memory_growing`, or the operating system failed to allocate additional
    /// memory. In that case, `error` might be downcastable to a `std::io::Error`.
    ///
    /// `kind` is the same kind that was passed to the corresponding
    /// `memory_growing` call, so an embedder that reserved something there
    /// knows which reservation to release.
    ///
    /// See the details on the return values for `memory_growing` for what the
    /// return value of this function indicates.
    fn memory_grow_failed(&mut self, error: crate::Error, _kind: MemoryKind) -> Result<()> {
        log::debug!("ignoring memory growth failure error: {error:?}");
        Ok(())
    }

    /// Notifies the resource limiter that a growth permitted by
    /// `memory_growing` has successfully committed.
    ///
    /// Every `memory_growing` call that returns `Ok(true)` for a growth (as
    /// opposed to an initial allocation) is followed by exactly one of this
    /// method or `memory_grow_failed`, with the same `kind` that was passed to
    /// `memory_growing`, unless one of Wasmtime's own internal invariants is
    /// violated and it panics in between. This is not called for a memory's
    /// initial allocation or for shared memories.
    ///
    /// `current` and `desired` are the heap's old and new sizes in bytes, and
    /// `kind` distinguishes a guest linear memory from an internal GC heap.
    ///
    /// This runs after the committed base pointer and length have been
    /// published to the owning `VMContext` — and, for a GC heap, after the grown
    /// memory and its new capacity have been handed back to the collector.
    /// Unwinding out of this method therefore leaves the store and its instances
    /// in a consistent, reusable state.
    fn memory_grown(&mut self, _current: usize, _desired: usize, _kind: MemoryKind) {}

    /// Notifies the resource limiter that an instance's table has been
    /// requested to grow.
    ///
    /// * `current` is the current number of elements in the table.
    /// * `desired` is the desired number of elements in the table.
    /// * `maximum` is either the table's maximum or a maximum from an instance
    ///   allocator.  A value of `None` indicates that the table is unbounded.
    ///
    /// Currently in Wasmtime each table element requires a pointer's worth of
    /// space (e.g. `mem::size_of::<usize>()`).
    ///
    /// See the details on the return values for `memory_growing` for what the
    /// return value of this function indicates.
    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool>;

    /// Notifies the resource limiter that growing a linear memory, permitted by
    /// the `table_growing` method, has failed.
    ///
    /// Note that this method is not called if `table_growing` returns an error.
    ///
    /// Reasons for failure include: the growth exceeds the `maximum` passed to
    /// `table_growing`. This could expand in the future.
    ///
    /// See the details on the return values for `memory_growing` for what the
    /// return value of this function indicates.
    fn table_grow_failed(&mut self, error: crate::Error) -> Result<()> {
        log::debug!("ignoring table growth failure error: {error:?}");
        Ok(())
    }

    /// The maximum number of instances that can be created for a `Store`.
    ///
    /// Module instantiation will fail if this limit is exceeded.
    ///
    /// This value defaults to 10,000.
    fn instances(&self) -> usize {
        DEFAULT_INSTANCE_LIMIT
    }

    /// The maximum number of tables that can be created for a `Store`.
    ///
    /// Creation of tables will fail if this limit is exceeded.
    ///
    /// This value defaults to 10,000.
    fn tables(&self) -> usize {
        DEFAULT_TABLE_LIMIT
    }

    /// The maximum number of linear memories that can be created for a `Store`
    ///
    /// Creation of memories will fail with an error if this limit is exceeded.
    ///
    /// This value defaults to 10,000.
    fn memories(&self) -> usize {
        DEFAULT_MEMORY_LIMIT
    }
}

/// Used by hosts to limit resource consumption of instances, blocking
/// asynchronously if necessary.
///
/// This trait is identical to [`ResourceLimiter`], except that the
/// `memory_growing` and `table_growing` functions are `async`.
///
/// This trait is used with
/// [`Store::limiter_async`](`crate::Store::limiter_async`)`: see those docs
/// for restrictions on using other Wasmtime interfaces with an async resource
/// limiter. Additionally see [`ResourceLimiter`] for more information about
/// limiting resources from WebAssembly.
///
/// The `async` here enables embedders that are already using asynchronous
/// execution of WebAssembly to block the WebAssembly, but no the OS thread, to
/// answer the question whether growing a memory or table is allowed.
#[cfg(feature = "async")]
#[async_trait::async_trait]
pub trait ResourceLimiterAsync: Send {
    /// Async version of [`ResourceLimiter::memory_growing`]
    async fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
        kind: MemoryKind,
    ) -> Result<bool>;

    /// Identical to [`ResourceLimiter::memory_grow_failed`]
    fn memory_grow_failed(&mut self, error: crate::Error, _kind: MemoryKind) -> Result<()> {
        log::debug!("ignoring memory growth failure error: {error:?}");
        Ok(())
    }

    /// Identical to [`ResourceLimiter::memory_grown`].
    fn memory_grown(&mut self, _current: usize, _desired: usize, _kind: MemoryKind) {}

    /// Asynchronous version of [`ResourceLimiter::table_growing`]
    async fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool>;

    /// Identical to [`ResourceLimiter::table_grow_failed`]
    fn table_grow_failed(&mut self, error: crate::Error) -> Result<()> {
        log::debug!("ignoring table growth failure error: {error:?}");
        Ok(())
    }

    /// Identical to [`ResourceLimiter::instances`]`
    fn instances(&self) -> usize {
        DEFAULT_INSTANCE_LIMIT
    }

    /// Identical to [`ResourceLimiter::tables`]`
    fn tables(&self) -> usize {
        DEFAULT_TABLE_LIMIT
    }

    /// Identical to [`ResourceLimiter::memories`]`
    fn memories(&self) -> usize {
        DEFAULT_MEMORY_LIMIT
    }
}

/// Used to build [`StoreLimits`].
pub struct StoreLimitsBuilder(StoreLimits);

impl StoreLimitsBuilder {
    /// Creates a new [`StoreLimitsBuilder`].
    ///
    /// See the documentation on each builder method for the default for each
    /// value.
    pub fn new() -> Self {
        Self(StoreLimits::default())
    }

    /// The maximum number of bytes a linear memory can grow to.
    ///
    /// Growing a linear memory beyond this limit will fail. This limit is
    /// applied to each linear memory individually, so if a wasm module has
    /// multiple linear memories then they're all allowed to reach up to the
    /// `limit` specified.
    ///
    /// By default, linear memory will not be limited.
    pub fn memory_size(mut self, limit: usize) -> Self {
        self.0.memory_size = Some(limit);
        self
    }

    /// The maximum number of elements in a table.
    ///
    /// Growing a table beyond this limit will fail. This limit is applied to
    /// each table individually, so if a wasm module has multiple tables then
    /// they're all allowed to reach up to the `limit` specified.
    ///
    /// By default, table elements will not be limited.
    pub fn table_elements(mut self, limit: usize) -> Self {
        self.0.table_elements = Some(limit);
        self
    }

    /// The maximum number of instances that can be created for a [`Store`](crate::Store).
    ///
    /// Module instantiation will fail if this limit is exceeded.
    ///
    /// This value defaults to 10,000.
    pub fn instances(mut self, limit: usize) -> Self {
        self.0.instances = limit;
        self
    }

    /// The maximum number of tables that can be created for a [`Store`](crate::Store).
    ///
    /// Module instantiation will fail if this limit is exceeded.
    ///
    /// This value defaults to 10,000.
    pub fn tables(mut self, tables: usize) -> Self {
        self.0.tables = tables;
        self
    }

    /// The maximum number of linear memories that can be created for a [`Store`](crate::Store).
    ///
    /// Instantiation will fail with an error if this limit is exceeded.
    ///
    /// This value defaults to 10,000.
    pub fn memories(mut self, memories: usize) -> Self {
        self.0.memories = memories;
        self
    }

    /// Indicates that a trap should be raised whenever a growth operation
    /// would fail.
    ///
    /// This operation will force `memory.grow` and `table.grow` instructions
    /// to raise a trap on failure instead of returning -1. This is not
    /// necessarily spec-compliant, but it can be quite handy when debugging a
    /// module that fails to allocate memory and might behave oddly as a result.
    ///
    /// This value defaults to `false`.
    pub fn trap_on_grow_failure(mut self, trap: bool) -> Self {
        self.0.trap_on_grow_failure = trap;
        self
    }

    /// Consumes this builder and returns the [`StoreLimits`].
    pub fn build(self) -> StoreLimits {
        self.0
    }
}

/// Provides limits for a [`Store`](crate::Store).
///
/// This type is created with a [`StoreLimitsBuilder`] and is typically used in
/// conjunction with [`Store::limiter`](crate::Store::limiter).
///
/// This is a convenience type included to avoid needing to implement the
/// [`ResourceLimiter`] trait if your use case fits in the static configuration
/// that this [`StoreLimits`] provides.
#[derive(Clone, Debug)]
pub struct StoreLimits {
    memory_size: Option<usize>,
    table_elements: Option<usize>,
    instances: usize,
    tables: usize,
    memories: usize,
    trap_on_grow_failure: bool,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            memory_size: None,
            table_elements: None,
            instances: DEFAULT_INSTANCE_LIMIT,
            tables: DEFAULT_TABLE_LIMIT,
            memories: DEFAULT_MEMORY_LIMIT,
            trap_on_grow_failure: false,
        }
    }
}

impl ResourceLimiter for StoreLimits {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
        _kind: MemoryKind,
    ) -> Result<bool> {
        let allow = match self.memory_size {
            Some(limit) if desired > limit => false,
            _ => match maximum {
                Some(max) if desired > max => false,
                _ => true,
            },
        };
        if !allow && self.trap_on_grow_failure {
            bail!("forcing trap when growing memory to {desired} bytes")
        } else {
            Ok(allow)
        }
    }

    fn memory_grow_failed(&mut self, error: crate::Error, _kind: MemoryKind) -> Result<()> {
        if self.trap_on_grow_failure {
            Err(error.context("forcing a memory growth failure to be a trap"))
        } else {
            log::debug!("ignoring memory growth failure error: {error:?}");
            Ok(())
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> Result<bool> {
        let allow = match self.table_elements {
            Some(limit) if desired > limit => false,
            _ => match maximum {
                Some(max) if desired > max => false,
                _ => true,
            },
        };
        if !allow && self.trap_on_grow_failure {
            bail!("forcing trap when growing table to {desired} elements")
        } else {
            Ok(allow)
        }
    }

    fn table_grow_failed(&mut self, error: crate::Error) -> Result<()> {
        if self.trap_on_grow_failure {
            Err(error.context("forcing a table growth failure to be a trap"))
        } else {
            log::debug!("ignoring table growth failure error: {error:?}");
            Ok(())
        }
    }

    fn instances(&self) -> usize {
        self.instances
    }

    fn tables(&self) -> usize {
        self.tables
    }

    fn memories(&self) -> usize {
        self.memories
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine, Instance, Module, Store};
    use alloc::vec::Vec;

    /// One limiter callback, recorded in the order it was observed.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Event {
        Growing(usize, usize, MemoryKind),
        Grown(usize, usize, MemoryKind),
        GrowFailed(MemoryKind),
    }

    impl Event {
        fn kind(&self) -> MemoryKind {
            match self {
                Event::Growing(_, _, k) | Event::Grown(_, _, k) | Event::GrowFailed(k) => *k,
            }
        }
    }

    /// A limiter that records every callback and can be told to reject growth
    /// or to panic from `memory_grown`.
    struct Recorder {
        events: Vec<Event>,
        /// Kinds whose growth requests are rejected by `memory_growing`.
        reject: Option<MemoryKind>,
        /// If set, the *next* `memory_grown` for this kind panics after
        /// recording itself. It fires once so that the store can be exercised
        /// afterwards to prove it survived the unwind.
        panic_on_grown: Option<MemoryKind>,
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder {
                events: Vec::new(),
                reject: None,
                panic_on_grown: None,
            }
        }

        #[cfg(feature = "gc")]
        fn rejecting(kind: MemoryKind) -> Recorder {
            Recorder {
                reject: Some(kind),
                ..Recorder::new()
            }
        }

        fn panicking_on_grown(kind: MemoryKind) -> Recorder {
            Recorder {
                panic_on_grown: Some(kind),
                ..Recorder::new()
            }
        }

        fn of_kind(&self, kind: MemoryKind) -> Vec<Event> {
            self.events
                .iter()
                .copied()
                .filter(|e| e.kind() == kind)
                .collect()
        }

        /// Asserts the callback protocol holds: after discarding the leading
        /// `initial_allocations` requests (initial allocations are deliberately
        /// never followed by `memory_grown`), every permitted `memory_growing`
        /// is immediately resolved by exactly one `memory_grown` or
        /// `memory_grow_failed` carrying the same kind and sizes.
        ///
        /// A *rejected* growth resolves itself and gets no follow-up: the
        /// limiter said no, so it never reserved anything that needs releasing.
        ///
        /// Note this deliberately rejects a `GrowFailed` with no preceding
        /// `Growing`. Such a sequence is legal in general — a request the
        /// memory's type cannot represent is refused before the limiter is
        /// consulted, see `unrepresentable_growth_fails_without_a_request` — but
        /// no test using this helper should be provoking that path, so seeing it
        /// here means the test is not exercising what it claims to.
        fn assert_growth_protocol(&self, initial_allocations: usize) {
            let mut remaining_initial = initial_allocations;
            let mut events = self.events.iter().copied().peekable();
            while let Some(event) = events.next() {
                let Event::Growing(current, desired, kind) = event else {
                    panic!("unpaired {event:?} in {:?}", self.events);
                };
                if remaining_initial > 0 {
                    remaining_initial -= 1;
                    continue;
                }
                if self.reject == Some(kind) {
                    continue;
                }
                match events.next() {
                    Some(Event::Grown(c, d, k)) => {
                        assert_eq!(
                            (c, d, k),
                            (current, desired, kind),
                            "mismatched resolution in {:?}",
                            self.events
                        );
                    }
                    Some(Event::GrowFailed(k)) => assert_eq!(
                        k, kind,
                        "`memory_grow_failed` reported the wrong kind in {:?}",
                        self.events
                    ),
                    other => panic!(
                        "permitted growth {event:?} was resolved by {other:?} in {:?}",
                        self.events
                    ),
                }
            }
            assert_eq!(
                remaining_initial, 0,
                "expected {initial_allocations} initial allocations in {:?}",
                self.events
            );
        }
    }

    impl ResourceLimiter for Recorder {
        fn memory_growing(
            &mut self,
            current: usize,
            desired: usize,
            _maximum: Option<usize>,
            kind: MemoryKind,
        ) -> Result<bool> {
            self.events.push(Event::Growing(current, desired, kind));
            if self.reject == Some(kind) {
                return Ok(false);
            }
            Ok(true)
        }

        fn memory_grown(&mut self, current: usize, desired: usize, kind: MemoryKind) {
            self.events.push(Event::Grown(current, desired, kind));
            if self.panic_on_grown == Some(kind) {
                self.panic_on_grown = None;
                panic!("memory_grown panicked on purpose");
            }
        }

        fn memory_grow_failed(&mut self, _error: crate::Error, kind: MemoryKind) -> Result<()> {
            self.events.push(Event::GrowFailed(kind));
            Ok(())
        }

        fn table_growing(
            &mut self,
            _current: usize,
            _desired: usize,
            _maximum: Option<usize>,
        ) -> Result<bool> {
            Ok(true)
        }
    }

    const MEM_WAT: &str = r#"
        (module
          (memory (export "m") 1)
          (func (export "load") (param i32) (result i32)
            local.get 0
            i32.load)
          (func (export "store") (param i32) (param i32)
            local.get 0
            local.get 1
            i32.store))
    "#;

    /// A config whose linear memories are backed by a virtual reservation large
    /// enough for the growth these tests perform, so the base pointer never
    /// moves. Set explicitly rather than relying on the default.
    fn fixed_base_config() -> Config {
        let mut config = Config::new();
        config.memory_reservation(16 << 20).memory_may_move(false);
        config
    }

    /// A config with no spare reservation, forcing growth to reallocate and move
    /// the memory's base pointer.
    fn moving_base_config() -> Config {
        let mut config = Config::new();
        config
            .memory_reservation(0)
            .memory_reservation_for_growth(0)
            .memory_guard_size(0)
            .memory_may_move(true);
        config
    }

    /// A config with a GC heap small enough that a handful of allocations
    /// force it to grow.
    #[cfg(feature = "gc")]
    fn gc_config() -> Config {
        let mut config = Config::new();
        config.wasm_gc(true).gc_heap_reservation(1 << 16);
        config
    }

    const PAGE: usize = 64 * 1024;
    /// An address that only exists once the memory has grown to two pages.
    const GROWN_ADDR: i32 = PAGE as i32 + 128;

    /// GOL-424: a `memory_grown` callback that panics must not be able to leave
    /// a `VMContext` holding a stale base pointer or length. After catching the
    /// panic the memory reports its new size and compiled wasm can read and
    /// write the newly committed region.
    fn memory_grown_panic_is_unwind_safe(mut config: Config, expect_base_move: bool) {
        let engine = Engine::new(&config.wasm_multi_memory(true)).unwrap();
        let module = Module::new(&engine, MEM_WAT).unwrap();

        let mut store = Store::new(
            &engine,
            Recorder::panicking_on_grown(MemoryKind::LinearMemory),
        );
        store.limiter(|r| r);

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let memory = instance.get_memory(&mut store, "m").unwrap();
        let load = instance
            .get_typed_func::<i32, i32>(&mut store, "load")
            .unwrap();
        let store_fn = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "store")
            .unwrap();

        let base_before = memory.data_ptr(&store);

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = memory.grow(&mut store, 1);
        }))
        .is_err();
        assert!(panicked, "expected the limiter's panic to propagate");

        // The growth committed, so the memory must report the new size.
        assert_eq!(memory.size(&store), 2);
        assert_eq!(memory.data_size(&store), 2 * PAGE);

        let base_after = memory.data_ptr(&store);
        if expect_base_move {
            assert_ne!(
                base_before, base_after,
                "expected this configuration to relocate the memory on growth"
            );
        }

        // The instance must still be usable, and crucially the compiled wasm
        // must see the *new* base and bounds: this store traps if the VMContext
        // still holds the pre-growth length, and corrupts freed memory if it
        // still holds the pre-growth base.
        store_fn
            .call(&mut store, (GROWN_ADDR, 0x1234_5678))
            .unwrap();
        assert_eq!(load.call(&mut store, GROWN_ADDR).unwrap(), 0x1234_5678);

        // The host's view of the memory must agree with what wasm just wrote,
        // which is only true if both are looking at the same allocation.
        let data = memory.data(&store);
        let addr = GROWN_ADDR as usize;
        assert_eq!(
            u32::from_le_bytes(data[addr..addr + 4].try_into().unwrap()),
            0x1234_5678
        );

        // And the store is healthy enough to grow again. The limiter's panic is
        // one-shot, so this growth runs its `memory_grown` normally.
        assert_eq!(memory.grow(&mut store, 1).unwrap(), 2);
        assert_eq!(memory.size(&store), 3);
        assert_eq!(load.call(&mut store, GROWN_ADDR).unwrap(), 0x1234_5678);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn memory_grown_panic_is_unwind_safe_with_fixed_base() {
        memory_grown_panic_is_unwind_safe(fixed_base_config(), false);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn memory_grown_panic_is_unwind_safe_with_moving_base() {
        memory_grown_panic_is_unwind_safe(moving_base_config(), true);
    }

    /// Ordinary linear-memory growth still reports `LinearMemory` and still
    /// pairs each permitted growth with exactly one resolution.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn linear_memory_growth_callback_sequence() {
        let engine = Engine::default();
        let module = Module::new(&engine, r#"(module (memory (export "m") 1 2))"#).unwrap();
        let mut store = Store::new(&engine, Recorder::new());
        store.limiter(|r| r);

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let memory = instance.get_memory(&mut store, "m").unwrap();

        assert_eq!(memory.grow(&mut store, 1).unwrap(), 1);
        // Growing past the declared maximum is permitted by this limiter but
        // fails in the allocator, which must report `memory_grow_failed`.
        assert!(memory.grow(&mut store, 1).is_err());

        let events = store.data().of_kind(MemoryKind::LinearMemory);
        assert_eq!(
            events,
            [
                Event::Growing(0, PAGE, MemoryKind::LinearMemory),
                Event::Growing(PAGE, 2 * PAGE, MemoryKind::LinearMemory),
                Event::Grown(PAGE, 2 * PAGE, MemoryKind::LinearMemory),
                Event::Growing(2 * PAGE, 3 * PAGE, MemoryKind::LinearMemory),
                Event::GrowFailed(MemoryKind::LinearMemory),
            ]
        );
        store.data().assert_growth_protocol(1);
        assert!(store.data().of_kind(MemoryKind::GcHeap).is_empty());
    }

    /// A growth the memory's own type cannot represent is refused before the
    /// limiter is ever asked, so `memory_grow_failed` arrives with no preceding
    /// `memory_growing`. This is the one sequence `memory_grow_failed`'s docs
    /// call out as unpaired; assert it really happens so the docs and
    /// `assert_growth_protocol`'s strictness stay honest.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn unrepresentable_growth_fails_without_a_request() {
        // A 1-byte-page memory cannot address the whole 32-bit range, so growth
        // towards it is rejected by the memory type itself.
        let mut config = Config::new();
        config.wasm_custom_page_sizes(true);
        let engine = Engine::new(&config).unwrap();
        let module =
            Module::new(&engine, r#"(module (memory (export "m") 1 (pagesize 1)))"#).unwrap();

        let mut store = Store::new(&engine, Recorder::new());
        store.limiter(|r| r);

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let memory = instance.get_memory(&mut store, "m").unwrap();
        let before = store.data().events.len();

        assert!(memory.grow(&mut store, 1 << 32).is_err());

        assert_eq!(
            &store.data().events[before..],
            [Event::GrowFailed(MemoryKind::LinearMemory)],
            "expected a lone failure with no growth request; saw {:?}",
            store.data().events,
        );
    }

    /// Growing by zero pages is a no-op for the limiter: nothing was requested,
    /// so nothing is reported.
    #[test]
    #[cfg_attr(miri, ignore)]
    fn zero_page_growth_is_not_reported() {
        let engine = Engine::default();
        let module = Module::new(&engine, r#"(module (memory (export "m") 1 2))"#).unwrap();
        let mut store = Store::new(&engine, Recorder::new());
        store.limiter(|r| r);

        let instance = Instance::new(&mut store, &module, &[]).unwrap();
        let memory = instance.get_memory(&mut store, "m").unwrap();
        let before = store.data().events.len();

        assert_eq!(memory.grow(&mut store, 0).unwrap(), 1);
        assert_eq!(store.data().events.len(), before);
    }

    /// GOL-425: a GC heap that grows successfully must resolve the
    /// `memory_growing` that permitted it, and must be labelled `GcHeap` rather
    /// than passed off as guest linear memory.
    #[cfg(feature = "gc")]
    #[test]
    #[cfg_attr(miri, ignore)]
    fn gc_heap_growth_reports_gc_heap_kind() {
        use crate::ExternRef;

        let engine = Engine::new(&gc_config()).unwrap();

        let mut store = Store::new(&engine, Recorder::new());
        store.limiter(|r| r);

        // Keep allocating rooted GC objects until the heap has to grow.
        let mut roots = Vec::new();
        for i in 0..10_000u32 {
            roots.push(ExternRef::new(&mut store, i).unwrap());
            if store
                .data()
                .events
                .iter()
                .any(|e| matches!(e, Event::Grown(_, _, MemoryKind::GcHeap)))
            {
                break;
            }
        }

        let events = store.data().of_kind(MemoryKind::GcHeap);
        assert!(
            events.iter().any(|e| matches!(e, Event::Grown(..)))
                && events.iter().any(|e| matches!(e, Event::Growing(..))),
            "expected the GC heap to grow at least once; saw {events:?}",
        );
        for event in &events {
            if let Event::Grown(current, desired, _) = event {
                assert!(desired > current, "reported a non-growth: {event:?}");
            }
        }
        store.data().assert_growth_protocol(1);

        // GC heap capacity must never be attributed to guest linear memory.
        assert!(
            !store
                .data()
                .events
                .iter()
                .any(|e| matches!(e, Event::Grown(_, _, MemoryKind::LinearMemory))),
            "GC heap growth must not be reported as linear-memory growth",
        );
    }

    /// GOL-424 for the GC heap: a `memory_grown` that panics unwinds through
    /// `TakenGcHeap::drop`, which is what hands the grown memory and its size
    /// delta back to the collector. If the notification ran before that delta
    /// was computed the collector would be told the heap grew by zero bytes.
    /// The deferred reference-counting collector believes that delta — it feeds
    /// it straight to `FreeList::add_capacity` — so it would never use the new
    /// capacity and would have to grow all over again on the next allocation.
    /// (The copying and null collectors recompute from the memory's size, so
    /// they mask this; hence the collector is pinned rather than defaulted.)
    #[cfg(feature = "gc-drc")]
    #[test]
    #[cfg_attr(miri, ignore)]
    fn gc_heap_grown_panic_is_unwind_safe() {
        use crate::{Collector, ExternRef};

        let mut config = Config::new();
        config
            .wasm_gc(true)
            .collector(Collector::DeferredReferenceCounting)
            .gc_heap_reservation(1 << 16);
        let engine = Engine::new(&config).unwrap();

        let mut store = Store::new(&engine, Recorder::panicking_on_grown(MemoryKind::GcHeap));
        store.limiter(|r| r);

        // Allocate until the one-shot panic fires out of the first GC heap
        // growth.
        let mut roots = Vec::new();
        let mut panicked = false;
        for i in 0..10_000u32 {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ExternRef::new(&mut store, i)
            }));
            match caught {
                Ok(Ok(r)) => roots.push(r),
                Ok(Err(e)) => panic!("GC allocation failed before the heap grew: {e:?}"),
                Err(_) => {
                    panicked = true;
                    break;
                }
            }
        }
        assert!(panicked, "expected the limiter's panic to propagate");

        let growths = |store: &Store<Recorder>| {
            store
                .data()
                .events
                .iter()
                .filter(|e| matches!(e, Event::Grown(_, _, MemoryKind::GcHeap)))
                .count()
        };
        let growths_at_panic = growths(&store);
        assert_eq!(
            growths_at_panic,
            1,
            "expected the panic to come from the first GC heap growth; saw {:?}",
            store.data().events,
        );

        // The growth committed, so the collector must have been handed the new
        // capacity before the unwind. If it was not, its free list gained zero
        // bytes and the very next allocation has to grow the heap all over
        // again — so allocating well within the capacity just added must not
        // trigger another growth.
        for i in 0..50u32 {
            roots.push(
                ExternRef::new(&mut store, i)
                    .expect("store must still allocate after the caught panic"),
            );
        }
        assert_eq!(
            growths(&store),
            growths_at_panic,
            "the capacity from the growth that panicked was lost: the collector \
             had to grow again immediately; saw {:?}",
            store.data().events,
        );
    }

    /// GOL-425: a GC heap growth the limiter *permits* but that then fails in
    /// the allocator must resolve with `memory_grow_failed` carrying `GcHeap`,
    /// not `LinearMemory`. Without the kind on the failure callback an embedder
    /// could not tell which reservation to release.
    #[cfg(feature = "gc")]
    #[test]
    #[cfg_attr(miri, ignore)]
    fn failed_gc_heap_growth_reports_gc_heap_kind() {
        use crate::ExternRef;

        // Pin the heap to a single reservation it may not move out of, so
        // growing beyond that capacity is permitted by the limiter and then
        // fails in the allocator.
        let mut config = Config::new();
        config
            .wasm_gc(true)
            .gc_heap_reservation(1 << 16)
            .gc_heap_reservation_for_growth(0)
            .gc_heap_may_move(false);
        let engine = Engine::new(&config).unwrap();

        let mut store = Store::new(&engine, Recorder::new());
        store.limiter(|r| r);

        let mut roots = Vec::new();
        for i in 0..100_000u32 {
            match ExternRef::new(&mut store, i) {
                Ok(r) => roots.push(r),
                Err(_) => break,
            }
            if store
                .data()
                .events
                .iter()
                .any(|e| matches!(e, Event::GrowFailed(_)))
            {
                break;
            }
        }

        let events = store.data().of_kind(MemoryKind::GcHeap);
        assert!(
            events.iter().any(|e| matches!(e, Event::GrowFailed(..))),
            "expected a permitted GC heap growth to fail in the allocator; saw {events:?}",
        );
        assert!(
            !store
                .data()
                .events
                .iter()
                .any(|e| matches!(e, Event::GrowFailed(MemoryKind::LinearMemory))),
            "a GC heap growth failure must not be reported as a linear-memory \
             failure; saw {:?}",
            store.data().events,
        );
        store.data().assert_growth_protocol(1);
    }

    /// A limiter that refuses GC heap growth sees the request and no
    /// resolution, because a refusal resolves itself. It must never see a
    /// `Grown` for a growth it rejected.
    #[cfg(feature = "gc")]
    #[test]
    #[cfg_attr(miri, ignore)]
    fn rejected_gc_heap_growth_reports_no_commit() {
        use crate::ExternRef;

        let engine = Engine::new(&gc_config()).unwrap();

        let mut store = Store::new(&engine, Recorder::rejecting(MemoryKind::GcHeap));
        store.limiter(|r| r);

        // Allocate until the heap is exhausted; growth is refused, so this must
        // eventually fail rather than silently succeed.
        let mut roots = Vec::new();
        let mut hit_limit = false;
        for i in 0..10_000u32 {
            match ExternRef::new(&mut store, i) {
                Ok(r) => roots.push(r),
                Err(_) => {
                    hit_limit = true;
                    break;
                }
            }
        }
        assert!(
            hit_limit,
            "expected GC allocation to fail once growth was refused"
        );

        let events = store.data().of_kind(MemoryKind::GcHeap);
        assert!(
            events.iter().any(|e| matches!(e, Event::Growing(..))),
            "expected at least one GC heap growth request; saw {events:?}",
        );
        assert!(
            !events.iter().any(|e| matches!(e, Event::Grown(..))),
            "a rejected growth must never be reported as committed: {events:?}",
        );
        store.data().assert_growth_protocol(1);
    }
}
