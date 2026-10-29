use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use cdr_discord::gateway::GatewayIdentity;
use cdr_discord::gateway::ingress::{MessageGapFence, MessageGapReceiver};
use cdr_store::history::history_poll_targets;
use twilight_model::id::{Id, marker::ChannelMarker};

use super::super::super::DiscordRuntimeError;
use super::super::TypedIngressContext;
use crate::discord_runtime::bootstrap::interaction_policy;
use crate::history_poll::discord_adapter::DiscordHistoryCycleIo;
use crate::history_poll::{
    HistoryPollRunError, HistoryPollState, PollStartedAt, run_begun_history_poll_cycle,
};
use crate::message_worker::MessageAdmissionError;
use crate::restart_readiness::drain::DrainGateError;

#[allow(
    clippy::too_many_lines,
    reason = "one ordered recovery or race scenario keeps custody boundaries visible"
)]
pub(super) async fn run_periodic(
    state: &mut HistoryPollState,
    identity: GatewayIdentity,
    gaps: &MessageGapReceiver,
    context: &TypedIngressContext,
) -> Result<(), DiscordRuntimeError> {
    let targets = match history_poll_targets(
        context.mirror_db(),
        &context.config.allowed_channel_ids,
        context.config.startup_channel_id,
    ) {
        Ok(targets) => targets,
        Err(error) => {
            eprintln!("discord_history_targets_failed: {error}");
            return Ok(());
        }
    };
    let retained = targets
        .iter()
        .map(|target| target.channel_id)
        .collect::<HashSet<_>>();
    state.retain_channels(&retained);

    for target in targets {
        if let Some(agy) = &context.agy
            && !agy.owner(target.channel_id).permits_codex()
        {
            agy.classify_later(target.channel_id);
            continue;
        }
        let _admission = match context.admission.try_enter() {
            Ok(permit) => permit,
            Err(DrainGateError::Sealed) => {
                eprintln!("discord_history_poll_skipped reason=restart_drain_sealed");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let Some(gap_fence) = capture_clear_gap_fence(gaps, target.channel_id)? else {
            eprintln!(
                "discord_history_poll_skipped channel={} reason=pending_message_gap",
                target.channel_id
            );
            continue;
        };
        let observed_at = SystemTime::now();
        let poll_started_at = poll_started_at(observed_at)?;
        let cycle = state.begin(target.channel_id, poll_started_at)?;
        let policy = match interaction_policy(&context.config, context.mirror_db()) {
            Ok(policy) => policy,
            Err(error) => {
                eprintln!(
                    "discord_history_policy_failed channel={} source={} error={error}",
                    target.channel_id,
                    target.source.label()
                );
                continue;
            }
        };
        let message_context = context.message_context(identity.application_id);
        let mut io = DiscordHistoryCycleIo::new_fenced(
            message_context,
            policy,
            Some(identity.user_id.get()),
            observed_at,
            gaps,
            gap_fence,
        )
        .with_ownership(
            context
                .agy
                .as_ref()
                .map(crate::agy::routing::AgyRouting::ownership),
        );
        match run_begun_history_poll_cycle(state, target.channel_id, cycle, &mut io).await {
            Ok(outcome) => eprintln!(
                "discord_history_poll channel={} source={} phase={:?} fetched={} processed={}",
                target.channel_id,
                target.source.label(),
                outcome.phase,
                outcome.fetched,
                outcome.processed
            ),
            Err(HistoryPollRunError::Source(error)) => eprintln!(
                "discord_history_source_failed channel={} error={error}",
                target.channel_id
            ),
            Err(HistoryPollRunError::Adaptation(error)) => eprintln!(
                "discord_history_adaptation_failed channel={} error={error}",
                target.channel_id
            ),
            Err(HistoryPollRunError::Claim(error)) if error.is_gap_fence_advanced() => eprintln!(
                "discord_history_poll_skipped channel={} reason=message_gap_advanced",
                target.channel_id
            ),
            Err(HistoryPollRunError::Claim(error @ MessageAdmissionError::MessageGapFence(_))) => {
                return Err(error.into());
            }
            Err(HistoryPollRunError::Claim(error)) => eprintln!(
                "discord_history_claim_failed channel={} error={error}",
                target.channel_id
            ),
            Err(HistoryPollRunError::Process(
                error @ MessageAdmissionError::OwnershipChanged { .. },
            )) => eprintln!("discord_history_ownership_deferred: {error}"),
            Err(HistoryPollRunError::Process(error)) => return Err(error.into()),
            Err(HistoryPollRunError::State(error)) => return Err(error.into()),
            Err(HistoryPollRunError::Commit(error)) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
fn channel_has_pending_gap(
    gaps: &MessageGapReceiver,
    channel_id: u64,
) -> Result<bool, DiscordRuntimeError> {
    Ok(gaps
        .snapshot()?
        .into_iter()
        .any(|notice| notice.snapshot().channel_id.get() == channel_id))
}

fn capture_clear_gap_fence(
    gaps: &MessageGapReceiver,
    channel_id: u64,
) -> Result<Option<MessageGapFence>, DiscordRuntimeError> {
    let Some(channel_id) = Id::<ChannelMarker>::new_checked(channel_id) else {
        return Err(DiscordRuntimeError::InvalidHistoryChannel);
    };
    Ok(gaps.capture_clear_fence(channel_id)?)
}

fn poll_started_at(observed_at: SystemTime) -> Result<PollStartedAt, DiscordRuntimeError> {
    let duration = observed_at.duration_since(UNIX_EPOCH)?;
    let micros =
        i64::try_from(duration.as_micros()).map_err(|_| DiscordRuntimeError::SystemClockRange)?;
    Ok(PollStartedAt::from_normalized_utc_micros(micros))
}

#[cfg(test)]
#[path = "cycle_tests.rs"]
mod tests;
