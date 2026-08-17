use super::*;
use crate::component::store::{ComponentInstanceId, RuntimeInstance};
use wasmtime_environ::component::RuntimeComponentInstanceIndex;

fn push_guest(
    state: &mut ConcurrentState,
    context: Option<OpaqueGuestTaskContext>,
) -> QualifiedThreadId {
    let thread = GuestTask::new(
        state,
        Box::new(|_, _| panic!("test guest parameters are never lowered")),
        LiftResult {
            lift: Box::new(|_, _| panic!("test guest results are never lifted")),
            ty: TypeTupleIndex::reserved_value(),
            memory: None,
            string_encoding: StringEncoding::Utf8,
        },
        Caller::Host {
            tx: None,
            host_future_present: false,
            caller: CurrentThread::None,
        },
        None,
        RuntimeInstance {
            instance: ComponentInstanceId::from_u32(0),
            index: RuntimeComponentInstanceIndex::from_u32(0),
        },
        true,
    )
    .unwrap();
    state.get_mut(thread.task).unwrap().embedder_context = context;
    thread
}

fn inherited_u32(state: &mut ConcurrentState, caller: &Caller) -> Option<Arc<u32>> {
    GuestTask::inherited_embedder_context(state, caller)
        .unwrap()
        .map(|context| context.downcast().unwrap())
}

#[test]
fn active_host_context_is_nested_and_restored() {
    assert!(current_guest_task_context::<u32>().unwrap().is_none());
    let future = poll_with_guest_task_context(Some(Arc::new(7_u32)), async {
        assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 7);
        with_guest_task_context(Some(Arc::new(8_u32)), || {
            assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 8);
        });
        assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 7);
        with_guest_task_context(None, || {
            assert!(current_guest_task_context::<u32>().unwrap().is_none());
        });
        assert_eq!(*current_guest_task_context::<u32>().unwrap().unwrap(), 7);
    });
    let mut future = pin!(future);
    let waker = futures::task::noop_waker();
    assert!(matches!(
        future.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(())
    ));
    assert!(current_guest_task_context::<u32>().unwrap().is_none());
}

#[test]
fn guest_context_is_snapshotted_for_children() {
    let mut state = ConcurrentState::default();
    let context_a = Arc::new(1_u32);
    let parent = push_guest(&mut state, Some(context_a.clone()));
    let caller = Caller::Guest { thread: parent };

    let child_a_context = GuestTask::inherited_embedder_context(&mut state, &caller).unwrap();
    let child_a = push_guest(&mut state, child_a_context);
    state.get_mut(parent.task).unwrap().embedder_context = Some(Arc::new(2_u32));
    let child_b_context = GuestTask::inherited_embedder_context(&mut state, &caller).unwrap();
    let child_b = push_guest(&mut state, child_b_context);
    state.get_mut(parent.task).unwrap().embedder_context = None;

    let child_context = |state: &mut ConcurrentState, child: QualifiedThreadId| {
        state
            .get_mut(child.task)
            .unwrap()
            .embedder_context
            .as_ref()
            .unwrap()
            .clone()
            .downcast::<u32>()
            .unwrap()
    };
    assert_eq!(*child_context(&mut state, child_a), 1);
    assert_eq!(*child_context(&mut state, child_b), 2);
    assert_eq!(*context_a, 1);
    assert!(inherited_u32(&mut state, &caller).is_none());
    assert!(
        inherited_u32(
            &mut state,
            &Caller::Host {
                tx: None,
                host_future_present: false,
                caller: CurrentThread::None,
            }
        )
        .is_none()
    );
}

#[test]
fn context_follows_direct_guest_and_host_ancestry() {
    let mut state = ConcurrentState::default();
    let root = push_guest(&mut state, Some(Arc::new(7_u32)));
    assert_eq!(
        *inherited_u32(&mut state, &Caller::Guest { thread: root }).unwrap(),
        7
    );

    let host = state
        .push(HostTask::new(
            root,
            HostTaskState::CalleeStarted,
            Some(Arc::new(7_u32)),
        ))
        .unwrap();
    state.get_mut(root.task).unwrap().embedder_context = Some(Arc::new(8_u32));
    assert_eq!(
        *inherited_u32(
            &mut state,
            &Caller::Host {
                tx: None,
                host_future_present: false,
                caller: CurrentThread::Host(host),
            }
        )
        .unwrap(),
        7
    );
}

#[test]
fn deleting_child_releases_only_its_context_reference() {
    let mut state = ConcurrentState::default();
    let context = Arc::new(7_u32);
    let parent = push_guest(&mut state, Some(context.clone()));
    let inherited =
        GuestTask::inherited_embedder_context(&mut state, &Caller::Guest { thread: parent })
            .unwrap();
    let child = push_guest(&mut state, inherited);
    assert_eq!(Arc::strong_count(&context), 3);

    drop(state.delete(child.task).unwrap());
    assert_eq!(Arc::strong_count(&context), 2);
    assert_eq!(
        *state
            .get_mut(parent.task)
            .unwrap()
            .embedder_context
            .as_ref()
            .unwrap()
            .clone()
            .downcast::<u32>()
            .unwrap(),
        7
    );
}
