use super::*;

#[test]
fn saves_identity_before_finishing_and_keeps_resumed_conversation_argument() {
    let mut output = Output::default();
    let mut ids = Vec::new();
    let mut persist = |id: &str| {
        ids.push(id.to_owned());
        Ok(())
    };
    output
        .consume(
            r#"{"event":"init","conversation_id":"native-123"}"#,
            &mut persist,
        )
        .unwrap();
    output
        .consume(
            r#"{"event":"step_update","step_update":{"text_delta":"답변"}}"#,
            &mut persist,
        )
        .unwrap();
    output.consume(r#"{"event":"result","result":{"conversation_id":"native-123","status":"SUCCESS","response":"최종 답변"}}"#,&mut persist).unwrap();
    assert_eq!(output.finish(0, "").unwrap(), "최종 답변");
    assert_eq!(ids, ["native-123"]);
    let config = AgyConfig {
        executable: "agy.exe".into(),
        projects: vec![],
        category_id: None,
        model: None,
        auto_approve: false,
    };
    let args = arguments(
        &config,
        "한글 질문 & quoted \"text\"",
        Some("native-123"),
        Path::new("C:/workspace with spaces"),
    );
    assert_eq!(args[1], "한글 질문 & quoted \"text\"");
    assert_eq!(&args[6..], ["--conversation", "native-123"]);
    assert!(
        !args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions")
    );
}

#[test]
fn partial_text_nonzero_exit_missing_result_and_failed_status_are_not_success() {
    for (final_line, code) in [
        (None, 0),
        (
            Some(r#"{"event":"result","result":{"status":"SUCCESS","response":"partial"}}"#),
            3,
        ),
        (
            Some(r#"{"event":"result","result":{"status":"ERROR","response":"partial"}}"#),
            0,
        ),
        (
            Some(r#"{"event":"result","result":{"response":"partial"}}"#),
            0,
        ),
    ] {
        let mut output = Output::default();
        output
            .consume(
                r#"{"event":"init","conversation_id":"id"}"#,
                &mut |_| Ok(()),
            )
            .unwrap();
        output
            .consume(
                r#"{"event":"step_update","step_update":{"text_delta":"partial"}}"#,
                &mut |_| Ok(()),
            )
            .unwrap();
        if let Some(line) = final_line {
            output.consume(line, &mut |_| Ok(())).unwrap();
        }
        assert!(output.finish(code, "error details").is_err());
    }
}

#[test]
fn malformed_events_identity_changes_and_failed_persistence_stop_processing() {
    let mut output = Output::default();
    assert!(output.consume("not json", &mut |_| Ok(())).is_err());
    assert!(
        output
            .consume(r#"{"event":"init","conversation_id":"id"}"#, &mut |_| Err(
                AgyError::Invalid("disk full".into())
            ))
            .is_err()
    );
    assert!(output.conversation.is_none());
    output
        .consume(
            r#"{"event":"init","conversation_id":"id"}"#,
            &mut |_| Ok(()),
        )
        .unwrap();
    assert!(
        output
            .consume(
                r#"{"event":"step_update","step_update":{"conversation_id":"different"}}"#,
                &mut |_| Ok(())
            )
            .is_err()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn native_runner_reports_nonzero_exit_and_cancels_without_waiting_for_the_cli() {
    use std::time::Duration;
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("fake-cli.ps1");
    let exe = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let init = r#"[Console]::WriteLine('{"event":"init","conversation_id":"stub-id"}')"#;
    let final_event = r#"[Console]::WriteLine('{"event":"result","result":{"status":"SUCCESS","response":"looks successful"}}')"#;
    std::fs::write(&script, format!("{init}\n{final_event}\nexit 7\n")).unwrap();
    let args = vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-File".into(),
        script.to_string_lossy().into_owned(),
    ];
    let (_sender, stop) = watch::channel(false);
    let error = execute_process(&exe, &args, temp.path(), stop, |_| Ok(()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("code 7"), "{error}");

    std::fs::write(
        &script,
        format!("{init}\nStart-Sleep -Seconds 30\n{final_event}\n"),
    )
    .unwrap();
    let (stop_sender, stop) = watch::channel(false);
    let (init_sender, init_receiver) = tokio::sync::oneshot::channel();
    let mut init_sender = Some(init_sender);
    let task = tokio::spawn({
        let cwd = temp.path().to_path_buf();
        async move {
            execute_process(&exe, &args, &cwd, stop, |_| {
                if let Some(sender) = init_sender.take() {
                    let _ = sender.send(());
                }
                Ok(())
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), init_receiver)
        .await
        .unwrap()
        .unwrap();
    stop_sender.send(true).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, Err(AgyError::Stopped)));
}
