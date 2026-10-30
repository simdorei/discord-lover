use serde_json::{Value, json};

/// Positive evidence that this answer attempt stopped before a mutation RPC.
#[derive(Clone, Debug, thiserror::Error)]
#[error("답변을 전송하지 않았습니다. {reason}")]
pub struct AsyncAnswerRefusal {
    pub thread_id: String,
    pub question_id: String,
    pub reason: String,
}

impl AsyncAnswerRefusal {
    #[must_use]
    pub fn outcome(&self) -> Value {
        json!({"kind":"async_answer_not_dispatched","action_completed":false,
            "answer_dispatched":false,"thread_id":self.thread_id,"question_id":self.question_id,"reason":self.reason})
    }

    #[must_use]
    pub fn from_outcome(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        if value["kind"] != "async_answer_not_dispatched"
            || value["action_completed"] != false
            || value["answer_dispatched"] != false
        {
            return None;
        }
        let thread_id = value["thread_id"].as_str()?.to_owned();
        let question_id = value["question_id"].as_str()?.to_owned();
        let reason = value["reason"].as_str()?.to_owned();
        if thread_id.trim().is_empty() || question_id.len() != 64 || reason.trim().is_empty() {
            return None;
        }
        Some(Self {
            thread_id,
            question_id,
            reason,
        })
    }
}
