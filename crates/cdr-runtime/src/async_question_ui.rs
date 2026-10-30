//! Question body and interactive controls use distinct, recorded HTTP identities.
use crate::completion_worker::{
    CompletionWorkerError, IdempotentChunk, send_recorded_message_with_components,
};
use cdr_app_server::async_questions;
use cdr_store::async_question::{self as store, Question};
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;
use twilight_http::Client;
use twilight_model::id::Id;

#[cfg(test)]
#[path = "async_question_ui_tests.rs"]
mod tests;

pub(crate) fn observe(
    db: &Path,
    runtime: &str,
    generation: u64,
    params: &Value,
) -> Result<(), CompletionWorkerError> {
    let Some(mut parsed) =
        async_questions::parse(params).map_err(CompletionWorkerError::Delivery)?
    else {
        return Ok(());
    };
    let source_text = if parsed.questions.is_empty() {
        parsed.questions.push(async_questions::AsyncQuestion {
            title: parsed.text.clone(),
            options: vec![],
        });
        String::new()
    } else {
        parsed.text.clone()
    };
    let generation = i64::try_from(generation)
        .map_err(|_| CompletionWorkerError::Delivery("invalid question generation".into()))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| CompletionWorkerError::Delivery(error.to_string()))?
        .as_secs_f64();
    for (index, question) in parsed.questions.into_iter().enumerate() {
        store::record_observation(
            db,
            &store::NewQuestion {
                runtime_id: runtime,
                generation,
                thread_id: &parsed.thread_id,
                turn_id: &parsed.turn_id,
                item_id: &parsed.item_id,
                body: &store::QuestionBody {
                    index,
                    source_text: source_text.clone(),
                    title: question.title,
                    options: question.options,
                },
                now,
            },
        )?;
    }
    store::reconcile_observations(db, runtime, generation)?;
    Ok(())
}

pub(crate) async fn deliver_pending(
    db: &Path,
    runtime: &str,
    generation: u64,
    http: &Client,
) -> Result<(), CompletionWorkerError> {
    prepare_pending(db, runtime, generation)?;
    let mut first = None;
    for q in store::pending(db, runtime)? {
        if let Err(error) = deliver_checked(db, generation, http, &q).await
            && first.is_none()
        {
            first = Some(error);
        }
    }
    first.map_or(Ok(()), Err)
}

pub(crate) fn prepare_pending(
    db: &Path,
    runtime: &str,
    generation: u64,
) -> Result<(), CompletionWorkerError> {
    let generation_i64 = i64::try_from(generation)
        .map_err(|_| CompletionWorkerError::Delivery("invalid question generation".into()))?;
    store::retire_old_owner(db, runtime, generation_i64)?;
    store::reconcile_observations(db, runtime, generation_i64)?;
    store::compact_terminal(
        db,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| CompletionWorkerError::Delivery(e.to_string()))?
            .as_secs_f64(),
    )?;
    Ok(())
}

pub(crate) async fn deliver_checked(
    db: &Path,
    generation: u64,
    http: &Client,
    q: &Question,
) -> Result<(), CompletionWorkerError> {
    if u64::try_from(q.generation).ok() != Some(generation) || q.state != "observed" {
        return Ok(());
    }
    let result = async {
        store::require_current_mapping(db, q)?;
        if store::owner_confirmed(db, q)? {
            deliver_one(db, http, q).await?;
        }
        Ok::<_, CompletionWorkerError>(())
    }
    .await;
    if let Err(error) = &result {
        store::record_error(db, &q.id, &error.to_string())?;
    }
    result
}

pub(crate) async fn deliver_one(
    db: &Path,
    http: &Client,
    q: &Question,
) -> Result<(), CompletionWorkerError> {
    let channel =
        Id::new(u64::try_from(q.channel_id).map_err(|_| CompletionWorkerError::ChannelId)?);
    // One item can contain several questions. Reuse one item-scoped receipt for
    // its original context, including retries from a later question in that item.
    if !q.body.source_text.is_empty() {
        let key = serde_json::to_string(&(&q.thread_id, &q.turn_id, &q.item_id))
            .map_err(|e| CompletionWorkerError::Delivery(e.to_string()))?;
        for (index, content) in
            cdr_discord::text::split_exact_delivery_chunks(&q.body.source_text, true)
                .into_iter()
                .enumerate()
        {
            send_recorded_message_with_components(
                db,
                http,
                channel,
                &IdempotentChunk {
                    domain: "async-question-item-text-v1",
                    logical_key: key.clone(),
                    chunk_index: index,
                    content,
                },
                &[],
            )
            .await?;
        }
    }
    let mut body = format!("질문 {}\n{}", q.body.index + 1, q.body.title);
    for (index, option) in q.body.options.iter().enumerate() {
        let _ = write!(body, "\n{}. {option}", index + 1);
    }
    // Renderability does not alter question/final classification or erase its text.
    let (rows, controls) = match cdr_discord::components::async_answer_rows(&q.id, &q.body.options)
    {
        Ok(rows) => (
            rows,
            format!("질문 {}의 답변을 선택하세요.", q.body.index + 1),
        ),
        Err(error) => (
            vec![],
            format!(
                "질문 {}: 선택 버튼을 만들 수 없습니다 ({error}). 자유 입력 또는 1~25개 범위를 벗어난 선택지는 이 버튼 경로에서 지원하지 않습니다. 새 메시지로 답해주세요.",
                q.body.index + 1
            ),
        ),
    };
    for (index, content) in cdr_discord::text::split_exact_delivery_chunks(&body, true)
        .into_iter()
        .enumerate()
    {
        send_recorded_message_with_components(
            db,
            http,
            channel,
            &IdempotentChunk {
                domain: "async-question-body-v1",
                logical_key: q.id.clone(),
                chunk_index: index,
                content,
            },
            &[],
        )
        .await?;
    }
    send_recorded_message_with_components(
        db,
        http,
        channel,
        &IdempotentChunk {
            domain: store::DELIVERY_DOMAIN,
            logical_key: q.id.clone(),
            chunk_index: 0,
            content: controls,
        },
        &rows,
    )
    .await?;
    store::bind_receipt(db, &q.id, !rows.is_empty())?;
    Ok(())
}
