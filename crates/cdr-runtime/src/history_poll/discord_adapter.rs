use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use cdr_discord::gateway::ingress::{MessageGapFence, MessageGapReceiver};
use cdr_discord::http::{DiscordHttp, DiscordHttpError};
use cdr_discord::interaction_access::InteractionAccessPolicy;
use thiserror::Error;
use twilight_model::{
    channel::Message,
    id::{Id, marker::ChannelMarker},
};

use super::{
    BoxHistoryPollFuture, HISTORY_POLL_PAGE_LIMIT, HistoryBatchItem, HistoryClaimOutcome,
    HistoryClaimPurpose, HistoryPollCycleIo, HistoryPollItem, HistoryWatermark,
};
use crate::config::RuntimeConfig;
use crate::message_worker::{
    AdmittedMessage, ErrorReportTarget, MessageAdmissionError, MessageCandidate,
    MessageClassification, MessageContext, classify_gateway_message,
    process_admitted_gateway_message, process_with_error_report, report_processing_error,
};
use crate::queue_runner::TurnBackend;

mod claim;

use claim::{DiscordHistoryDiscardFence, claim_candidate_at, discard_candidate_at};

#[derive(Debug, Error)]
pub(crate) enum DiscordHistorySourceError {
    #[error("Discord history channel ID must be non-zero: {channel_id}")]
    InvalidChannelId { channel_id: u64 },
    #[error("Discord history adapter supports a page limit of {supported}, not {requested}")]
    PageLimit { requested: usize, supported: usize },
    #[error(transparent)]
    Discord(#[from] DiscordHttpError),
}

pub(crate) struct HistoryMessageCandidate {
    candidate: MessageCandidate,
    report_target: ErrorReportTarget,
}

pub(crate) struct HistoryMessagePayload {
    message: Box<Message>,
}

pub(crate) enum HistoryAdmittedMessage {
    Process {
        admitted: AdmittedMessage,
        report_target: ErrorReportTarget,
    },
    Discarded,
}

struct DiscordHistoryMessageAdapter<'a> {
    config: &'a RuntimeConfig,
    database: PathBuf,
    policy: InteractionAccessPolicy,
    bot_user_id: Option<u64>,
    settings_resolver: Option<crate::settings_binding::SettingsTargetResolver>,
}

impl<'a> DiscordHistoryMessageAdapter<'a> {
    fn new(
        config: &'a RuntimeConfig,
        database: &Path,
        policy: InteractionAccessPolicy,
        bot_user_id: Option<u64>,
    ) -> Self {
        Self {
            config,
            database: database.to_path_buf(),
            policy,
            bot_user_id,
            settings_resolver: None,
        }
    }

    fn adapt(
        &self,
        message: Message,
    ) -> Result<HistoryBatchItem<HistoryMessageCandidate>, MessageAdmissionError> {
        let database = &self.database;
        let config = self.config;
        let policy = &self.policy;
        let bot_user_id = self.bot_user_id;
        adapt_discord_history_message_with(message, |message| {
            let classified =
                classify_gateway_message(message, database, config, policy, bot_user_id)?;
            match (classified, &self.settings_resolver) {
                (MessageClassification::Candidate(candidate), Some(resolver)) => Ok(
                    MessageClassification::Candidate(candidate.bind_settings(resolver)?),
                ),
                (other, _) => Ok(other),
            }
        })
    }
}

fn adapt_discord_history_message_with<F>(
    message: Message,
    classify: F,
) -> Result<HistoryBatchItem<HistoryMessageCandidate>, MessageAdmissionError>
where
    F: FnOnce(Message) -> Result<MessageClassification, MessageAdmissionError>,
{
    let watermark = HistoryWatermark::from_message(message.timestamp.as_micros(), message.id.get());
    if message.author.bot
        || cdr_discord::gateway::ingress::is_force_restart_message(&message.content)
    {
        return Ok(HistoryBatchItem {
            watermark,
            kind: HistoryPollItem::Ignore,
        });
    }
    let report_target = ErrorReportTarget::from_message(&message);
    let kind = match classify(message)? {
        MessageClassification::Ignore(_) => HistoryPollItem::Ignore,
        MessageClassification::Candidate(candidate) => {
            HistoryPollItem::Candidate(HistoryMessageCandidate {
                candidate,
                report_target,
            })
        }
    };
    Ok(HistoryBatchItem { watermark, kind })
}

pub(crate) struct DiscordHistoryCycleIo<'a, B: TurnBackend> {
    api: DiscordHttp,
    adapter: DiscordHistoryMessageAdapter<'a>,
    context: MessageContext<'a, B>,
    observed_at: SystemTime,
    discard_fence: Option<DiscordHistoryDiscardFence<'a>>,
    ownership: Option<Arc<crate::agy::ownership::AgyOwnership>>,
}

impl<'a, B: TurnBackend> DiscordHistoryCycleIo<'a, B> {
    #[must_use]
    pub(crate) fn new(
        context: MessageContext<'a, B>,
        policy: InteractionAccessPolicy,
        bot_user_id: Option<u64>,
        observed_at: SystemTime,
    ) -> Self {
        let api = DiscordHttp::new(Arc::clone(&context.http), context.application_id);
        let mut adapter = DiscordHistoryMessageAdapter::new(
            context.config,
            context.executor.mirror_db(),
            policy,
            bot_user_id,
        );
        adapter.settings_resolver = Some(context.executor.settings_resolver());
        Self {
            api,
            adapter,
            context,
            observed_at,
            discard_fence: None,
            ownership: None,
        }
    }

    pub(crate) fn with_ownership(
        mut self,
        ownership: Option<Arc<crate::agy::ownership::AgyOwnership>>,
    ) -> Self {
        self.ownership = ownership;
        self
    }

    fn require_codex(&self, channel: u64) -> Result<(), MessageAdmissionError> {
        if self
            .ownership
            .as_ref()
            .is_some_and(|owner| !owner.classify(channel).permits_codex())
        {
            return Err(MessageAdmissionError::OwnershipChanged { channel });
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn new_fenced(
        context: MessageContext<'a, B>,
        policy: InteractionAccessPolicy,
        bot_user_id: Option<u64>,
        observed_at: SystemTime,
        gaps: &'a MessageGapReceiver,
        fence: MessageGapFence,
    ) -> Self {
        let mut io = Self::new(context, policy, bot_user_id, observed_at);
        io.discard_fence = Some(DiscordHistoryDiscardFence::new(gaps, fence));
        io
    }
}

impl<B: TurnBackend> HistoryPollCycleIo for DiscordHistoryCycleIo<'_, B> {
    type Payload = Message;
    type Item = HistoryMessagePayload;
    type Admitted = HistoryAdmittedMessage;
    type SourceError = DiscordHistorySourceError;
    type AdaptationError = MessageAdmissionError;
    type ClaimError = MessageAdmissionError;
    type ProcessError = MessageAdmissionError;

    fn fetch(
        &mut self,
        channel_id: u64,
        limit: usize,
    ) -> BoxHistoryPollFuture<'_, Vec<Message>, DiscordHistorySourceError> {
        if limit != HISTORY_POLL_PAGE_LIMIT {
            return Box::pin(std::future::ready(Err(
                DiscordHistorySourceError::PageLimit {
                    requested: limit,
                    supported: HISTORY_POLL_PAGE_LIMIT,
                },
            )));
        }
        let Some(channel_id) = Id::<ChannelMarker>::new_checked(channel_id) else {
            return Box::pin(std::future::ready(Err(
                DiscordHistorySourceError::InvalidChannelId { channel_id },
            )));
        };
        Box::pin(async move {
            self.api
                .fetch_latest_channel_messages(channel_id)
                .await
                .map_err(DiscordHistorySourceError::from)
        })
    }

    fn adapt(
        &mut self,
        payload: Message,
    ) -> Result<HistoryBatchItem<HistoryMessagePayload>, MessageAdmissionError> {
        // Capture only ordering metadata while the whole page is assembled.
        // Stateful classification runs once, oldest first, immediately before
        // admission, after earlier !new requests in this page have committed.
        let watermark =
            HistoryWatermark::from_message(payload.timestamp.as_micros(), payload.id.get());
        let kind = if payload.author.bot {
            HistoryPollItem::Ignore
        } else {
            HistoryPollItem::Candidate(HistoryMessagePayload {
                message: Box::new(payload),
            })
        };
        Ok(HistoryBatchItem { watermark, kind })
    }

    fn claim(
        &mut self,
        item: HistoryMessagePayload,
        purpose: HistoryClaimPurpose,
    ) -> Result<HistoryClaimOutcome<HistoryAdmittedMessage>, MessageAdmissionError> {
        self.require_codex(item.message.channel_id.get())?;
        let classified = self.adapter.adapt(*item.message)?;
        let HistoryPollItem::Candidate(item) = classified.kind else {
            return Ok(HistoryClaimOutcome::Lost);
        };
        if purpose == HistoryClaimPurpose::Discard {
            return if let Some(fence) = &self.discard_fence {
                fence.claim(item, self.observed_at)
            } else {
                discard_candidate_at(item, self.observed_at)
            };
        }
        claim_candidate_at(item, self.observed_at)
    }

    fn process(
        &mut self,
        admitted: HistoryAdmittedMessage,
    ) -> BoxHistoryPollFuture<'_, (), MessageAdmissionError> {
        Box::pin(async move {
            let HistoryAdmittedMessage::Process {
                admitted,
                report_target,
            } = admitted
            else {
                return Err(cdr_store::StoreError::Integrity(
                    "discarded history is not executable".into(),
                )
                .into());
            };
            // Fetch and claim may both have yielded to topology updates. Do not
            // consume recovery progress or execute under obsolete ownership.
            self.require_codex(report_target.channel_id.get())?;
            process_with_error_report(
                report_target,
                admitted,
                |admitted| process_admitted_gateway_message(admitted, &self.context),
                |target, error| {
                    report_processing_error(
                        self.context.executor.mirror_db(),
                        &self.api,
                        target,
                        error,
                    )
                },
            )
            .await
        })
    }
}

#[cfg(test)]
#[path = "discord_adapter_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "discord_adapter_boundary_tests.rs"]
mod boundary_tests;

#[cfg(test)]
#[path = "settings_rejection_tests.rs"]
mod settings_rejection_tests;

#[cfg(test)]
#[path = "new_prompt_history_tests.rs"]
mod new_prompt_history_tests;

#[cfg(test)]
mod ownership_tests;
