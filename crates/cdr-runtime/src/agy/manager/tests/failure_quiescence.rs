use super::*;
use crate::agy::{
    Room,
    tests::support::{RemoteBarrier, fixture},
};

#[cfg(windows)]
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one ordered recovery or race scenario keeps custody boundaries visible"
)]
async fn failed_result_and_sibling_panic_join_process_db_and_handlers_before_recovery() {
    let f = fixture().await;
    let root = f.temp.path();
    let mut r = resources(root);
    r.remote = Some(f.remote.clone());
    settings(root, false).write(&r.settings_path).unwrap();
    let writer = Configuration::acquire(&r.settings_path).unwrap();
    let (handle, status, failures) = routing_handle(
        Some(Arc::clone(&f.service)),
        Arc::clone(&f.service.ownership),
        Phase::Ready,
    );
    let lease = handle.lease().unwrap();
    let barrier = Arc::new(RemoteBarrier::default());
    *f.remote.panic_barrier.lock().unwrap() = Some(Arc::clone(&barrier));
    let path = root.join(".codex_discord_agy.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_result BEFORE UPDATE OF state ON requests WHEN OLD.id='message:102' AND NEW.state='failed' BEGIN SELECT RAISE(FAIL,'fixture result persistence failure'); END;").unwrap();
    // An owned harmless child represents a sibling CLI tree. No AGY executable,
    // account, or external service is invoked by this failure fixture.
    let exe = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let (out, err) = f
        .service
        .processes
        .launch(
            "message:101",
            &exe,
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Start-Sleep -Seconds 30".into(),
            ],
            root,
        )
        .unwrap();
    drop((out, err));
    f.service
        .submit(
            "message:101",
            f.thread,
            Room::Session(f.thread),
            "sibling B".into(),
            false,
        )
        .await
        .unwrap();
    let (worker_stop, worker_rx) = watch::channel(false);
    let service = Arc::clone(&f.service);
    let task = tokio::spawn(crate::agy::worker::run(service, f.receiver, worker_rx));
    tokio::time::timeout(Duration::from_secs(2), barrier.entered.notified())
        .await
        .unwrap();
    f.service
        .submit(
            "message:102",
            300,
            Room::Unsupported,
            "result A".into(),
            false,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.service.acceptance.lock().await.open {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "worker must join B after A result failure"
    );
    let (_, commands) = mpsc::channel(1);
    let mut manager = Manager {
        resources: r,
        handle,
        status,
        view: Status::new("fixture".into()),
        commands,
        failures,
        writer: Some(writer),
        activation: None,
        worker: Some(Worker {
            stop: worker_stop,
            task,
        }),
        probe: Arc::new(HeldProbe {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(1)),
            verified: true,
        }),
        login: login::Login::default(),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        provisioning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cleaning: None,
        cleanup_started: None,
    };
    manager.view.phase = Phase::FaultedCleaning;
    manager.cache(settings(root, false));
    manager.publish();
    assert!(
        manager
            .apply(Command::On {
                expected_revision: 1
            })
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row(
            "SELECT state FROM requests WHERE id='message:102'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "accepted"
    );
    barrier.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.worker.is_some() {
            manager.advance().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.service.processes.count(), 0);
    assert!(f.service.active().unwrap().is_empty());
    assert_eq!(f.service.handlers.load(Ordering::SeqCst), 1);
    assert!(!manager.quiescent());
    assert!(
        manager
            .apply(Command::On {
                expected_revision: 1
            })
            .await
            .is_err()
    );
    let (release_db, held_db) = std::sync::mpsc::channel();
    let (entered, started) = oneshot::channel();
    let worker = f.service.store.worker.clone();
    let storage = tokio::spawn(async move {
        worker
            .call_with_budget(Duration::from_secs(2), move || {
                entered.send(()).unwrap();
                held_db.recv().unwrap();
                Ok(())
            })
            .await
    });
    started.await.unwrap();
    drop(lease);
    assert!(!manager.quiescent());
    assert!(
        manager
            .apply(Command::On {
                expected_revision: 1
            })
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM requests WHERE state='interrupted'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0,
        "no Store::open recovery while any old owner remains"
    );
    release_db.send(()).unwrap();
    storage.await.unwrap().unwrap();
    manager.advance().await;
    assert!(manager.quiescent());
    let key = crate::restart_readiness::drain::DrainFenceKey::new("fixture", "1|2", "quiescence")
        .unwrap();
    f.service.admission.seal(&key).unwrap();
    assert!(f.service.admission.is_drained_for(&key));
    *f.remote.panic_barrier.lock().unwrap() = None;
    manager
        .apply(Command::On {
            expected_revision: 1,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while manager.view.phase != Phase::Ready {
            manager.advance().await;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM requests WHERE state='interrupted'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(manager.handle.service().unwrap().quiescent());
    manager.shutdown_cleanup().await.unwrap();
}
