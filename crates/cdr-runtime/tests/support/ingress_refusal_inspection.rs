use super::*;

#[tokio::test]
async fn unsent_answer_remains_distinct_from_completed_execution_in_runners() {
    let (_temp, executor, backend) = fixture();
    let db = executor.mirror_db();
    let key = "interaction:501";
    admit(
        db,
        &NewIngress {
            ingress_id: key.into(),
            kind: IngressKind::Interaction,
            event_id: Some(501),
            application_id: Some(1),
            channel_id: 99,
            owner_user_id: 20,
            source_message_id: Some(1234),
            payload: json!({"answer":"private original answer"}),
            target_thread_id: Some("saved-target".into()),
            canonical_owner: None,
            now: 1.0,
        },
    )
    .unwrap();
    let outcome = cdr_store::ingress::AsyncAnswerRefusal {
        thread_id: "saved-target".into(),
        question_id: "a".repeat(64),
        reason: "read timed out before sending".into(),
    }
    .outcome();
    cdr_store::ingress::record_result(db, key, &outcome, 2.0).unwrap();
    for held in [false, true] {
        if held {
            hold(db, key, "notice confirmation failed", false, 3.0).unwrap();
        }
        let before = get(db, key).unwrap().unwrap();
        let list = executor
            .execute(CommandAction::Runners, 99, 20)
            .await
            .unwrap()
            .text;
        assert!(list.contains("answer not sent; notification unconfirmed"));
        assert!(list.contains("saved-target"));
        assert!(list.contains("read timed out before sending"));
        assert!(!list.contains("execution completed; confirmation pending"));
        assert!(!list.contains("private original answer"));
        let detail = executor
            .execute(detail_action(key), 99, 20)
            .await
            .unwrap()
            .text;
        assert!(detail.contains("known_outcome: 답변을 전송하지 않았습니다"));
        assert!(detail.contains("notification_confirmed: false"));
        assert_eq!(display_payload(&detail), before.payload);
        assert_eq!(get(db, key).unwrap().unwrap(), before);
        assert!(executor.execute(detail_action(key), 99, 21).await.is_err());
        assert!(executor.execute(detail_action(key), 100, 20).await.is_err());
    }
    assert!(backend.starts.lock().await.is_empty());
    assert!(cdr_store::delivery::list_pending(db).unwrap().is_empty());
}

#[tokio::test]
async fn mc_9_runners_distinguishes_known_refusal_from_notification_status() {
    let (_temp, executor, backend) = fixture();
    let db = executor.mirror_db();
    let key = "action:refused";
    admit(
        db,
        &NewIngress {
            ingress_id: key.into(),
            kind: IngressKind::Action,
            event_id: None,
            application_id: None,
            channel_id: 99,
            owner_user_id: 20,
            source_message_id: None,
            payload: json!({"content":"!mirror sync"}),
            target_thread_id: Some("saved-target".into()),
            canonical_owner: None,
            now: 1.0,
        },
    )
    .unwrap();
    let outcome = cdr_store::ingress::CleanupRefusal {
        room: 31,
        reason: "ingress".into(),
    }
    .outcome();
    cdr_store::ingress::record_result(db, key, &outcome, 2.0).unwrap();
    for held in [false, true] {
        if held {
            hold(
                db,
                key,
                "notification delivery confirmed; ingress confirmation write failed",
                false,
                3.0,
            )
            .unwrap();
        }
        let before = get(db, key).unwrap().unwrap();
        let list = executor
            .execute(CommandAction::Runners, 99, 20)
            .await
            .unwrap()
            .text;
        assert!(list.contains("sync stopped; notification unconfirmed"));
        assert!(list.contains("room 31"));
        assert!(!list.contains("execution completed; confirmation pending"));
        let detail = executor
            .execute(detail_action(key), 99, 20)
            .await
            .unwrap()
            .text;
        assert!(detail.contains("known_outcome: Mirror sync stopped"));
        assert!(detail.contains("notification_confirmed: false"));
        if held {
            assert!(detail.contains("delivery confirmed; ingress confirmation write failed"));
        }
        assert_eq!(display_payload(&detail), before.payload);
        assert_eq!(get(db, key).unwrap().unwrap(), before);
        assert!(executor.execute(detail_action(key), 99, 21).await.is_err());
    }
    assert!(backend.starts.lock().await.is_empty());
}
