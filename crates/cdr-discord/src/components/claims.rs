use sha2::{Digest, Sha256};

use super::{ComponentId, parse_component_id};

#[must_use]
pub fn persistent_claim_key(message_id: u64, custom_id: &str) -> Option<String> {
    persistent_component_claim_key(message_id, &parse_component_id(custom_id)?)
}

#[must_use]
pub fn persistent_component_claim_key(message_id: u64, component: &ComponentId) -> Option<String> {
    let kind = match component {
        ComponentId::AsyncChoice { question_id, .. }
        | ComponentId::AsyncTextAnswer { question_id, .. } => {
            return Some(format!("async-question:{message_id}:{question_id}"));
        }
        ComponentId::Approval { .. } => "codex_approval",
        ComponentId::Input { .. } => "codex_input",
        ComponentId::BoundApproval {
            thread_fingerprint,
            request_fingerprint,
            ..
        } => {
            return Some(bound_claim_key(
                message_id,
                "codex_approval",
                thread_fingerprint,
                request_fingerprint,
            ));
        }
        ComponentId::BoundInput {
            thread_fingerprint,
            request_fingerprint,
            ..
        } => {
            return Some(bound_claim_key(
                message_id,
                "codex_input",
                thread_fingerprint,
                request_fingerprint,
            ));
        }
        ComponentId::AsyncTextInput { .. }
        | ComponentId::Busy { .. }
        | ComponentId::RecoveryPublicationDecision { .. }
        | ComponentId::RecoveryAbandonDecision { .. } => return None,
    };
    Some(hex::encode(Sha256::digest(
        format!("{kind}:{message_id}").as_bytes(),
    )))
}

fn bound_claim_key(message_id: u64, kind: &str, thread: &str, request: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cdr-discord/component-claim/v2");
    hash_field(&mut hasher, kind.as_bytes());
    hasher.update(message_id.to_be_bytes());
    hash_field(&mut hasher, thread.as_bytes());
    hash_field(&mut hasher, request.as_bytes());
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}
