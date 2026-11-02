//! Durable `SQLite` state shared by the Rust Discord runtime.

pub mod archive_fence;
pub mod archived_cleanup;
pub mod async_question;
pub mod async_resolution;
pub mod backup;
pub mod claims;
pub mod commentary_outbox;
pub mod completion_work;
pub mod control_binding;
pub mod dead_generation;
pub mod delivery;
pub mod delivery_receipt;
mod error;
pub mod execution_hold;
pub mod final_recovery;
pub mod first_reply;
pub mod goal_progress;
pub mod history;
pub mod idle_release;
pub mod ingress;
pub mod mapping;
pub mod mirror;
pub mod mutation_attempt;
pub mod new_reply;
pub mod observation_gap;
pub mod observed_completion;
pub mod observed_final_answer;
pub mod processed;
pub mod prompt_intake;
pub mod queue;
pub mod reserve_policy;
pub mod reserve_retirement;
pub mod restart_readiness;
pub mod room_cleanup;
pub mod schema;

pub use error::{Result, StoreError};
