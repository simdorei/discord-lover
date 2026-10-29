use super::*;
pub(in crate::agy) mod support;
use support::{fixture, message};

#[path = "tests/lifecycle_contract.rs"]
mod lifecycle_contract;

#[test]
fn restart_retains_native_identity_and_never_reexecutes_uncertain_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agy.sqlite");
    let store = Store::open(&path, 10).unwrap();
    store.create_session(100, "test", temp.path()).unwrap();
    assert!(store.claim("message:1", 100).unwrap());
    store.running("message:1", 100).unwrap();
    store.save_conversation(100, "native-conversation").unwrap();
    store.create_session(101, "test", temp.path()).unwrap();
    store.claim("message:2", 101).unwrap();
    store.running("message:2", 101).unwrap();
    drop(store);
    let reopened = Store::open(&path, 10).unwrap();
    assert_eq!(
        reopened
            .session(100)
            .unwrap()
            .unwrap()
            .conversation
            .as_deref(),
        Some("native-conversation")
    );
    assert!(!reopened.session(100).unwrap().unwrap().blocked);
    assert!(reopened.session(101).unwrap().unwrap().blocked);
    assert!(!reopened.claim("message:1", 100).unwrap());
    assert_eq!(reopened.last_result(100).unwrap().unwrap().0, "interrupted");
    assert!(Store::open(&path, 11).is_err());
}

#[test]
fn store_rejects_workspace_retargeting_identity_changes_and_corruption() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agy.sqlite");
    let store = Store::open(&path, 10).unwrap();
    store.bind_project("test", temp.path()).unwrap();
    assert!(store.bind_project("test", Path::new("different")).is_err());
    store.create_session(100, "test", temp.path()).unwrap();
    store.save_conversation(100, "first").unwrap();
    assert!(store.save_conversation(100, "second").is_err());
    store
        .begin_container("category", None, "GuildCategory")
        .unwrap();
    assert_eq!(store.container("category").unwrap(), Some(None));
    assert!(
        store
            .begin_container("category", None, "GuildCategory")
            .is_err()
    );
    drop(store);
    let corrupt = temp.path().join("corrupt.sqlite");
    std::fs::write(&corrupt, b"corrupt database").unwrap();
    assert!(Store::open(&corrupt, 10).is_err());
}

#[test]
fn disabled_configuration_has_no_cli_or_filesystem_dependency() {
    assert!(
        config::AgyConfig::from_environment(
            &std::collections::BTreeMap::new(),
            Path::new("missing")
        )
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn codex_traffic_keeps_the_fast_path_and_wrong_user_or_guild_cannot_submit() {
    let mut f = fixture().await;
    let reads = f.remote.reads.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        !f.service
            .handle_message(&message(1, 90, "codex prompt"), Some(50))
            .await
            .unwrap()
    );
    assert_eq!(
        f.remote.reads.load(std::sync::atomic::Ordering::SeqCst),
        reads,
        "known Codex rooms must not require Discord REST lookup"
    );
    let mut denied = message(2, f.thread, "not authorized");
    support::other_user(&mut denied);
    assert!(!f.service.handle_message(&denied, Some(50)).await.unwrap());
    let mut foreign = message(3, f.thread, "wrong guild");
    foreign.guild_id = Some(twilight_model::id::Id::new(2));
    assert!(!f.service.handle_message(&foreign, Some(50)).await.unwrap());
    assert!(f.receiver.try_recv().is_err());
    assert!(f.messages.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn agy_prompts_are_deduplicated_and_stop_only_cancels_the_agy_conversation() {
    let mut f = fixture().await;
    let prompt = message(1, f.thread, "continue this conversation");
    assert!(f.service.handle_message(&prompt, Some(50)).await.unwrap());
    assert!(f.service.handle_message(&prompt, Some(50)).await.unwrap());
    let job = f.receiver.try_recv().unwrap();
    assert_eq!(job.room, Room::Session(f.thread));
    assert_eq!(
        f.service
            .store
            .session(f.thread)
            .await
            .unwrap()
            .unwrap()
            .conversation
            .as_deref(),
        Some("native-saved-id")
    );
    assert!(f.receiver.try_recv().is_err());
    assert!(!*job.stop.borrow());
    f.service
        .handle_message(&message(2, f.thread, "second prompt"), Some(50))
        .await
        .unwrap();
    assert!(
        !*job.stop.borrow(),
        "a new prompt must not silently cancel the active turn"
    );
    assert!(f.receiver.try_recv().is_err());
    f.service
        .handle_message(&message(3, f.thread, "!stop"), Some(50))
        .await
        .unwrap();
    assert!(*job.stop.borrow());
}

#[tokio::test]
async fn codex_commands_and_unregistered_old_agy_threads_never_start_a_cli() {
    let mut f = fixture().await;
    for (index, content) in [
        "!bridge_sync",
        "!settings --model gpt",
        "!use someone-elses-id",
    ]
    .iter()
    .enumerate()
    {
        assert!(
            f.service
                .handle_message(&message(10 + index as u64, f.thread, content), Some(50))
                .await
                .unwrap()
        );
    }
    let mut old = f.remote.channels.lock().unwrap()[&f.thread].clone();
    old.id = 201;
    f.remote.channels.lock().unwrap().insert(201, old);
    assert!(
        f.service
            .handle_message(&message(20, 201, "resume old AGY"), Some(50))
            .await
            .unwrap()
    );
    assert!(
        f.service.store.session(201).await.unwrap().is_none(),
        "unknown old native conversation must not be replaced"
    );
    assert!(f.receiver.try_recv().is_err());
}

#[tokio::test]
async fn new_in_a_thread_creates_an_independent_job_and_drain_waits_for_it() {
    let mut f = fixture().await;
    f.service
        .handle_message(&message(1, f.thread, "!new fresh task"), Some(50))
        .await
        .unwrap();
    let job = f.receiver.try_recv().unwrap();
    assert!(job.new);
    assert_eq!(job.prompt, "fresh task");
    let key =
        crate::restart_readiness::drain::DrainFenceKey::new("runtime", "12|34", "nonce").unwrap();
    f.service.admission.seal(&key).unwrap();
    assert!(!f.service.admission.is_drained_for(&key));
    f.service
        .handle_message(&message(2, f.project, "blocked during restart"), Some(50))
        .await
        .unwrap();
    assert!(f.receiver.try_recv().is_err());
    f.service
        .handle_message(&message(3, f.thread, "!stop"), Some(50))
        .await
        .unwrap();
    assert!(*job.stop.borrow());
    drop(job);
    assert!(f.service.admission.is_drained_for(&key));
}

#[tokio::test]
async fn overlapping_codex_ownership_and_uncertain_room_creation_are_rejected() {
    let f = fixture().await;
    cdr_store::mapping::upsert_project(
        &f.service.mirror_db,
        "C:/overlap",
        "overlap",
        i64::try_from(f.project).unwrap(),
        1.0,
        |a, b| a == b,
    )
    .unwrap();
    assert!(
        Rooms::prepare(
            &f.service.config,
            &f.service.store,
            &f.service.remote,
            1,
            &f.service.mirror_db,
            &f.service.ownership
        )
        .await
        .is_err()
    );
    let pending = Database::open(
        f.service.store.worker.clone(),
        f.temp.path().join("pending.sqlite"),
        1,
    )
    .await
    .unwrap();
    pending
        .begin_container("category", None, "GuildCategory")
        .await
        .unwrap();
    let before = f.remote.channels.lock().unwrap().len();
    assert!(
        Rooms::prepare(
            &f.service.config,
            &pending,
            &f.service.remote,
            1,
            &f.service.mirror_db,
            &f.service.ownership
        )
        .await
        .is_err()
    );
    assert_eq!(
        f.remote.channels.lock().unwrap().len(),
        before,
        "uncertain creation must not create duplicates"
    );
}

#[tokio::test]
async fn newly_created_thread_is_busy_before_its_first_prompt_can_race() {
    let mut f = fixture().await;
    f.service
        .handle_message(&message(1, f.project, "create a conversation"), Some(50))
        .await
        .unwrap();
    let job = f.receiver.try_recv().unwrap();
    let created = worker::create_thread(&f.service, &job, &f.service.config.projects[0], f.project)
        .await
        .unwrap();
    assert_eq!(f.service.active().unwrap()[&job.id].target, Some(created));
    assert_eq!(
        f.service.rooms.known(created).unwrap(),
        Some(Room::Session(created))
    );
    f.service
        .handle_message(&message(2, created, "another prompt"), Some(50))
        .await
        .unwrap();
    assert!(
        f.receiver.try_recv().is_err(),
        "a second native session must not race the first init event"
    );
    f.service
        .handle_message(&message(3, created, "!stop"), Some(50))
        .await
        .unwrap();
    assert!(*job.stop.borrow());
    assert!(f.service.store.session(created).await.unwrap().is_some());
    assert!(
        !cdr_store::mapping::is_mirrored_channel(
            &f.service.mirror_db,
            Some(i64::try_from(created).unwrap())
        )
        .unwrap()
    );
}
