use cdr_discord::gateway::{
    GatewayIdentityConflictReceiver, GatewayIdentityReceiver, ingress::MessageIngress,
};
use tokio::sync::{mpsc, watch};

use super::super::DiscordRuntimeError;
use super::TypedIngressContext;
use super::identity::{fail_on_conflict, guard, wait_for_identity};
use crate::discord_runtime::message_create::handle_message_create;

pub(super) async fn run(
    mut messages: mpsc::Receiver<MessageIngress>,
    mut identity: GatewayIdentityReceiver,
    mut conflict: GatewayIdentityConflictReceiver,
    context: TypedIngressContext,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), DiscordRuntimeError> {
    let Some(identity) = wait_for_identity(&mut identity, &mut conflict, &mut shutdown).await?
    else {
        return Ok(());
    };
    let (resume, mut resolved) = mpsc::channel(32);
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        fail_on_conflict(&conflict)?;
        let envelope = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
                continue;
            }
            changed = conflict.changed() => {
                super::identity::handle_conflict_change(&changed, &conflict, &shutdown)?;
                continue;
            }
            message = messages.recv() => message,
            message = resolved.recv() => message,
        };
        let Some(envelope) = envelope else {
            return Err(DiscordRuntimeError::TypedIngressClosed("message"));
        };
        let envelope = if let Some(agy) = &context.agy {
            let Some(envelope) = agy.message(envelope, identity.user_id.get(), resume.clone())
            else {
                continue;
            };
            envelope
        } else {
            envelope
        };
        let process = Box::pin(handle_message_create(
            envelope.event.0,
            envelope.nonce,
            Some(identity.user_id.get()),
            identity.application_id,
            &context,
        ));
        let Some(result) = guard(process, &mut conflict, &mut shutdown).await? else {
            return Ok(());
        };
        result?;
    }
}
