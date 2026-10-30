//! This module performs only local claims and read-only RPCs; it never sends an answer.
use super::{ComponentWorkerError, DispatchMode, Question, ResidentAppServer, invalid, store};
use cdr_app_server::{outcomes::TurnStatus, requests::get_goal};
use std::{path::Path, time::Duration};
use tokio::time::Instant;

pub(super) async fn prepare(
    db: &Path,
    server: &ResidentAppServer,
    q: &Question,
    answer: store::Answer,
    message: &str,
    prompt: &str,
) -> Result<DispatchMode, ComponentWorkerError> {
    let prepared = preflight(server, q).await?;
    store::begin_dispatch(
        db,
        &store::Claim {
            id: &q.id,
            runtime_id: server.instance_id(),
            generation: q.generation,
            channel: q.channel_id,
            actor: q.owner_user_id,
            message,
            answer,
            mode: prepared.0,
            baseline_turn_ids: prepared.1.clone(),
            prompt,
            now: super::super::now()?,
        },
    )?;
    let checked = async {
        store::require_current_mapping(db, q)?;
        if preflight(server, q).await? != prepared {
            return Err(invalid(
                "질문의 원래 작업 상태가 변경되어 답변하지 않았습니다.",
            ));
        }
        store::validate_dispatch_guards(db, &q.thread_id)?;
        Ok::<_, ComponentWorkerError>(())
    }
    .await;
    if let Err(error) = checked {
        store::reject_definite(db, &q.id, &error.to_string())?;
        return Err(error);
    }
    Ok(prepared.0)
}

async fn preflight(
    server: &ResidentAppServer,
    q: &Question,
) -> Result<(DispatchMode, Vec<String>), ComponentWorkerError> {
    let deadline = Instant::now() + Duration::from_secs(8);
    let snapshot = server.lifecycle_snapshot().await;
    if !snapshot.healthy
        || snapshot.quarantined
        || snapshot.restart_pending
        || server.instance_id() != q.runtime_id
        || i64::try_from(snapshot.generation).ok() != Some(q.generation)
    {
        return Err(invalid(
            "질문을 만든 기존 연결이 변경되었거나 준비되지 않았습니다. 답변하지 않았습니다.",
        ));
    }
    if let Some(active) = server.active_turn_id(&q.thread_id).await? {
        return if active == q.turn_id {
            Ok((DispatchMode::Steer, Vec::new()))
        } else {
            Err(invalid(
                "새 작업이 이미 시작되어 이전 질문은 만료되었습니다.",
            ))
        };
    }
    let index =
        crate::app_backend::turn_index::read(server, &q.thread_id, snapshot.generation, deadline)
            .await
            .map_err(|error| invalid(&error.to_string()))?;
    if index.latest_turn_id.as_deref() != Some(&q.turn_id)
        || !index
            .turns
            .iter()
            .any(|turn| turn.turn_id == q.turn_id && turn.status == TurnStatus::Completed)
        || index
            .turns
            .iter()
            .any(|turn| turn.status == TurnStatus::InProgress)
    {
        return Err(invalid(
            "질문의 원래 작업이 마지막 완료 작업인지 확인되지 않았습니다. 이전 질문을 새 작업에 전달하지 않습니다.",
        ));
    }
    let mut goal_request = get_goal(&q.thread_id);
    goal_request.timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|v| !v.is_zero())
        .ok_or_else(|| invalid("답변 전 상태 확인이 8초 안에 끝나지 않았습니다."))?;
    let goal = server
        .execute(goal_request, Some(snapshot.generation))
        .await?;
    if cdr_app_server::goal::parse_thread_goal_status(&goal, &q.thread_id)
        .map_err(|e| invalid(&e.to_string()))?
        == Some(cdr_app_server::goal::ThreadGoalStatus::Active)
    {
        return Err(invalid(
            "목표 작업이 계속 실행 중입니다. 원래 작업이 활성 상태일 때 답하거나 새 메시지로 요청해 주세요.",
        ));
    }
    Ok((
        DispatchMode::Start,
        index.turns.into_iter().map(|turn| turn.turn_id).collect(),
    ))
}
