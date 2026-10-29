use super::*;
use crate::agy::{
    config::{AgyConfig, Project},
    manager::tests::resources,
};

pub(super) struct SettingsReadBarrier {
    pub read: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

fn fixture_config(root: &Path, name: &str) -> AgyConfig {
    AgyConfig {
        executable: std::env::current_exe().unwrap(),
        projects: vec![Project {
            name: name.into(),
            cwd: root.into(),
            channel_id: None,
        }],
        category_id: None,
        model: None,
        auto_approve: false,
    }
}

#[tokio::test]
#[ignore = "isolated subprocess fixture, launched by forged_pid_is_rejected_before_payload"]
async fn forged_pipe_child() {
    let Some(root) = std::env::var_os("CDR_AGY_FAKE_PIPE_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mut pipe = native::pipe(&pipe_name(&root), true).unwrap();
    std::fs::write(root.join("child-ready"), b"ready").unwrap();
    pipe.connect().await.unwrap();
    let mut byte = [0];
    let result = tokio::time::timeout(Duration::from_secs(3), pipe.read(&mut byte))
        .await
        .unwrap();
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "client transmitted to forged server"
    );
    std::fs::write(root.join("no-payload"), b"verified").unwrap();
}

#[tokio::test]
async fn forged_pid_is_rejected_before_payload() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let own = native::identity(std::process::id()).unwrap();
    let forged = Binding {
        protocol: PROTOCOL,
        root: root.clone(),
        runtime_uuid: uuid::Uuid::new_v4().to_string(),
        pid: own.pid,
        creation: own.creation,
    };
    std::fs::write(root.join(MARKER), serde_json::to_vec(&forged).unwrap()).unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agy::control::tests::forged_pipe_child",
            "--ignored",
            "--nocapture",
        ])
        .env("CDR_AGY_FAKE_PIPE_ROOT", &root)
        .creation_flags(0x0800_0000)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.join("child-ready").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let error = client(&root, Operation::Status).await.unwrap_err();
    assert!(
        error.to_string().contains("OS identity mismatch"),
        "{error}"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert_eq!(std::fs::read(root.join("no-payload")).unwrap(), b"verified");
}

#[tokio::test]
async fn old_settings_with_new_status_revision_cannot_overwrite_second_tray() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let (stop, rx) = watch::channel(false);
    let managed = AgyManager::start(resources(&root), rx).await;
    managed
        .handle
        .command(Command::Configure {
            expected_revision: 0,
            config: fixture_config(&root, "first"),
        })
        .await
        .unwrap();
    let own = native::identity(std::process::id()).unwrap();
    let binding = Binding {
        protocol: PROTOCOL,
        root: root.clone(),
        runtime_uuid: managed.handle.status().runtime_uuid,
        pid: own.pid,
        creation: own.creation,
    };
    std::fs::write(root.join(MARKER), serde_json::to_vec(&binding).unwrap()).unwrap();
    let barrier = Arc::new(SettingsReadBarrier {
        read: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let (mutations, _) = mpsc::channel(1);
    let endpoint = Endpoint {
        binding,
        own,
        manager: Arc::clone(&managed.handle),
        ledger: Arc::new(Mutex::new(Ledger::default())),
        mutations,
        settings_read: Some(Arc::clone(&barrier)),
    };
    let pipe = native::pipe(&pipe_name(&root), true).unwrap();
    let server = tokio::spawn(async move {
        pipe.connect().await.unwrap();
        endpoint.connection(pipe).await.unwrap();
    });
    let requesting = client(&root, Operation::Settings);
    let concurrent = async {
        barrier.read.notified().await;
        managed
            .handle
            .command(Command::Configure {
                expected_revision: 1,
                config: fixture_config(&root, "second"),
            })
            .await
            .unwrap();
        barrier.release.notify_one();
    };
    let (response, ()) = tokio::join!(requesting, concurrent);
    let response = response.unwrap();
    server.await.unwrap();
    let displayed = response.settings.unwrap();
    assert_eq!(displayed.revision, 1);
    assert_eq!(response.status.config_revision, 2);
    assert!(
        managed
            .handle
            .command(Command::Configure {
                expected_revision: displayed.revision,
                config: displayed.config
            })
            .await
            .is_err()
    );
    let saved = managed.handle.settings().unwrap().unwrap();
    assert_eq!(saved.revision, 2);
    assert_eq!(saved.config.projects[0].name, "second");
    stop.send(true).unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
}
#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "one cohesive lifecycle or contract scenario preserves ordering visibly"
)]
async fn real_windows_pipe_identity_replay_oversize_disconnect_and_runtime_change() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (stop, shutdown) = watch::channel(false);
    let managed = AgyManager::start(resources(root), shutdown.clone()).await;
    let control = Control::start(Arc::clone(&managed.handle), root.into(), shutdown);
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.join(MARKER).exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let status = client(root, Operation::Status).await.unwrap();
    assert_eq!(status.status.phase, super::super::lifecycle::Phase::Off);
    let id = format!(
        "{}/{}/{}",
        status.status.runtime_uuid,
        native::monotonic_millis(),
        uuid::Uuid::new_v4()
    );
    let command = Command::Configure {
        expected_revision: 0,
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
    };
    client(
        root,
        Operation::Mutate {
            id: id.clone(),
            command: command.clone(),
        },
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let response = client(root, Operation::Result { id: id.clone() })
                .await
                .unwrap();
            if response.outcome == "complete" {
                break response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(result.error.is_none());
    assert_eq!(result.status.config_revision, 1);
    let replay = client(
        root,
        Operation::Mutate {
            id: id.clone(),
            command,
        },
    )
    .await
    .unwrap();
    assert_eq!(replay.outcome, "complete");
    assert_eq!(replay.status.config_revision, 1);
    let conflict = client(
        root,
        Operation::Mutate {
            id,
            command: Command::On {
                expected_revision: 1,
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(conflict.outcome, "rejected");
    let mut pipe = ClientOptions::new()
        .open(pipe_name(&root.canonicalize().unwrap()))
        .unwrap();
    pipe.write_u32_le(u32::try_from(MAX_FRAME + 1).unwrap())
        .await
        .unwrap();
    assert!(read_frame::<Response>(&mut pipe).await.is_err());
    drop(pipe);
    assert_eq!(
        client(root, Operation::Status)
            .await
            .unwrap()
            .status
            .config_revision,
        1
    );
    let original = std::fs::read(root.join(MARKER)).unwrap();
    let mut fake: Binding = serde_json::from_slice(&original).unwrap();
    fake.creation += 1;
    std::fs::write(root.join(MARKER), serde_json::to_vec(&fake).unwrap()).unwrap();
    assert!(client(root, Operation::Status).await.is_err());
    std::fs::write(root.join(MARKER), original).unwrap();
    let old_id = format!(
        "previous-runtime/{}/{}",
        native::monotonic_millis(),
        uuid::Uuid::new_v4()
    );
    assert_eq!(
        client(
            root,
            Operation::Mutate {
                id: old_id,
                command: Command::On {
                    expected_revision: 1
                }
            }
        )
        .await
        .unwrap()
        .outcome,
        "rejected"
    );
    stop.send(true).unwrap();
    control
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
    managed
        .join(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .unwrap();
}
