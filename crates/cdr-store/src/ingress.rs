//! Durable custody before Discord acknowledgement or asynchronous preparation.
//! Receipts never authorize replay: an uncertain unowned effect is held for review.

mod admission;
mod async_answer_refusal;
pub use async_answer_refusal::AsyncAnswerRefusal;
mod busy;
mod cancellation;
mod cleanup_refusal;
pub(crate) use cancellation::cancellation_owners;
pub use cleanup_refusal::CleanupRefusal;
mod lifecycle;
mod mapped_slash;
pub use mapped_slash::{admit_mapped_slash_prompt, frozen_slash_target};
mod new_command;
mod new_creation;
mod new_evidence;
mod new_input;
mod new_prompt_arm;
mod ownership;
mod read;
mod recovery;
mod recovery_custody;
pub use recovery_custody::{RecoveryClaim, claim_recovery, validate_recovery_binding_in};
mod schema;
pub mod stop;

pub use admission::admit;
pub use busy::admit_busy_interaction;
pub use lifecycle::{
    acknowledge, begin_confirmation, begin_execution, begin_thread_start, confirm,
    record_created_thread, record_processing_mode, record_result,
};
pub use new_command::new_command_prompt;
pub use new_creation::record_new_creation;
pub(crate) use new_evidence::record_new_evidence;
pub use new_input::{new_execution_prompt, record_new_input};
pub use new_prompt_arm::pending_new_prompt;
pub(crate) use ownership::verify_new_prompt;
pub(crate) use ownership::{link_prompt_owner, link_prompt_owner_by_key, record_busy_owner};
pub use read::{by_origin, get, get_for_owner_readonly, list_for_owner, unfinished_for_archive};
pub(crate) use recovery::retire_unowned_in;
pub use recovery::{hold, recover_prior_runtime};
pub(crate) use schema::{migrate_schema, schema_current};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressKind {
    Message,
    Interaction,
    Action,
}

impl IngressKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Interaction => "interaction",
            Self::Action => "action",
        }
    }
}

#[derive(Clone)]
pub struct NewIngress {
    pub ingress_id: String,
    pub kind: IngressKind,
    pub event_id: Option<i64>,
    pub application_id: Option<i64>,
    pub channel_id: i64,
    pub owner_user_id: i64,
    pub source_message_id: Option<i64>,
    pub payload: Value,
    pub target_thread_id: Option<String>,
    pub canonical_owner: Option<String>,
    pub now: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredIngress {
    pub ingress_id: String,
    pub kind: IngressKind,
    pub event_id: Option<i64>,
    pub application_id: Option<i64>,
    pub channel_id: i64,
    pub owner_user_id: i64,
    pub source_message_id: Option<i64>,
    pub payload: Value,
    pub runtime_id: Option<String>,
    pub state: String,
    pub phase: String,
    pub target_thread_id: Option<String>,
    pub canonical_owner: Option<String>,
    pub owner_kind: Option<String>,
    pub owner_id: Option<String>,
    pub outcome: Option<Value>,
    pub confirmation_delivered: bool,
    pub hold_reason: String,
    pub created_at: f64,
    pub updated_at: f64,
}

pub struct IngressAdmission {
    pub created: bool,
    /// A new Discord event referencing an existing operation still needs its own
    /// ACK. This never grants permission to run the original operation again.
    pub canonical_repeat_created: bool,
    /// None means an old ID-only message claim. Its payload is not fabricated.
    pub record: Option<StoredIngress>,
    pub busy_choice: Option<crate::claims::BusyChoice>,
}
