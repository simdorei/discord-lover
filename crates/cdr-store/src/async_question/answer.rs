use super::{ANSWER_PREFIX, Question, invalid};
use crate::Result;
use serde_json::{Value, json};

pub const FREE_TEXT_COMPONENT: &str = "async_free_text_answer";
pub const FREE_TEXT_FORMAT_VERSION: i64 = 1;
pub const MAX_ANSWER_CHARS: usize = 4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Choice(usize),
    Text(String),
}

impl Answer {
    pub(super) fn require_capability_in(&self, db: &rusqlite::Connection) -> Result<()> {
        if self.text().is_none() {
            return Ok(());
        }
        // Old binaries must reject this database at their read-only launch gate.
        db.execute("INSERT OR IGNORE INTO cdr_runtime_capability_requirements(component,format_version) VALUES (?,?)",
            rusqlite::params![FREE_TEXT_COMPONENT, FREE_TEXT_FORMAT_VERSION])?;
        let version: i64 = db.query_row(
            "SELECT format_version FROM cdr_runtime_capability_requirements WHERE component=?",
            [FREE_TEXT_COMPONENT],
            |r| r.get(0),
        )?;
        if version != FREE_TEXT_FORMAT_VERSION {
            return Err(invalid("unsupported free-text answer format"));
        }
        Ok(())
    }

    pub fn validate(&self, question: &Question) -> Result<()> {
        match self {
            Self::Choice(index) if *index < question.body.options.len() => Ok(()),
            Self::Text(text)
                if !text.trim().is_empty() && text.chars().count() <= MAX_ANSWER_CHARS =>
            {
                Ok(())
            }
            Self::Choice(_) => Err(invalid("invalid sealed answer option")),
            Self::Text(_) => Err(invalid("invalid sealed answer text")),
        }
    }

    #[must_use]
    pub const fn choice(&self) -> Option<usize> {
        match self {
            Self::Choice(index) => Some(*index),
            Self::Text(_) => None,
        }
    }

    #[must_use]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Choice(_) => None,
            Self::Text(text) => Some(text),
        }
    }
}

pub fn chosen_answer(q: &Question) -> Result<Answer> {
    let answer = match (q.chosen, q.chosen_text.as_ref()) {
        (Some(index), None) => Answer::Choice(index),
        (None, Some(text)) => Answer::Text(text.clone()),
        _ => return Err(invalid("question has no unique sealed answer")),
    };
    answer.validate(q)?;
    Ok(answer)
}

pub fn answer_prompt_with(q: &Question, answer: &Answer) -> Result<String> {
    answer.validate(q)?;
    let mut data = json!({"thread_id":q.thread_id,"original_turn_id":q.turn_id,
        "question_item_id":q.item_id,"question_index":q.body.index,"question_title":q.body.title});
    match answer {
        Answer::Choice(index) => {
            data["selected_option_index"] = json!(index);
            data["selected_option"] = json!(q.body.options[*index]);
        }
        Answer::Text(text) => {
            data["answer_kind"] = json!("free_text");
            data["answer_text"] = json!(text);
        }
    }
    Ok(format!("{ANSWER_PREFIX}\n{data}"))
}

/// Keep existing choice-only seals byte-for-byte compatible.
pub(crate) fn bind_text(identity: &mut Value, q: &Question) {
    if let Some(text) = &q.chosen_text {
        identity["answer_text"] = json!(text);
    }
}
