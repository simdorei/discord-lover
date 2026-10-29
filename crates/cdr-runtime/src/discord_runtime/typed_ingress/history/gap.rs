use std::time::SystemTime;

use cdr_discord::gateway::GatewayIdentity;
use cdr_discord::gateway::ingress::{MessageGapAckOutcome, MessageGapReceiver, MessageGapSnapshot};

use super::super::super::DiscordRuntimeError;
use super::super::TypedIngressContext;
use crate::discord_runtime::bootstrap::interaction_policy;
use crate::history_poll::discord_adapter::DiscordHistoryCycleIo;
use crate::history_poll::{
    HistoryGapCoverage, HistoryGapRecoveryError, HistoryWatermark, run_history_gap_recovery,
};
use crate::restart_readiness::drain::DrainGateError;

pub(super) async fn recover_pending(
    gaps: &MessageGapReceiver,
    identity: GatewayIdentity,
    context: &TypedIngressContext,
) -> Result<(), DiscordRuntimeError> {
    for notice in gaps.snapshot()? {
        let notice = if let Some(agy) = &context.agy {
            let Some(notice) = agy.gap(notice, gaps, identity.user_id.get()) else {
                continue;
            };
            notice
        } else {
            notice
        };
        let _admission = match context.admission.try_enter() {
            Ok(permit) => permit,
            Err(DrainGateError::Sealed) => {
                eprintln!("discord_message_gap_deferred reason=restart_drain_sealed");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let snapshot = notice.snapshot();
        let Some(floor) = HistoryWatermark::from_message(
            snapshot.earliest.timestamp_micros,
            snapshot.earliest.message_id.get(),
        ) else {
            return Err(DiscordRuntimeError::InvalidMessageGap);
        };
        let policy = match interaction_policy(&context.config, context.mirror_db()) {
            Ok(policy) => policy,
            Err(error) => {
                log_failure(snapshot, "policy", &error);
                continue;
            }
        };
        let observed_at = SystemTime::now();
        let message_context = context.message_context(identity.application_id);
        let mut io = DiscordHistoryCycleIo::new(
            message_context,
            policy,
            Some(identity.user_id.get()),
            observed_at,
        )
        .with_ownership(
            context
                .agy
                .as_ref()
                .map(crate::agy::routing::AgyRouting::ownership),
        );
        match run_history_gap_recovery(snapshot.channel_id.get(), floor, &mut io).await {
            Ok(outcome) if outcome.coverage == HistoryGapCoverage::Reached => {
                match gaps.acknowledge(notice)? {
                    MessageGapAckOutcome::Cleared => eprintln!(
                        "discord_message_gap_recovered channel={} fetched={} processed={}",
                        snapshot.channel_id, outcome.fetched, outcome.processed
                    ),
                    MessageGapAckOutcome::Stale => eprintln!(
                        "discord_message_gap_ack_stale channel={} revision={}",
                        snapshot.channel_id, snapshot.revision
                    ),
                }
            }
            Ok(outcome) => eprintln!(
                "discord_message_gap_degraded channel={} floor_timestamp={} floor_message={} \
                 fetched={} status=incomplete",
                snapshot.channel_id,
                snapshot.earliest.timestamp_micros,
                snapshot.earliest.message_id,
                outcome.fetched
            ),
            Err(HistoryGapRecoveryError::Source(error)) => {
                log_failure(snapshot, "source", &error);
            }
            Err(HistoryGapRecoveryError::Adaptation(error)) => {
                log_failure(snapshot, "adaptation", &error);
            }
            Err(HistoryGapRecoveryError::Claim(error)) => {
                log_failure(snapshot, "claim", &error);
            }
            Err(HistoryGapRecoveryError::Process(
                error @ crate::message_worker::MessageAdmissionError::OwnershipChanged { .. },
            )) => log_failure(snapshot, "ownership", &error),
            Err(HistoryGapRecoveryError::Process(error)) => return Err(error.into()),
            Err(HistoryGapRecoveryError::BatchTooLarge { actual }) => {
                return Err(DiscordRuntimeError::HistoryGapBatchTooLarge { actual });
            }
        }
    }
    Ok(())
}

fn log_failure(error_context: MessageGapSnapshot, stage: &str, error: &impl std::fmt::Display) {
    eprintln!(
        "discord_message_gap_failed channel={} stage={stage} error={error}",
        error_context.channel_id
    );
}
