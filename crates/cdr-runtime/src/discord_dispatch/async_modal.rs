//! Opening an input window is read-only; only a submitted answer acquires custody.
use cdr_discord::components::{ComponentId, deferred_update};
use cdr_discord::interaction::RoutedWork;
use cdr_discord::interaction_access::RoutedInteraction;
use cdr_discord::responses::interaction_message;
use cdr_store::async_question;
use std::path::Path;
use twilight_model::http::interaction::InteractionResponse;
use twilight_model::id::Id;

pub(super) fn answer_target(
    database: &Path,
    request: &super::custody::StageRequest<'_>,
) -> Result<Option<String>, cdr_store::StoreError> {
    let RoutedWork::Component(
        ComponentId::AsyncChoice { question_id, .. }
        | ComponentId::AsyncTextAnswer { question_id, .. },
    ) = request.work
    else {
        return Ok(None);
    };
    let q = async_question::get(database, question_id)?;
    if u64::try_from(q.channel_id).ok() != Some(request.channel_id)
        || u64::try_from(q.owner_user_id).ok() != Some(request.user_id)
        || request.source_message_id.is_none()
        || request.source_message_id.map(|id| id.to_string()) != q.message_id
    {
        return Err(cdr_store::StoreError::Integrity(
            "question actor, channel or source message does not match".into(),
        ));
    }
    async_question::require_current_mapping(database, &q)?;
    Ok(Some(q.thread_id))
}

pub(super) fn response(database: &Path, routed: &RoutedInteraction) -> Option<InteractionResponse> {
    let Some(RoutedWork::Component(ComponentId::AsyncTextInput { question_id })) = &routed.work
    else {
        return None;
    };
    let checked = (|| -> Result<_, cdr_store::StoreError> {
        let q = async_question::get(database, question_id)?;
        if routed.channel_id.map(Id::get) != u64::try_from(q.channel_id).ok()
            || routed.user_id.map(Id::get) != u64::try_from(q.owner_user_id).ok()
            || routed.source_message_id.is_none()
            || routed.source_message_id.map(|id| id.to_string()) != q.message_id
            || q.body.options.is_empty()
        {
            return Err(cdr_store::StoreError::Integrity(
                "question binding does not match".into(),
            ));
        }
        async_question::require_current_mapping(database, &q)?;
        Ok(q.state)
    })();
    Some(match checked {
        Ok(state) if state == "open" => routed.initial_response.clone(),
        Ok(state) if state == "submitted" => deferred_update(),
        Ok(_) | Err(_) => interaction_message(
            "이 질문은 지금 답변할 수 없습니다. 최신 질문을 확인해 주세요.",
            true,
        ),
    })
}
