use std::sync::Arc;

use cdr_runtime::action_executor::{ActionExecutor, ActionUi};
use cdr_runtime::bridge_state::BridgeState;
use cdr_runtime::command_plan::CommandAction;
use cdr_runtime::queue_runner::{BoxBackendFuture, QueueCoordinator, TurnBackend, TurnRecord};
use cdr_store::claims::{BusyChoice, get_busy_choice};
use cdr_store::prompt_intake::list_prompt_intakes;
use cdr_store::queue::list;
use rusqlite::Connection;

#[derive(Default)]
struct BusyBackend {
    reads: std::sync::atomic::AtomicUsize,
    finish_after_first: bool,
}

impl TurnBackend for BusyBackend {
    fn generation(&self) -> u64 {
        1
    }

    fn active_turn_id<'a>(&'a self, _thread_id: &'a str) -> BoxBackendFuture<'a, Option<String>> {
        let call = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(
            async move { Ok((!self.finish_after_first || call == 0).then(|| "turn-live".into())) },
        )
    }

    fn read_turns<'a>(&'a self, _thread_id: &'a str) -> BoxBackendFuture<'a, Vec<TurnRecord>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn resume_thread<'a>(&'a self, _thread_id: &'a str) -> BoxBackendFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn start_turn<'a>(
        &'a self,
        _thread_id: &'a str,
        _prompt: &'a str,
    ) -> BoxBackendFuture<'a, String> {
        Box::pin(async { panic!("a busy target must not start another turn") })
    }
}

#[tokio::test]
async fn human_busy_prompt_creates_durable_controls_without_implicitly_queueing() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state.sqlite");
    Connection::open(&state)
        .unwrap()
        .execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, cwd TEXT, updated_at INTEGER,
             rollout_path TEXT, model TEXT, reasoning_effort TEXT, tokens_used INTEGER,
             archived INTEGER, archived_at INTEGER, source TEXT, thread_source TEXT);
             INSERT INTO threads VALUES ('thread-a','Title','C:/repo',1,'a','m','h',0,0,0,'v','u');",
        )
        .unwrap();
    let mirror = temp.path().join("mirror.sqlite");
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-a")).unwrap();
    let queue = Arc::new(QueueCoordinator::new(
        mirror,
        Arc::new(BusyBackend::default()),
    ));
    let executor = ActionExecutor::new(state, temp.path().join("mirror.sqlite"), bridge, queue);

    let result = executor
        .execute(
            CommandAction::Ask {
                prompt: "choose me".into(),
            },
            10,
            20,
        )
        .await
        .unwrap();
    let ActionUi::Busy {
        choice_id,
        allow_steer,
    } = result.ui.unwrap()
    else {
        panic!("expected busy controls")
    };
    assert!(
        !allow_steer,
        "this queue-only fixture has no resident able to receive a control"
    );
    assert!(list(executor.mirror_db()).unwrap().is_empty());
    let choice = get_busy_choice(executor.mirror_db(), &choice_id, 0.0)
        .unwrap()
        .unwrap();
    assert_eq!(choice.prompt, "choose me");
    assert_eq!(choice.owner_user_id, 20);

    let queued = executor.enqueue_busy_choice(&choice).await.unwrap();
    assert!(
        queued.ui.is_none(),
        "Queue must not display another Busy choice"
    );
    let jobs = list(executor.mirror_db()).unwrap();
    assert_eq!(jobs.len(), 1, "Queue must save the chosen request");
    assert_eq!(jobs[0].prompt, "choose me");
    assert_eq!(jobs[0].target_thread_id, "thread-a");
    assert_eq!(jobs[0].state, cdr_store::queue::QueueJobState::Pending);
    assert!(
        list_prompt_intakes(executor.mirror_db())
            .unwrap()
            .is_empty()
    );
    assert_repeat_does_not_reenqueue(&executor, &choice).await;
}

async fn assert_repeat_does_not_reenqueue(
    executor: &ActionExecutor<BusyBackend>,
    choice: &BusyChoice,
) {
    executor.enqueue_busy_choice(choice).await.unwrap();
    let jobs = list(executor.mirror_db()).unwrap();
    assert_eq!(
        jobs.len(),
        1,
        "repeat click must retain the same occurrence"
    );
    // Model completion removing all transient queue/outbox state. The receipt
    // must still prevent a late repeated button from creating another turn.
    Connection::open(executor.mirror_db())
        .unwrap()
        .execute("DELETE FROM codex_turn_queue", [])
        .unwrap();
    executor.enqueue_busy_choice(choice).await.unwrap();
    assert!(list(executor.mirror_db()).unwrap().is_empty());
    assert!(
        list_prompt_intakes(executor.mirror_db())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn busy_button_does_not_claim_steer_now_after_original_turn_disappears() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("mirror.sqlite");
    let bridge = Arc::new(BridgeState::new(temp.path().join("bridge.json")));
    bridge.set_selected_thread_id(Some("thread-a")).unwrap();
    let queue = Arc::new(QueueCoordinator::new(
        db.clone(),
        Arc::new(BusyBackend {
            finish_after_first: true,
            ..Default::default()
        }),
    ));
    let executor = ActionExecutor::new(temp.path().join("state.sqlite"), db, bridge, queue);
    let result = executor
        .execute(
            CommandAction::Ask {
                prompt: "new direction".into(),
            },
            10,
            20,
        )
        .await
        .unwrap();
    let Some(ActionUi::PendingBusy { ref choice_id }) = result.ui else {
        panic!("a disappeared turn must have pending-only controls")
    };
    assert!(
        !get_busy_choice(executor.mirror_db(), choice_id, 0.0)
            .unwrap()
            .unwrap()
            .allow_steer
    );
    assert_eq!(
        cdr_store::control_binding::resolve(executor.mirror_db(), choice_id, "thread-a").unwrap(),
        None
    );
    let rendered =
        serde_json::to_value(cdr_runtime::action_ui::render_action_ui(result.ui.as_ref()).unwrap())
            .unwrap();
    let ids: Vec<_> = rendered[0]["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|button| button["custom_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            format!("codex_busy:{choice_id}:queue"),
            format!("codex_busy:{choice_id}:ignore")
        ],
        "no Steer or Stop may target the vanished turn"
    );
}
