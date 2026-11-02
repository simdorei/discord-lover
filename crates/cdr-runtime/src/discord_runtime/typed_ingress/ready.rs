use std::future::pending;
use std::time::Duration;

use cdr_discord::gateway::{GatewayIdentityConflictReceiver, GatewayIdentityReceiver};
use cdr_discord::http::{CommandRegistrationScope, DiscordHttp};
use tokio::sync::watch;
use twilight_model::id::Id;

use super::super::DiscordRuntimeError;
use super::TypedIngressContext;
use super::identity::{guard, wait_for_identity};
use crate::discord_runtime::startup_notice::{STARTUP_NOTICE_DOMAIN, StartupNoticeState};

const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

pub(super) async fn run(
    mut identity: GatewayIdentityReceiver,
    mut conflict: GatewayIdentityConflictReceiver,
    context: TypedIngressContext,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    let Some(identity) = wait_for_identity(&mut identity, &mut conflict, &mut shutdown).await?
    else {
        return Ok(());
    };
    eprintln!(
        "discord_ready user={} application={}",
        identity.user_id, identity.application_id
    );
    let registration = register_until_success(identity.application_id, &context);
    let notice = send_notice_until_success(identity.application_id, &context);
    let setup = async {
        tokio::join!(registration, notice);
    };
    if guard(setup, &mut conflict, &mut shutdown).await?.is_none() {
        return Ok(());
    }
    let _ = guard(pending::<()>(), &mut conflict, &mut shutdown).await?;
    Ok(())
}

async fn register_until_success(
    application_id: Id<twilight_model::id::marker::ApplicationMarker>,
    context: &TypedIngressContext,
) {
    let api = DiscordHttp::new(context.http.clone(), application_id)
        .with_store(context.executor.mirror_db(), None);
    let scope = context
        .config
        .guild_id
        .map_or(CommandRegistrationScope::Global, |guild| {
            CommandRegistrationScope::Guild(Id::new(guild))
        });
    let mut failures = 0_usize;
    loop {
        match api
            .register_slash_commands(scope, context.config.qa_commands)
            .await
        {
            Ok(()) => return,
            Err(error) => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                eprintln!(
                    "discord_ready_registration_failed retry_seconds={} error={error}",
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn send_notice_until_success(
    application_id: Id<twilight_model::id::marker::ApplicationMarker>,
    context: &TypedIngressContext,
) {
    if !context.config.startup_notify {
        return;
    }
    let Some(channel) = context.config.startup_channel_id.map(Id::new) else {
        return;
    };
    let api = DiscordHttp::new(context.http.clone(), application_id)
        .with_store(context.executor.mirror_db(), None);
    let mut state = StartupNoticeState::new(uuid::Uuid::new_v4().simple().to_string());
    let mut failures = 0_usize;
    loop {
        let logical_key = state.logical_key().to_owned();
        match state
            .try_send(|| {
                api.send_idempotent_message(
                    channel,
                    "Codex Discord Rust runtime started.",
                    &[],
                    STARTUP_NOTICE_DOMAIN,
                    &logical_key,
                    0,
                )
            })
            .await
        {
            Ok(_) => return,
            Err(error) => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                eprintln!(
                    "discord_startup_notice_failed retry_seconds={} error={error}",
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn retry_delay(failures: usize) -> Duration {
    RETRY_DELAYS[failures.min(RETRY_DELAYS.len() - 1)]
}

#[cfg(test)]
#[path = "ready_tests.rs"]
mod tests;
