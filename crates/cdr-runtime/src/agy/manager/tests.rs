use super::*;
use crate::agy::{config::Project, tests::support::Remote};
#[cfg(windows)]
mod failure_quiescence;

#[tokio::test]
async fn five_second_cold_storage_stall_does_not_hold_codex_startup() {
    let baseline = tempfile::tempdir().unwrap();
    let (stop, rx) = watch::channel(false);
    let at = tokio::time::Instant::now();
    let first = AgyManager::start(resources(baseline.path()), rx).await;
    let baseline_elapsed = at.elapsed();
    stop.send(true).unwrap();
    first
        .join(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    let held = tempfile::tempdir().unwrap();
    let owner = AgyOwnership::for_root(held.path(), 1);
    let worker = owner.worker().unwrap();
    let (entered, started) = oneshot::channel();
    let blocker = tokio::spawn(async move {
        worker
            .call_with_budget(Duration::from_secs(6), move || {
                let _ = entered.send(());
                std::thread::sleep(Duration::from_secs(5));
                Ok(())
            })
            .await
    });
    started.await.unwrap();
    let (stop, rx) = watch::channel(false);
    let at = tokio::time::Instant::now();
    let managed = AgyManager::start(resources(held.path()), rx).await;
    let delta = at.elapsed().saturating_sub(baseline_elapsed);
    assert!(
        delta <= Duration::from_millis(100),
        "startup delta {delta:?}"
    );
    assert!(!blocker.is_finished());
    assert_eq!(
        tokio::spawn(async { "Codex ready" }).await.unwrap(),
        "Codex ready"
    );
    blocker.await.unwrap().unwrap();
    stop.send(true).unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}
use std::sync::atomic::{AtomicUsize, Ordering};
pub(crate) fn resources(root: &std::path::Path) -> Resources {
    let policy = RuntimeConfig::from_map(
        &std::collections::BTreeMap::from([
            ("DISCORD_BOT_TOKEN".into(), "fixture".into()),
            ("DISCORD_GUILD_ID".into(), "1".into()),
            ("DISCORD_ALLOW_ALL_CHANNELS".into(), "1".into()),
            ("DISCORD_ALLOWED_USER_IDS".into(), "7".into()),
        ]),
        crate::config::CliOptions::default(),
    )
    .unwrap();
    Resources {
        root: root.into(),
        mirror_db: root.join("mirror.sqlite"),
        settings_path: root.join("agy.json"),
        policy: Arc::new(policy),
        http: Arc::new(Client::new("fixture".into())),
        admission: AdmissionGate::new(),
        runtime_uuid: uuid::Uuid::new_v4().to_string(),
        topology: GatewayTopology::default(),
        remote: Some(Arc::new(Remote::default())),
    }
}
fn settings(root: &std::path::Path, enabled: bool) -> Settings {
    Settings {
        version: 1,
        revision: 1,
        enabled,
        config: AgyConfig {
            executable: std::env::current_exe().unwrap(),
            projects: vec![Project {
                name: "test".into(),
                cwd: root.into(),
                channel_id: None,
            }],
            category_id: None,
            model: None,
            auto_approve: false,
        },
    }
}

#[cfg(windows)]
#[tokio::test]
async fn two_runtime_roots_resolving_one_junction_settings_target_have_one_writer() {
    use std::os::windows::process::CommandExt;
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("runtime-a");
    let b = temp.path().join("runtime-b");
    let shared = temp.path().join("shared");
    let alias = temp.path().join("alias");
    for path in [&a, &b, &shared] {
        std::fs::create_dir(path).unwrap();
    }
    let output=std::process::Command::new("powershell.exe").args(["-NoProfile","-NonInteractive","-Command","New-Item -ItemType Junction -Path $env:CDR_TEST_ALIAS -Target $env:CDR_TEST_TARGET -ErrorAction Stop | Out-Null"])
        .env("CDR_TEST_ALIAS",&alias).env("CDR_TEST_TARGET",&shared).creation_flags(0x0800_0000).output().unwrap();
    assert!(output.status.success());
    settings(&a, false).write(&shared.join("agy.json")).unwrap();
    let mut ra = resources(&a);
    ra.settings_path = shared.join("agy.json");
    let mut rb = resources(&b);
    rb.settings_path = alias.join("agy.json");
    let (stop_a, rx_a) = watch::channel(false);
    let first = AgyManager::start(ra, rx_a).await;
    until(&first.handle, Phase::Off).await;
    let (stop_b, rx_b) = watch::channel(false);
    let second = AgyManager::start(rb, rx_b).await;
    until(&second.handle, Phase::SetupRequired).await;
    assert!(
        second
            .handle
            .command(Command::Configure {
                expected_revision: 1,
                config: settings(&b, false).config.clone()
            })
            .await
            .is_err()
    );
    assert_eq!(
        Settings::read(&shared.join("agy.json"))
            .unwrap()
            .config
            .projects[0]
            .cwd,
        a
    );
    stop_a.send(true).unwrap();
    first
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    second
        .handle
        .command(Command::Configure {
            expected_revision: 1,
            config: settings(&b, false).config,
        })
        .await
        .unwrap();
    let saved = Settings::read(&shared.join("agy.json")).unwrap();
    assert_eq!(saved.revision, 2);
    assert_eq!(saved.config.projects[0].cwd, b);
    stop_b.send(true).unwrap();
    second
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    std::fs::remove_dir(&alias).unwrap();
}
pub(crate) async fn until(handle: &AgyManager, phase: Phase) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while handle.status().phase != phase {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {phase:?}, got {:?}", handle.status()));
}
pub(in crate::agy) fn routing_handle(
    service: Option<Arc<AgyRuntime>>,
    ownership: Arc<AgyOwnership>,
    phase: Phase,
) -> (
    Arc<AgyManager>,
    watch::Sender<Status>,
    mpsc::Receiver<String>,
) {
    let mut view = Status::new("routing-fixture".into());
    view.phase = phase;
    view.generation = 1;
    let (status, status_rx) = watch::channel(view);
    let (commands, _) = mpsc::channel(32);
    let (failures, failure_rx) = mpsc::channel(8);
    (
        Arc::new(AgyManager {
            ownership,
            status: status_rx,
            service: Arc::new(RwLock::new(service)),
            commands,
            failures,
            settings: Arc::new(RwLock::new(None)),
        }),
        status,
        failure_rx,
    )
}
struct HeldProbe {
    calls: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
    verified: bool,
}

#[tokio::test]
async fn delayed_create_after_off_retains_receipt_without_ready_or_more_creation() {
    let temp = tempfile::tempdir().unwrap();
    let mut r = resources(temp.path());
    settings(temp.path(), false)
        .write(&r.settings_path)
        .unwrap();
    let remote = Arc::new(Remote::default());
    let barrier = Arc::new(crate::agy::tests::support::RemoteBarrier::default());
    *remote.create_barrier.lock().unwrap() = Some(Arc::clone(&barrier));
    r.remote = Some(remote.clone());
    let (stop, rx) = watch::channel(false);
    let managed = AgyManager::start_with_probe(
        r,
        rx,
        Arc::new(HeldProbe {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(1)),
            verified: true,
        }),
    )
    .await;
    managed
        .handle
        .command(Command::On {
            expected_revision: 1,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), barrier.entered.notified())
        .await
        .unwrap();
    managed
        .handle
        .command(Command::Off {
            expected_revision: 2,
            immediate: false,
        })
        .await
        .unwrap();
    assert_ne!(managed.handle.status().phase, Phase::Ready);
    barrier.release.notify_one();
    until(&managed.handle, Phase::Off).await;
    assert_eq!(remote.creates.load(Ordering::SeqCst), 1);
    let path = temp.path().join(".codex_discord_agy.sqlite");
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let receipt: String = db
        .query_row(
            "SELECT channel FROM containers WHERE key='category'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let id = receipt.parse::<u64>().unwrap();
    assert_eq!(
        managed.handle.ownership.classify(id),
        crate::agy::ownership::Owner::Agy
    );
    assert!(managed.handle.service().is_none());
    assert!(!managed.handle.status().enabled);
    stop.send(true).unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
}
impl Probe for HeldProbe {
    fn check<'a>(&'a self, _: &'a AgyConfig) -> ProbeFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let permit = self.release.acquire().await.unwrap();
            permit.forget();
            Ok(if self.verified {
                activation::Evidence::Verified { cutoff: 100 }
            } else {
                activation::Evidence::Unverified("test gate".into())
            })
        })
    }
}
#[tokio::test]
async fn off_is_lazy_and_late_verified_probe_cannot_create_rooms_or_reenable() {
    let temp = tempfile::tempdir().unwrap();
    let r = resources(temp.path());
    settings(temp.path(), false)
        .write(&r.settings_path)
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let probe = Arc::new(HeldProbe {
        calls: Arc::clone(&calls),
        release: Arc::clone(&release),
        verified: true,
    });
    let (stop, shutdown) = watch::channel(false);
    let managed = AgyManager::start_with_probe(r, shutdown, probe).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(managed.handle.status().phase, Phase::Off);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    managed
        .handle
        .command(Command::On {
            expected_revision: 1,
        })
        .await
        .unwrap();
    while calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    managed
        .handle
        .command(Command::On {
            expected_revision: 2,
        })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    managed
        .handle
        .command(Command::Off {
            expected_revision: 2,
            immediate: false,
        })
        .await
        .unwrap();
    assert!(
        managed
            .handle
            .command(Command::On {
                expected_revision: 3
            })
            .await
            .is_err()
    );
    release.add_permits(1);
    until(&managed.handle, Phase::Off).await;
    assert!(!temp.path().join(".codex_discord_agy.sqlite").exists());
    assert!(
        !Settings::read(&temp.path().join("agy.json"))
            .unwrap()
            .enabled
    );
    stop.send(true).unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
}
#[tokio::test]
async fn real_manager_ready_off_recheck_and_corrupt_settings_remain_optional() {
    let temp = tempfile::tempdir().unwrap();
    let r = resources(temp.path());
    settings(temp.path(), false)
        .write(&r.settings_path)
        .unwrap();
    let (stop, shutdown) = watch::channel(false);
    let managed = AgyManager::start_with_probe(
        r,
        shutdown,
        Arc::new(HeldProbe {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(1)),
            verified: true,
        }),
    )
    .await;
    managed
        .handle
        .command(Command::On {
            expected_revision: 1,
        })
        .await
        .unwrap();
    until(&managed.handle, Phase::Ready).await;
    assert!(
        managed
            .handle
            .command(Command::Off {
                expected_revision: 1,
                immediate: true
            })
            .await
            .is_err()
    );
    assert_eq!(
        managed.handle.status().phase,
        Phase::Ready,
        "stale Off must have no side effects"
    );
    assert!(managed.handle.status().enabled);
    assert!(
        managed
            .handle
            .service()
            .unwrap()
            .acceptance
            .lock()
            .await
            .open
    );
    managed
        .handle
        .command(Command::Off {
            expected_revision: 2,
            immediate: false,
        })
        .await
        .unwrap();
    until(&managed.handle, Phase::Off).await;
    assert!(managed.handle.service().is_none());
    assert!(managed.handle.ownership.known_ids().len() >= 2);
    stop.send(true).unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    let other = tempfile::tempdir().unwrap();
    let r = resources(other.path());
    std::fs::write(&r.settings_path, b"broken").unwrap();
    let (stop, shutdown) = watch::channel(false);
    let managed = AgyManager::start(r, shutdown).await;
    until(&managed.handle, Phase::SetupRequired).await;
    managed
        .handle
        .command(Command::Configure {
            expected_revision: 0,
            config: settings(other.path(), false).config,
        })
        .await
        .unwrap();
    managed
        .handle
        .command(Command::On {
            expected_revision: 1,
        })
        .await
        .unwrap();
    until(&managed.handle, Phase::VerificationRequired).await;
    assert!(!other.path().join(".codex_discord_agy.sqlite").exists());
    stop.send(true).unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
}
