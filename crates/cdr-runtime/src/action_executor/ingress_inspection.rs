use cdr_store::ingress::{get_for_owner_readonly, list_for_owner};
use serde_json::Value;

use super::{ActionError, ActionExecutor, id_i64};
use crate::queue_runner::TurnBackend;

#[cfg(test)]
#[path = "ingress_delivery_contract.rs"]
mod delivery_contract;

impl<B: TurnBackend> ActionExecutor<B> {
    pub(super) async fn runners_for_actor(
        &self,
        channel_id: u64,
        user_id: u64,
    ) -> Result<String, ActionError> {
        Ok(format!(
            "{}\n\n{}\n\n{}",
            self.runners_message()?,
            self.runner_target_summary(channel_id).await?,
            self.saved_request_summary(channel_id, user_id)?,
        ))
    }

    fn saved_request_summary(&self, channel_id: u64, user_id: u64) -> Result<String, ActionError> {
        let records = list_for_owner(&self.mirror_db, id_i64(channel_id)?, id_i64(user_id)?)?;
        let mut lines = vec!["Your saved requests needing attention (latest 20):".to_owned()];
        for record in records {
            if let Some(refusal) =
                cdr_store::ingress::CleanupRefusal::from_outcome(record.outcome.as_ref())
            {
                lines.push(format!(
                    "{} | status: sync stopped; notification unconfirmed | reason: {}",
                    record.ingress_id,
                    one_line(
                        if record.hold_reason.is_empty() {
                            refusal.pending_summary()
                        } else {
                            record.hold_reason
                        }
                        .as_str()
                    )
                ));
                continue;
            }
            if let Some(refusal) =
                cdr_store::ingress::AsyncAnswerRefusal::from_outcome(record.outcome.as_ref())
            {
                lines.push(format!(
                    "{} | status: answer not sent; notification unconfirmed | target: {} | reason: {}",
                    record.ingress_id,
                    one_line(&refusal.thread_id),
                    one_line(&refusal.reason)
                ));
                continue;
            }
            let (status, default_reason) = if record.phase == "cancelled" {
                (
                    "cancelled; original confirmation pending",
                    "request cancelled by its original sender; no execution retry",
                )
            } else if record.state == "completed" {
                (
                    "execution completed; confirmation pending",
                    "confirmation delivery not recorded",
                )
            } else {
                ("saved; manual review required", "manual review required")
            };
            lines.push(format!(
                "{} | status: {status} | target: {} | reason: {}",
                record.ingress_id,
                one_line(
                    record
                        .target_thread_id
                        .as_deref()
                        .unwrap_or("not yet known")
                ),
                one_line(if record.hold_reason.is_empty() {
                    default_reason
                } else {
                    &record.hold_reason
                }),
            ));
        }
        if lines.len() == 1 {
            lines.push("none".into());
        }
        lines.push("Inspect: !runners <request_id> (original user and channel only).".into());
        Ok(lines.join("\n"))
    }

    pub(super) fn saved_request_message(
        &self,
        channel_id: u64,
        user_id: u64,
        request_id: &str,
    ) -> Result<String, ActionError> {
        let channel_id = id_i64(channel_id)?;
        let user_id = id_i64(user_id)?;
        let record = get_for_owner_readonly(&self.mirror_db, request_id, channel_id, user_id)?
            .ok_or_else(|| {
                ActionError::Invalid(
                    "saved request is unavailable for this user and channel".into(),
                )
            })?;
        // The database payload stays intact. Credential-shaped envelope fields
        // are removed only from this authorized display copy; prompt text is
        // not scanned, rewritten, logged, or sent in an automatic notice.
        let known_outcome =
            cdr_store::ingress::CleanupRefusal::from_outcome(record.outcome.as_ref())
                .map(|refusal| refusal.message())
                .or_else(|| {
                    cdr_store::ingress::AsyncAnswerRefusal::from_outcome(record.outcome.as_ref())
                        .map(|refusal| refusal.to_string())
                })
                .map_or_else(String::new, |message| {
                    format!(
                        "\nknown_outcome: {}\nnotification_confirmed: {}",
                        message, record.confirmation_delivered
                    )
                });
        let mut payload = record.payload;
        redact_credential_fields(&mut payload);
        let payload = serde_json::to_string_pretty(&payload).map_err(|error| {
            ActionError::Invalid(format!("saved payload formatting failed: {error}"))
        })?;
        let display_reason = format!("{}{known_outcome}", record.hold_reason);
        Ok(format!(
            "Saved Discord request (read-only)\nrequest_id: {}\nstate: {}\nphase: {}\ntarget: {}\nreason: {}\nThis does not retry, release, or delete the request.\nOriginal payload below omits credential fields only.\noriginal_payload:\n{payload}",
            record.ingress_id,
            record.state,
            record.phase,
            record
                .target_thread_id
                .as_deref()
                .unwrap_or("not yet known"),
            display_reason,
        ))
    }
}

fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn redact_credential_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|key, _| !is_credential_field(key));
            for value in object.values_mut() {
                redact_credential_fields(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(redact_credential_fields),
        _ => {}
    }
}

fn is_credential_field(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().replace(['_', '-'], "").as_str(),
        "token"
            | "interactiontoken"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "sessiontoken"
            | "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "password"
            | "passwd"
            | "apikey"
            | "clientsecret"
            | "secret"
            | "credentials"
            | "credential"
            | "otp"
            | "otpcode"
    )
}
