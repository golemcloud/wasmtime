//! Policy-free observation of the concurrent runtime.
//!
//! An embedder which arbitrates Store suspension outside Wasmtime installs a
//! [`RuntimeObserver`] per Store. The runtime then reports which work it has
//! admitted and when its event loop reaches a genuine no-work boundary. Nothing
//! here makes a suspension decision; that policy stays with the embedder.
//!
//! When no observer is installed none of the machinery in this module is
//! instantiated, so the concurrent runtime behaves and performs as it does
//! without this module.

use crate::store::StoreId;
use alloc::sync::Arc;
use alloc::task::Wake;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use core::task::{Context, Poll, Waker};
use futures::task::AtomicWaker;
use std::cell::Cell;

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_RUNTIME_RUN_ID: AtomicU64 = AtomicU64::new(1);

/// An opaque identity which is never reused during a process lifetime.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RuntimeActivityId(u64);

impl RuntimeActivityId {
    fn next() -> Self {
        Self(NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Identity of one driver run, ordered by admission.
///
/// Successive driver runs on the same Store have strictly increasing identities.
/// A token is assigned when the driver is first polled, before registering its
/// parent waker or emitting observations. Creating and dropping an unpolled
/// driver does not assign a token. Notifications from old saved wakers can arrive
/// after a newer run has started, but retain their original token.
///
/// Tokens are never reused. Exhausting the process-wide token space panics before
/// a token is published; the allocator remains exhausted even if panic is caught.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RuntimeRunId(u64);

impl RuntimeRunId {
    fn next(counter: &AtomicU64) -> Self {
        Self(
            counter
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .expect("runtime run identity space exhausted"),
        )
    }
}

/// The runtime-owned work represented by an activity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeActivityKind {
    /// The future passed to the concurrent driver.
    Root,
    /// A concurrent host import.
    Import,
    /// An [`AccessorTask`](super::AccessorTask).
    Background,
    /// A future or stream transfer.
    Transfer,
    /// A fiber which retains access to the Store while suspended.
    StoreRetainingFiber,
    /// A queued call into guest code, including event delivery to a guest task.
    GuestCall,
    /// A queued host closure which runs on a worker fiber, such as lowering the
    /// result of an async import.
    WorkerFunction,
}

/// Why a previously published blocked observation is no longer current.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeInvalidation {
    /// The outer driver was polled again.
    Poll,
    /// The inner driver waker was woken.
    Wake,
    /// The outer driver was dropped.
    DriverDrop,
}

/// Synchronous, policy-free notifications from the component async runtime.
#[derive(Clone, Debug)]
pub enum RuntimeObservation {
    /// An activity was registered before its first poll.
    ActivityStarted {
        /// Opaque activity identity.
        activity: RuntimeActivityId,
        /// Runtime classification of the activity.
        kind: RuntimeActivityKind,
    },
    /// An activity finished.
    ActivityFinished {
        /// Opaque activity identity from the matching start event.
        activity: RuntimeActivityId,
    },
    /// The inner driver reached a genuine no-work boundary.
    DriverBlocked {
        /// Opaque driver run identity.
        run: RuntimeRunId,
        /// Generation invalidated by the next poll, wake, or driver drop.
        generation: usize,
    },
    /// A blocked generation was invalidated synchronously.
    DriverInvalidated {
        /// Opaque driver run identity.
        run: RuntimeRunId,
        /// Generations strictly below this watermark are invalid. Observers must retain the
        /// greatest watermark even if the blocked callback has not arrived yet.
        generation: usize,
        /// The operation which invalidated the generation.
        reason: RuntimeInvalidation,
    },
}

/// Receives runtime observations synchronously on the thread causing them.
///
/// Install one observer per Store before creating work. Activity identities belong to that
/// Store and may outlive a driver run. Every activity notification invalidates any eligibility
/// conclusion derived from an earlier blocked notification; observers must apply the activity
/// change before reconsidering eligibility. Callbacks from different threads may overlap: retain
/// monotonically increasing invalidation watermarks and reject older blocked observations.
/// `DriverDrop` permanently retires a run. A driver poll remains active until its enclosing poll
/// returns.
///
/// # Re-entrancy
///
/// Callbacks run on whichever thread causes them, and from inside runtime operations the
/// embedder may itself be performing:
///
/// - [`RuntimeInvalidation::Wake`] is delivered from inside [`Waker::wake`], on the waking
///   thread (for example an async runtime's timer or I/O driver thread).
/// - [`RuntimeObservation::ActivityFinished`] is delivered whenever the runtime drops the
///   work, including while the `Store` itself is dropped, while a `run_concurrent` future is
///   dropped, and while a host future is dropped on an arbitrary thread.
///
/// Therefore an observer must not block, must not wait for runtime progress, must not
/// re-enter the Store, and must not acquire a lock which the embedder may hold while it drops
/// a `Store`, polls or drops a driver future, drops a host future, or wakes a `Waker`.
pub trait RuntimeObserver: Send + Sync + 'static {
    /// Handles one synchronous runtime notification.
    fn observe(&self, event: RuntimeObservation);
}

impl<F: Fn(RuntimeObservation) + Send + Sync + 'static> RuntimeObserver for F {
    fn observe(&self, event: RuntimeObservation) {
        self(event)
    }
}

/// Reports the lifetime of one unit of admitted runtime work to an observer.
///
/// Started on construction, finished on drop. Only created when an observer is installed.
pub(super) struct ActivityGuard {
    observer: Arc<dyn RuntimeObserver>,
    pub(super) activity: RuntimeActivityId,
}

impl ActivityGuard {
    pub(super) fn new(observer: Arc<dyn RuntimeObserver>, kind: RuntimeActivityKind) -> Self {
        let activity = RuntimeActivityId::next();
        observer.observe(RuntimeObservation::ActivityStarted { activity, kind });
        Self { observer, activity }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.observer.observe(RuntimeObservation::ActivityFinished {
            activity: self.activity,
        });
    }
}

/// The waker interposed between the executor and one driver run so that wakes
/// can invalidate a published blocked generation.
///
/// `epoch` holds even values after an invalidation and odd values after a
/// publication. `start` is the epoch at the beginning of the current outer
/// poll; a publication succeeds only if no invalidation happened since then.
struct ProbeWake {
    run: RuntimeRunId,
    epoch: AtomicUsize,
    start: AtomicUsize,
    parent: AtomicWaker,
    observer: Arc<dyn RuntimeObserver>,
}

impl ProbeWake {
    fn invalidate(&self, reason: RuntimeInvalidation) -> usize {
        let old = self
            .epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                n.checked_add(2).map(|n| n & !1)
            })
            .expect("runtime driver generation exhausted");
        let generation = (old + 2) & !1;
        self.observer
            .observe(RuntimeObservation::DriverInvalidated {
                run: self.run,
                generation,
                reason,
            });
        generation
    }
}

impl Wake for ProbeWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.invalidate(RuntimeInvalidation::Wake);
        self.parent.wake();
    }
}

/// One observed driver run, owned by the driver future for its whole lifetime.
pub(super) struct DriverProbe {
    probe: Arc<ProbeWake>,
    /// The waker handed to the inner driver, built once per run.
    waker: Waker,
}

impl DriverProbe {
    pub(super) fn new(observer: Arc<dyn RuntimeObserver>) -> Self {
        let probe = Arc::new(ProbeWake {
            run: RuntimeRunId::next(&NEXT_RUNTIME_RUN_ID),
            epoch: AtomicUsize::new(0),
            start: AtomicUsize::new(0),
            parent: AtomicWaker::new(),
            observer,
        });
        let waker = Waker::from(probe.clone());
        Self { probe, waker }
    }

    /// Polls `inner` with the probe waker, recording the generation at which
    /// this poll started.
    pub(super) fn poll<F: Future>(
        &self,
        inner: Pin<&mut F>,
        cx: &mut Context<'_>,
    ) -> Poll<F::Output> {
        self.probe.parent.register(cx.waker());
        let start = self.probe.invalidate(RuntimeInvalidation::Poll);
        self.probe.start.store(start, Ordering::SeqCst);
        inner.poll(&mut Context::from_waker(&self.waker))
    }

    /// Publishes a blocked observation for the current poll unless work is
    /// queued or a wake arrived since the poll started.
    pub(super) fn publish(&self, queues_empty: bool) {
        let probe = &self.probe;
        let start = probe.start.load(Ordering::SeqCst);
        if queues_empty
            && probe
                .epoch
                .compare_exchange(start, start | 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            probe.observer.observe(RuntimeObservation::DriverBlocked {
                run: probe.run,
                generation: start | 1,
            });
        }
    }
}

impl Drop for DriverProbe {
    fn drop(&mut self) {
        self.probe.invalidate(RuntimeInvalidation::DriverDrop);
    }
}

std::thread_local! {
    static ACTIVITY: Cell<Option<(StoreId, RuntimeActivityId)>> = const { Cell::new(None) };
}

/// Marks the activity whose future is being polled on this thread, for
/// [`StoreContextMut::runtime_activity`](crate::StoreContextMut::runtime_activity).
pub(super) struct ActivityScope(Option<(StoreId, RuntimeActivityId)>);

impl ActivityScope {
    pub(super) fn enter(activity: Option<(StoreId, RuntimeActivityId)>) -> Self {
        Self(ACTIVITY.with(|current| current.replace(activity)))
    }
}

impl Drop for ActivityScope {
    fn drop(&mut self) {
        ACTIVITY.with(|current| current.set(self.0));
    }
}

pub(super) fn current_activity(store: StoreId) -> Option<RuntimeActivityId> {
    ACTIVITY.with(|current| {
        current
            .get()
            .filter(|(id, _)| *id == store)
            .map(|(_, id)| id)
    })
}

#[cfg(test)]
mod tests {
    use super::super::{ConcurrentState, WorkItem, tls};
    use super::*;
    use crate::vm::AlwaysMut;
    use crate::{AsContextMut, Engine, Store};
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use core::future;
    use core::mem;
    use std::sync::{Condvar, Mutex};

    #[test]
    fn exhausted_run_identity_allocator_never_wraps_or_recovers() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(RuntimeRunId::next(&counter), RuntimeRunId(u64::MAX - 1));
        for _ in 0..2 {
            assert!(std::panic::catch_unwind(|| RuntimeRunId::next(&counter)).is_err());
            assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        }
    }

    #[test]
    fn exhausted_driver_generation_never_publishes_a_wrapped_watermark() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let probe = ProbeWake {
            run: RuntimeRunId(1),
            epoch: AtomicUsize::new(usize::MAX - 3),
            start: AtomicUsize::new(0),
            parent: AtomicWaker::new(),
            observer: Arc::new(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
            }),
        };
        assert_eq!(probe.invalidate(RuntimeInvalidation::Poll), usize::MAX - 1);
        for _ in 0..2 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || probe.invalidate(RuntimeInvalidation::Wake)
                ))
                .is_err()
            );
            assert_eq!(probe.epoch.load(Ordering::Relaxed), usize::MAX - 1);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
        }
    }

    struct MonotonicObserver {
        state: Mutex<MonotonicState>,
        blocked_arrived: Condvar,
        release_blocked: Condvar,
    }

    #[derive(Default)]
    struct MonotonicState {
        invalid_before: usize,
        delay_blocked: bool,
        blocked_waiting: bool,
        accepted: Vec<usize>,
    }

    impl RuntimeObserver for MonotonicObserver {
        fn observe(&self, event: RuntimeObservation) {
            match event {
                RuntimeObservation::DriverBlocked { generation, .. } => {
                    let mut state = self.state.lock().unwrap();
                    if state.delay_blocked {
                        state.blocked_waiting = true;
                        self.blocked_arrived.notify_one();
                        while state.delay_blocked {
                            state = self.release_blocked.wait(state).unwrap();
                        }
                    }
                    if generation >= state.invalid_before {
                        state.accepted.push(generation);
                    }
                }
                RuntimeObservation::DriverInvalidated { generation, .. } => {
                    let mut state = self.state.lock().unwrap();
                    state.invalid_before = state.invalid_before.max(generation);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn delayed_blocked_publication_is_rejected_after_saved_wake_invalidation() {
        let observer = Arc::new(MonotonicObserver {
            state: Mutex::new(MonotonicState {
                delay_blocked: true,
                ..MonotonicState::default()
            }),
            blocked_arrived: Condvar::new(),
            release_blocked: Condvar::new(),
        });
        let probe = DriverProbe::new(observer.clone());
        let begin_poll = |probe: &DriverProbe| {
            let mut ready = core::pin::pin!(future::ready(()));
            assert!(
                probe
                    .poll(ready.as_mut(), &mut Context::from_waker(Waker::noop()))
                    .is_ready()
            );
        };
        begin_poll(&probe);

        std::thread::scope(|scope| {
            let publishing = &probe;
            let publisher = scope.spawn(move || publishing.publish(true));
            let mut state = observer.state.lock().unwrap();
            while !state.blocked_waiting {
                state = observer.blocked_arrived.wait(state).unwrap();
            }
            drop(state);

            let saved_wake = probe.waker.clone();
            saved_wake.wake_by_ref();

            let mut state = observer.state.lock().unwrap();
            state.delay_blocked = false;
            observer.release_blocked.notify_one();
            drop(state);
            publisher.join().unwrap();
        });

        assert!(observer.state.lock().unwrap().accepted.is_empty());

        begin_poll(&probe);
        let generation = probe.probe.start.load(Ordering::SeqCst);
        probe.publish(true);
        assert_eq!(observer.state.lock().unwrap().accepted, [generation | 1]);
    }

    #[test]
    fn wake_reaches_the_most_recently_registered_parent() {
        let probe = DriverProbe::new(Arc::new(|_| {}));
        let woken = Arc::new(AtomicUsize::new(0));
        struct Count(Arc<AtomicUsize>);
        impl Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut pending = core::pin::pin!(future::pending::<()>());
        assert!(
            probe
                .poll(pending.as_mut(), &mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let parent = Waker::from(Arc::new(Count(woken.clone())));
        assert!(
            probe
                .poll(pending.as_mut(), &mut Context::from_waker(&parent))
                .is_pending()
        );
        probe.waker.clone().wake();
        assert_eq!(woken.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelling_low_priority_yield_disposes_unexecuted_work_without_blocking() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        let executed = Arc::new(AtomicUsize::new(0));
        let ran = executed.clone();
        let mut store = Store::new(&Engine::default(), ());
        store
            .as_context_mut()
            .set_runtime_observer(Arc::new(move |event| observed.lock().unwrap().push(event)))
            .unwrap();
        store
            .as_context_mut()
            .0
            .concurrent_state_mut()
            .push_low_priority(WorkItem::WorkerFunction(AlwaysMut::new(Box::new(
                move |_| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            ))));

        let mut root = core::pin::pin!(future::pending::<()>());
        let mut driver = Box::pin(store.as_context_mut().poll_until(
            root.as_mut(),
            false,
            false,
            None,
        ));
        assert!(
            driver
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(driver);
        assert_eq!(executed.load(Ordering::SeqCst), 0);

        let events = events.lock().unwrap();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeObservation::DriverBlocked { .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeObservation::ActivityStarted { .. }))
                .count(),
            events
                .iter()
                .filter(|event| matches!(event, RuntimeObservation::ActivityFinished { .. }))
                .count(),
        );
    }

    #[test]
    fn observer_cannot_be_installed_after_work_or_twice() {
        let mut store = Store::new(&Engine::default(), ());
        store
            .as_context_mut()
            .set_runtime_observer(Arc::new(|_| {}))
            .unwrap();
        assert!(
            store
                .as_context_mut()
                .set_runtime_observer(Arc::new(|_| {}))
                .is_err()
        );

        let mut store = Store::new(&Engine::default(), ());
        store
            .as_context_mut()
            .0
            .concurrent_state_mut()
            .push_low_priority(WorkItem::WorkerFunction(AlwaysMut::new(Box::new(|_| {
                Ok(())
            }))));
        assert!(
            store
                .as_context_mut()
                .set_runtime_observer(Arc::new(|_| {}))
                .is_err()
        );
    }

    #[test]
    fn queued_work_is_registered_before_selection_and_balanced_on_drop() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let observed = events.clone();
        let mut state = ConcurrentState::default();
        state.runtime_observer = Some(Arc::new(move |event| observed.lock().unwrap().push(event)));

        state.push_low_priority(WorkItem::WorkerFunction(AlwaysMut::new(Box::new(|_| {
            Ok(())
        }))));
        let activity = match events.lock().unwrap().as_slice() {
            [
                RuntimeObservation::ActivityStarted {
                    activity,
                    kind: RuntimeActivityKind::WorkerFunction,
                },
            ] => *activity,
            events => panic!("unexpected admission events: {events:?}"),
        };

        let queued = state.low_priority.pop_back().unwrap();
        state.high_priority.push(queued);
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "promotion re-registered work"
        );

        let selected = mem::take(&mut state.high_priority);
        drop(selected);
        assert!(events.lock().unwrap().iter().any(|event| matches!(
            event,
            RuntimeObservation::ActivityFinished { activity: finished }
                if *finished == activity
        )));
    }

    #[test]
    fn queued_work_is_free_without_an_observer() {
        let mut state = ConcurrentState::default();
        state.push_low_priority(WorkItem::WorkerFunction(AlwaysMut::new(Box::new(|_| {
            Ok(())
        }))));
        assert!(
            state
                .low_priority
                .iter()
                .all(|queued| queued.activity.is_none())
        );
        assert!(state.start_activity(RuntimeActivityKind::Import).is_none());
    }

    #[test]
    fn activity_scope_restores_nested_polls_and_panics() {
        let engine = Engine::default();
        let mut store = Store::new(&engine, ());
        let id = store.as_context_mut().0.id();
        let outer = RuntimeActivityId(1);
        let inner = RuntimeActivityId(2);
        assert_eq!(store.as_context_mut().runtime_activity(), None);
        {
            let _outer = ActivityScope::enter(Some((id, outer)));
            assert_eq!(store.as_context_mut().runtime_activity(), Some(outer));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tls::set(store.as_context_mut().0, || {
                    tls::get(|store| {
                        assert_eq!(current_activity(store.id()), None);
                        let _inner = ActivityScope::enter(Some((store.id(), inner)));
                        assert_eq!(current_activity(store.id()), Some(inner));
                        panic!("nested poll");
                    });
                });
            }));
            assert!(result.is_err());
            assert_eq!(store.as_context_mut().runtime_activity(), Some(outer));
        }
        assert_eq!(store.as_context_mut().runtime_activity(), None);
    }
}
