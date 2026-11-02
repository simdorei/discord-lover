use std::sync::Arc;

use cdr_discord::http::DiscordHttp;
use twilight_model::channel::Message;

use super::reply_delivery::{MessageReplyKind, deliver_reply_text, send_reply_once};
use super::{MessageContext, MessageWorkerError, custody};
use crate::action_executor::ActionContext;
use crate::action_ui::render_action_ui;
use crate::attachments::{enrich_message_attachments, enrich_new_message_attachments};
use crate::command_plan::CommandAction;
use crate::message_plan::MessagePlan;
use crate::queue_runner::TurnBackend;

pub(super) async fn enrich_plan<B: TurnBackend>(
    message: &Message,
    context: &MessageContext<'_, B>,
    plan: MessagePlan,
) -> Result<MessagePlan, MessageWorkerError> {
    Ok(match plan {
        MessagePlan::Execute(CommandAction::New { prompt }) if !message.attachments.is_empty() => {
            let prepared = enrich_new_message_attachments(
                message,
                &prompt,
                context.config,
                context.attachment_root,
                context.attachment_client,
            )
            .await?;
            cdr_store::ingress::record_new_input(
                context.executor.mirror_db(),
                &format!("message:{}", message.id),
                &prompt,
                &prepared,
                custody::now()?,
            )?;
            MessagePlan::Execute(CommandAction::New { prompt })
        }
        MessagePlan::Execute(CommandAction::Ask { prompt }) => {
            let prompt = enrich_message_attachments(
                message,
                &prompt,
                context.config,
                context.attachment_root,
                context.attachment_client,
            )
            .await?;
            MessagePlan::Execute(CommandAction::Ask { prompt })
        }
        other => other,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "Keep the ordered validation and effect boundary visible in one workflow"
)]
pub(super) async fn execute_plan<B: TurnBackend>(
    message: &Message,
    context: &MessageContext<'_, B>,
    channel_id: u64,
    user_id: u64,
    plan: MessagePlan,
) -> Result<bool, MessageWorkerError> {
    let key = format!("message:{}", message.id);
    #[cfg(test)]
    let _timing = crate::test_support::path_timing::PhaseTimer::new("message.execute");
    let api = DiscordHttp::new(Arc::clone(&context.http), context.application_id)
        .with_store(context.executor.mirror_db(), Some(message.channel_id.get()));
    match plan {
        MessagePlan::Ignore(_) => unreachable!("ignore plans never cross durable admission"),
        MessagePlan::Respond(text) => {
            cdr_store::ingress::record_result(
                context.executor.mirror_db(),
                &key,
                &serde_json::json!({"response":text}),
                custody::now()?,
            )?;
            deliver_reply_text(
                context.executor.mirror_db(),
                &api,
                message.channel_id,
                message.id,
                MessageReplyKind::PlannedResponse,
                &text,
            )
            .await
            .map_err(|error| MessageWorkerError::Delivery(Box::new(error)))?;
        }
        MessagePlan::Execute(action) => {
            let reply_kind = if matches!(action, CommandAction::SavedRequest { .. }) {
                MessageReplyKind::SavedRequest
            } else {
                MessageReplyKind::ActionResult
            };
            let result = context
                .executor
                .execute_with_ingress_context(
                    action,
                    ActionContext {
                        channel_id,
                        user_id,
                        discord_message_id: Some(message.id.get()),
                        auto_queue_when_busy: message.author.bot,
                    },
                    &key,
                )
                .await;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    return super::cleanup_refusal::deliver(message, context, &api, &key, error)
                        .await;
                }
            };
            cdr_store::ingress::record_result(
                context.executor.mirror_db(),
                &key,
                &serde_json::json!({"response":result.text,"waits_for_final":result.waits_for_final}),
                custody::now()?,
            )?;
            if let Some(crate::action_executor::ActionUi::ServerPrompts { prompts }) = &result.ui {
                crate::server_prompt_delivery::deliver(
                    &crate::server_prompt_delivery::PromptDeliveryContext {
                        database: context.executor.mirror_db(),
                        server: context.server,
                        http: &context.http,
                        channel_id,
                        user_id,
                        command_key: &key,
                    },
                    prompts,
                )
                .await?;
                return Ok(false);
            }
            let components = render_action_ui(result.ui.as_ref())?;
            if components.is_empty() {
                deliver_reply_text(
                    context.executor.mirror_db(),
                    &api,
                    message.channel_id,
                    message.id,
                    reply_kind,
                    &result.text,
                )
                .await
                .map_err(|error| MessageWorkerError::Delivery(Box::new(error)))?;
            } else {
                send_reply_once(
                    context.executor.mirror_db(),
                    &api,
                    message.channel_id,
                    message.id,
                    MessageReplyKind::ActionResult,
                    &result.text,
                    &components,
                )
                .await?;
            }
        }
    }
    Ok(false)
}
