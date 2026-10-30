use super::*;

pub(super) async fn create_while_claimed(
    fixture: &MessageFixture,
    lock: tokio::sync::OwnedMutexGuard<()>,
) {
    let db = fixture.executor.mirror_db();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let claimed: bool = cdr_store::schema::open_initialized(db)
                .unwrap()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM busy_choices WHERE claimed_at IS NOT NULL)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if claimed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cdr_store::mapping::upsert_thread(db, "thread-a", "project", "other", 100, 43, 1.0).unwrap();
    cdr_store::queue::enqueue(
        db,
        cdr_store::queue::NewQueueJob {
            job_id: "other-job",
            target_thread_id: "thread-a",
            channel_id: 43,
            owner_user_id: Some(3),
            discord_message_id: None,
            app_server_generation: i64::try_from(fixture.server.generation()).unwrap(),
            prompt: "other",
            queued: false,
            ack_sent: true,
            created_at: 1.0,
        },
    )
    .unwrap();
    let result = fixture
        .executor
        .execute(
            crate::command_plan::CommandAction::Ask {
                prompt: "other busy request".into(),
            },
            43,
            3,
        )
        .await
        .unwrap();
    assert!(matches!(
        result.ui,
        Some(crate::action_executor::ActionUi::PendingBusy { .. })
    ));
    let retained: bool = cdr_store::schema::open_initialized(db).unwrap().query_row(
        "SELECT EXISTS(SELECT 1 FROM codex_busy_control_bindings b JOIN busy_choices c USING(choice_id)
         WHERE b.thread_id='thread-b' AND b.job_id='preparing' AND b.turn_id IS NULL AND c.claimed_at IS NOT NULL)",
        [], |row| row.get(0),
    ).unwrap();
    assert!(
        retained,
        "unrelated cleanup must preserve the original claimed job binding"
    );
    drop(lock);
}
