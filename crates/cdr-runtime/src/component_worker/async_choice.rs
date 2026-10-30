use super::{ComponentWorkerError, ConfirmationPlan};
use crate::{
    action_executor::ActionExecutor,
    discord_dispatch::{InboundInteractionWork, InteractionProcessingMode},
    queue_runner::TurnBackend,
};
use cdr_app_server::{
    AppServerError, ResidentAppServer,
    requests::{start_turn, steer_turn},
};
use cdr_store::async_question::{self as store, Answer, DispatchMode, Question};
mod preparation;

pub(super) async fn handle<B: TurnBackend>(
    work: &InboundInteractionWork,
    id: &str,
    answer: Answer,
    executor: &ActionExecutor<B>,
    server: &ResidentAppServer,
) -> Result<ConfirmationPlan, ComponentWorkerError> {
    let db = executor.mirror_db();
    let q = store::get(db, id)?;
    let _lock = executor
        .control_lock(&q.thread_id)
        .await
        .map_err(|e| invalid(&e.to_string()))?;
    let q = store::get(db, id)?;
    let message = work
        .source_message_id
        .ok_or(ComponentWorkerError::MissingSourceMessage)?
        .to_string();
    if u64::try_from(q.channel_id).ok() != Some(work.channel_id.get())
        || u64::try_from(q.owner_user_id).ok() != Some(work.user_id.get())
        || q.message_id.as_deref() != Some(&message)
        || answer.validate(&q).is_err()
    {
        return Err(invalid(
            "질문의 사용자·방·메시지·선택지가 일치하지 않아 답변하지 않았습니다.",
        ));
    }
    store::require_current_mapping(db, &q)?;
    if q.state == "submitted" {
        if store::chosen_answer(&q)? != answer {
            return Err(ComponentWorkerError::AlreadyHandled);
        }
        return Ok(confirmation(&q)); // Display recovery only, never another RPC.
    }
    if work.processing_mode == InteractionProcessingMode::ConfirmationOnly {
        return Err(ComponentWorkerError::ActionUnconfirmed);
    }
    if q.state != "open" {
        return Err(invalid(&format!(
            "질문 답변 상태: {}. 자동 재전송하지 않습니다. {}",
            q.state, q.error
        )));
    }
    let prompt = store::answer_prompt_with(&q, &answer)?;
    let mode = preparation::prepare(db, server, &q, answer, &message, &prompt)
        .await
        .map_err(|error| {
            ComponentWorkerError::AsyncAnswerNotDispatched(cdr_store::ingress::AsyncAnswerRefusal {
                thread_id: q.thread_id.clone(),
                question_id: q.id.clone(),
                reason: error.to_string(),
            })
        })?;
    dispatch_claimed(db, server, &q, mode, &prompt).await?;
    executor.notify_delivery_ready();
    Ok(confirmation(&q))
}

async fn dispatch_claimed(
    db: &std::path::Path,
    server: &ResidentAppServer,
    q: &Question,
    mode: DispatchMode,
    prompt: &str,
) -> Result<(), ComponentWorkerError> {
    let id = &q.id;
    let request = match mode {
        DispatchMode::Steer => steer_turn(&q.thread_id, prompt, &q.turn_id),
        DispatchMode::Start => start_turn(&q.thread_id, prompt),
    };
    let generation =
        u64::try_from(q.generation).map_err(|_| invalid("invalid question generation"))?;
    let response = match server.execute(request, Some(generation)).await {
        Ok(response) => response,
        Err(error) => {
            if mode == DispatchMode::Start
                && let AppServerError::Remote { data, .. } = &error
                && cdr_app_server::is_usage_limit_error(data.as_ref())
            {
                store::reject_usage_limit(db, id, &error.to_string())?;
                return Err(error.into());
            }
            if matches!(
                error,
                AppServerError::Remote { .. }
                    | AppServerError::GenerationMismatch { .. }
                    | AppServerError::GenerationQuarantined { .. }
                    | AppServerError::DeadGenerationFence { .. }
            ) {
                store::reject_definite(db, id, &error.to_string())?;
            } else {
                store::record_error(db, id, &error.to_string())?;
            }
            return Err(error.into());
        }
    };
    let accepted = match mode {
        DispatchMode::Steer => response.get("turnId"),
        DispatchMode::Start => response.get("turn").and_then(|t| t.get("id")),
    }
    .and_then(serde_json::Value::as_str);
    let Some(accepted) = accepted.filter(|v| !v.is_empty()) else {
        store::record_error(
            db,
            id,
            "app-server response omitted accepted turn identity; answer outcome held",
        )?;
        return Err(ComponentWorkerError::ActionOutcomeIndeterminate(
            "accepted turn identity missing".into(),
        ));
    };
    store::confirm_dispatch(db, id, accepted)?;
    Ok(())
}

fn confirmation(q: &Question) -> ConfirmationPlan {
    ConfirmationPlan {
        content: "선택한 답변을 원래 Codex 스레드에 전달했습니다.",
        domain: "async-question-confirmation-v1",
        logical_key: q.id.clone(),
    }
}

fn invalid(message: &str) -> ComponentWorkerError {
    ComponentWorkerError::AsyncQuestion(message.into())
}
