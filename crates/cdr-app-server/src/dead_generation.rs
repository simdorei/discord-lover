use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{RequestId, ServerRequestOccurrence};

/// Called synchronously under the replacement lock, before clearing dead work.
/// Implementations must commit their durable fence before returning success and
/// must not call resident APIs (which can acquire the same replacement lock).
pub trait DeadGenerationFence: Send + Sync {
    /// A short-lived effect permit acquired at the actual writer and retained
    /// through outcome recording. It never covers the lifetime of a native turn.
    fn admit_effect(
        &self,
        _method: &str,
        _params: &Value,
    ) -> Result<Option<Box<dyn Send + Sync>>, crate::AppServerError> {
        Ok(None)
    }

    fn persist(&self, work: &DeadGenerationWork) -> Result<(), crate::AppServerError>;

    /// Return true only after committing this exact wire occurrence before bytes.
    /// The default grants no new isolation capability to legacy implementations.
    /// Called once before RPC preparation, never refreshed at the final writer.
    fn request_origin(
        &self,
        _method: &str,
        _params: &Value,
    ) -> Result<Option<Value>, crate::AppServerError> {
        Ok(None)
    }

    /// Existing implementations retain their original guard; no latest revision
    /// is fabricated when an older adapter lacks origin support.
    fn begin_mutation_with_origin(
        &self,
        owner: (&str, u64),
        request: (&str, &RequestId),
        method: &str,
        params: &Value,
        scoped: bool,
        _origin: Option<&Value>,
    ) -> Result<bool, crate::AppServerError> {
        self.begin_mutation(owner, request, method, params, scoped)
    }

    fn begin_mutation(
        &self,
        _owner: (&str, u64),
        _request: (&str, &RequestId),
        _method: &str,
        _params: &Value,
        _scoped: bool,
    ) -> Result<bool, crate::AppServerError> {
        Ok(false)
    }

    /// Validate original queue authority in the same transaction as the wire
    /// claim. A legacy fence must not silently discard this new authority.
    fn begin_queue_mutation(
        &self,
        _owner: (&str, u64),
        _request: (&str, &RequestId),
        _method: &str,
        _params: &Value,
        _claim: &Value,
    ) -> Result<bool, crate::AppServerError> {
        Err(crate::AppServerError::MutationHeld {
            message: "durable fence does not support claimed queue dispatch".into(),
        })
    }

    /// Capture original response custody before any asynchronous writer wait.
    fn response_authority(
        &self,
        _owner: (&str, u64),
        _request: &crate::ServerRequest,
    ) -> Result<Option<Value>, crate::AppServerError> {
        Ok(None)
    }

    fn begin_response(
        &self,
        _owner: (&str, u64),
        _request: &crate::ServerRequest,
        _authority: &Value,
        _payload: &Value,
    ) -> Result<(), crate::AppServerError> {
        Err(crate::AppServerError::MutationHeld {
            message: "durable original response admission is unavailable".into(),
        })
    }

    fn finish_response(
        &self,
        _owner: (&str, u64),
        _request: &crate::ServerRequest,
        _authority: &Value,
        _payload: &Value,
        _outcome: &str,
    ) -> Result<(), crate::AppServerError> {
        Err(crate::AppServerError::MutationHeld {
            message: "durable original response completion is unavailable".into(),
        })
    }

    fn begin_stop_mutation(
        &self,
        _owner: (&str, u64),
        _request: (&str, &RequestId),
        _params: &Value,
        _claim: &Value,
    ) -> Result<bool, crate::AppServerError> {
        Err(crate::AppServerError::MutationHeld {
            message: "durable fence does not support original stop control".into(),
        })
    }

    fn finish_stop_mutation(
        &self,
        _owner: (&str, u64),
        _request: (&str, &RequestId),
        _outcome: &str,
        _claim: &Value,
    ) -> Result<(), crate::AppServerError> {
        Err(crate::AppServerError::MutationHeld {
            message: "durable stop completion is not supported".into(),
        })
    }

    fn finish_mutation(
        &self,
        _owner: (&str, u64),
        _request: (&str, &RequestId),
        _outcome: &str,
    ) -> Result<(), crate::AppServerError> {
        Ok(())
    }

    fn check_request(
        &self,
        _generation: u64,
        _method: &str,
        _params: &Value,
    ) -> Result<(), crate::AppServerError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeadActiveTurn {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeadServerRequest {
    pub id: RequestId,
    pub occurrence: ServerRequestOccurrence,
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeadGenerationWork {
    pub generation: u64,
    pub closed_reason: String,
    pub active_turns: Vec<DeadActiveTurn>,
    pub server_requests: Vec<DeadServerRequest>,
}

impl DeadGenerationWork {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active_turns.is_empty() && self.server_requests.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadGenerationSettleResult {
    Settled,
    AlreadySettled,
    NotEligible,
    SnapshotChanged,
}
