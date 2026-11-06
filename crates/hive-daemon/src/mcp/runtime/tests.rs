use super::*;

#[tokio::test]
async fn an_unpolled_call_releases_admission_before_publishing_settlement() {
    let (mut instance, _) = idle_instance(60);
    Arc::get_mut(&mut instance).unwrap().slots = Arc::new(Semaphore::new(1));
    instance.leases.store(1, Ordering::Release);
    let lease = Lease {
        instance: instance.clone(),
    };
    // A separate current-thread runtime lets shutdown destroy the task before its first poll.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let call = {
        let _entered = runtime.enter();
        lease.request(McpRequest::new("ping", serde_json::json!({})).unwrap())
    };
    let settled = call.settlement();
    assert_eq!(instance.slots.available_permits(), 0);
    // Fence settlement publication so the assertion observes destructor order, not thread speed.
    let fence = instance.state.lock().unwrap();
    let shutdown = std::thread::spawn(move || runtime.shutdown_timeout(Duration::from_secs(5)));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while instance.operations.load(Ordering::Acquire) != 0 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    let retiring = instance.operations.load(Ordering::Acquire) == 0;
    let released = instance.slots.available_permits() == 1;
    let published = *settled.borrow();
    // Always unblock and join cleanup before reporting an assertion failure.
    drop(fence);
    shutdown.join().unwrap();
    assert!(retiring, "runtime shutdown never retired the unpolled call");
    assert!(
        released,
        "operation retirement still owns its admission slot"
    );
    assert!(!published, "settlement crossed the cleanup fence");
    assert!(*settled.borrow(), "shutdown did not publish settlement");
}

#[tokio::test(start_paused = true)]
async fn resource_protection_outlives_idle_timeout_and_waits_for_confirmed_process_exit() {
    let (instance, stopping) = idle_instance(60);
    instance.leases.store(1, Ordering::Release);
    let lease = Lease {
        instance: instance.clone(),
    };
    let done = lease.resource_settlement();
    drop(lease);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(
        *stopping.borrow(),
        "a resource waiter must not prevent idle shutdown"
    );
    assert!(!*done.borrow(), "sending stop does not prove termination");
    let mut state = instance.state.lock().unwrap();
    let generation = state.as_mut().unwrap();
    generation.done = watch::channel(true).1;
    generation.status = watch::channel(Status::Failed(RuntimeError::Cleanup)).1;
    instance.settle_resources(&state);
    assert!(
        !*done.borrow(),
        "unconfirmed cleanup must retain protection"
    );
    state.as_mut().unwrap().status = watch::channel(Status::Stopped).1;
    instance.settle_resources(&state);
    assert!(*done.borrow());
}

#[tokio::test(start_paused = true)]
async fn resource_waiters_cover_reacquisition_operations_and_replacement_generations() {
    let (instance, _) = idle_instance(60);
    instance.leases.store(1, Ordering::Release);
    let lease = Lease {
        instance: instance.clone(),
    };
    let done = lease.resource_settlement();
    drop(lease);
    {
        let mut state = instance.state.lock().unwrap();
        instance.leases.store(1, Ordering::Release);
        instance.cancel_idle();
        state.as_mut().unwrap().done = watch::channel(true).1;
        state.as_mut().unwrap().status = watch::channel(Status::Failed(RuntimeError::Connection)).1;
        instance.settle_resources(&state);
        assert!(
            !*done.borrow(),
            "a surviving restart-capable lease still needs files"
        );
        state.as_mut().unwrap().done = watch::channel(false).1;
        state.as_mut().unwrap().status = watch::channel(Status::Starting).1;
    }
    instance.operations.store(1, Ordering::Release);
    drop(Lease {
        instance: instance.clone(),
    });
    {
        let mut state = instance.state.lock().unwrap();
        state.as_mut().unwrap().done = watch::channel(true).1;
        state.as_mut().unwrap().status = watch::channel(Status::Stopped).1;
        instance.settle_resources(&state);
        assert!(!*done.borrow(), "operations can outlive their lease");
    }
    drop(Operation {
        instance,
        settled: watch::channel(false).0,
        permit: None,
    });
    assert!(*done.borrow());
}

// Synthetic generations test idle scheduling without coupling virtual time to OS pipes.
fn idle_instance(seconds: u64) -> (Arc<Instance>, watch::Receiver<bool>) {
    let mut identity = super::super::tests::identity();
    identity.definition.0.lifecycle = Lifecycle::OnDemand {
        idle_timeout_seconds: seconds,
    };
    let (_, status) = watch::channel(Status::Starting);
    let (stop, stopping) = watch::channel(false);
    let (_, done) = watch::channel(false);
    (
        Arc::new(Instance {
            resource_waiters: Mutex::new(Vec::new()),
            identity,
            policy: RuntimePolicy::default(),
            state: Mutex::new(Some(Generation {
                status,
                stop,
                done,
                starts: VecDeque::new(),
                failures: 0,
                retry_at: Instant::now(),
            })),
            leases: AtomicUsize::new(0),
            operations: AtomicUsize::new(0),
            idle_epoch: AtomicU64::new(0),
            idle_timer: Mutex::new(None),
            slots: Arc::new(Semaphore::new(64)),
            stopping: AtomicBool::new(false),
        }),
        stopping,
    )
}

#[tokio::test(start_paused = true)]
async fn idle_cleanup_requires_both_last_lease_and_last_operation_to_finish() {
    let (instance, stopping) = idle_instance(0);
    instance.leases.store(1, Ordering::Release);
    instance.operations.store(1, Ordering::Release);
    instance.idle();
    tokio::task::yield_now().await;
    assert!(!*stopping.borrow());
    instance.leases.store(0, Ordering::Release);
    instance.idle();
    tokio::task::yield_now().await;
    assert!(!*stopping.borrow());
    instance.operations.store(0, Ordering::Release);
    instance.idle();
    tokio::task::yield_now().await;
    assert!(*stopping.borrow());
}

#[tokio::test(start_paused = true)]
async fn reacquisition_invalidates_the_old_idle_deadline() {
    let (instance, stopping) = idle_instance(60);
    instance.idle();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    instance.leases.store(1, Ordering::Release);
    {
        let _state = instance.state.lock().unwrap();
        instance.cancel_idle();
    }
    assert!(instance.idle_timer.lock().unwrap().is_none());
    instance.leases.store(0, Ordering::Release);
    instance.idle();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    assert!(!*stopping.borrow());
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    assert!(*stopping.borrow());
}

#[test]
fn task_augmentation_is_rejected_even_when_null_but_nested_tool_arguments_are_allowed() {
    for task in [
        serde_json::json!({"ttl": 60000}),
        Value::Null,
        serde_json::json!(false),
    ] {
        assert!(matches!(
            McpRequest::new(
                "tools/call",
                serde_json::json!({"name": "work", "task": task})
            ),
            Err(RuntimeError::Request)
        ));
    }
    assert!(
        McpRequest::new(
            "tools/call",
            serde_json::json!({"name": "work", "arguments": {"task": "ordinary input"}})
        )
        .is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn rescheduling_aborts_the_previous_timer_and_completion_releases_its_handle() {
    let (instance, stopping) = idle_instance(60);
    instance.idle();
    let first = instance
        .idle_timer
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .abort_handle();
    instance.idle();
    tokio::task::yield_now().await;
    assert!(first.is_finished());
    assert!(instance.idle_timer.lock().unwrap().is_some());
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(*stopping.borrow());
    assert!(instance.idle_timer.lock().unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn dropping_an_instance_aborts_its_timer_without_waiting_for_the_deadline() {
    let (instance, _) = idle_instance(60);
    instance.idle();
    let timer = instance
        .idle_timer
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .abort_handle();
    let weak = Arc::downgrade(&instance);
    drop(instance);
    tokio::task::yield_now().await;
    assert!(weak.upgrade().is_none());
    assert!(timer.is_finished());
}

#[tokio::test(start_paused = true)]
async fn shutdown_and_registry_drop_cancel_owned_idle_timers() {
    for explicit_shutdown in [true, false] {
        let (instance, stopping) = idle_instance(60);
        // A completed synthetic generation lets shutdown exercise ownership without OS processes.
        instance.state.lock().unwrap().as_mut().unwrap().done = watch::channel(true).1;
        let runtime = Runtime(Arc::new(Registry {
            node: NodeId::new(),
            policy: RuntimePolicy::default(),
            instances: Mutex::new(vec![instance.clone()]),
            stopping: AtomicBool::new(false),
        }));
        instance.idle();
        let timer = instance
            .idle_timer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .abort_handle();
        if explicit_shutdown {
            runtime.shutdown().await;
        }
        drop(runtime);
        tokio::task::yield_now().await;
        assert!(timer.is_finished());
        assert!(instance.idle_timer.lock().unwrap().is_none());
        assert!(*stopping.borrow());
    }
}

#[test]
fn restart_window_boundaries_prune_and_compute_delay_from_the_same_instant() {
    let first = Instant::now();
    let window = Duration::from_secs(60);
    let starts = VecDeque::from([
        first,
        first + Duration::from_secs(1),
        first + Duration::from_secs(2),
    ]);
    assert_eq!(
        restart_window_remaining(
            &mut starts.clone(),
            first + window - Duration::from_nanos(1),
            window
        ),
        Some(Duration::from_nanos(1))
    );
    let mut boundary = starts.clone();
    assert_eq!(
        restart_window_remaining(&mut boundary, first + window, window),
        None
    );
    assert_eq!(boundary.len(), 2);
    let mut expired = starts;
    assert_eq!(
        restart_window_remaining(
            &mut expired,
            first + window + Duration::from_secs(2),
            window
        ),
        None
    );
    assert!(expired.is_empty());
}

#[test]
fn outbound_limit_includes_the_sdk_envelope_id_and_json_escaping() {
    let id = agent_client_protocol::schema::v1::RequestId::Str("reserved-sdk-id".into());
    for params in [
        serde_json::json!({}),
        serde_json::json!({"name": "work", "arguments": {"text": "\"\\\n秘密"}}),
    ] {
        let request = McpRequest::new("tools/call", params).unwrap();
        // Compare the reconstructed frame against the SDK's real public wire serializer.
        let wire = agent_client_protocol::RawJsonRpcMessage::request(
            request.0.method.clone(),
            request.0.params.clone(),
            id.clone(),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&wire).unwrap();
        assert_eq!(
            validate_outbound_request(&request.0, &id, bytes.len()),
            Ok(())
        );
        let error = RuntimeError::RequestTooLarge {
            max_frame_bytes: bytes.len() - 1,
        };
        assert_eq!(
            validate_outbound_request(&request.0, &id, bytes.len() - 1),
            Err(error.clone())
        );
        assert!(!error.to_string().contains("秘密"));
    }
}

#[test]
fn runtime_rejects_invalid_policy_and_reserved_methods_without_panicking() {
    let policy = RuntimePolicy {
        max_concurrent_calls: usize::MAX,
        ..Default::default()
    };
    assert!(matches!(
        Runtime::new(NodeId::new(), policy),
        Err(RuntimeError::Inputs)
    ));
    for method in [
        "initialize",
        "notifications/cancelled",
        "resources/subscribe",
        "tasks/get",
    ] {
        assert!(matches!(
            McpRequest::new(method, serde_json::json!({})),
            Err(RuntimeError::Request)
        ));
    }
}
