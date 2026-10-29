//! Process configuration and executable orchestration for Codex Discord Remote.

// Share external-facing integration fixtures with connected library tests.
#[cfg(test)]
extern crate self as cdr_runtime;

pub mod action_executor;
pub mod action_ui;
pub mod admin;
pub mod agy;
pub mod app_backend;
pub mod archive_delete;
#[cfg(test)]
mod async_orphan_tests;
mod async_question_ui;
pub mod attachments;
pub mod bridge_state;
mod cleanup_refusal;
pub mod command_plan;
pub mod commentary_stream;
pub mod completion_worker;
pub mod component_worker;
pub mod config;
pub mod context_report;
mod context_view;
pub mod dead_generation_recovery;
mod diagnostic_report;
pub mod discord_dispatch;
pub mod discord_runtime;
mod error_message;
pub mod final_watch;
pub mod history_poll;
mod idle_release;
pub mod interaction_worker;
#[cfg(test)]
mod manual_reserve_tests;
pub mod message_plan;
pub mod message_worker;
pub mod mirror_sync;
mod new_reply_worker;
pub mod operation_marker;
pub mod prefix_plan;
pub mod pro_runtime;
pub mod prompt_intake_worker;
pub mod prompt_preprocessor;
mod queue_recovery_transport;
pub mod queue_runner;
mod resource_report;
pub mod restart_readiness;
pub mod runtime_instance;
pub mod runtime_paths;
pub mod server_prompt;
pub mod server_prompt_authority;
pub mod server_prompt_delivery;
pub mod server_prompt_redisplay;
pub mod server_request_worker;
pub mod session_mirror;
pub mod session_mirror_worker;
pub mod settings_binding;
pub mod soak;
pub mod startup;
#[cfg(test)]
mod test_support;
pub mod writer_recovery;
