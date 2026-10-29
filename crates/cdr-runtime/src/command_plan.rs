pub use cdr_discord::interaction::AUTO_RESERVE_REMOVED;

use cdr_discord::interaction::SlashInvocation;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CommandAction {
    Help,
    List {
        limit: u32,
    },
    ArchivedList {
        limit: u32,
    },
    Use {
        reference: String,
    },
    Status {
        reference: Option<String>,
    },
    Settings {
        reference: Option<String>,
        model: Option<String>,
        effort: Option<String>,
        speed: Option<String>,
    },
    AutoReserve {
        reference: Option<String>,
        enabled: bool,
    },
    Where,
    Context {
        all_threads: bool,
        refresh: bool,
        limit: u32,
    },
    Usage {
        days: u32,
    },
    New {
        prompt: String,
    },
    Ask {
        prompt: String,
    },
    Interview {
        prompt: String,
    },
    Doctor,
    Runners,
    SavedRequest {
        request_id: String,
    },
    DiscardRequest {
        job_id: String,
    },
    Retract {
        reference: Option<String>,
    },
    MirrorCheck,
    MirrorInspect {
        limit: Option<u32>,
        list: bool,
    },
    BridgeSync {
        limit: Option<i64>,
    },
    QaButtons,
    Open {
        reference: String,
        abort: bool,
    },
    Stop {
        reference: Option<String>,
    },
    Recover {
        reference: Option<String>,
    },
    Repair {
        reference: Option<String>,
    },
    SettingsOptions {
        reference: Option<String>,
        field: Option<String>,
    },
    RestartCodex,
    ForceRestartCodex,
    Archive {
        reference: Option<String>,
    },
    DeleteArchivePreview {
        reference: String,
    },
    DeleteArchiveConfirm {
        reference: String,
    },
    Resume {
        reference: Option<String>,
    },
    Identity,
    Resources,
    Approval,
    Steer {
        prompt: String,
    },
    HostReboot,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CommandPlanError {
    #[error("unsupported slash command: {0}")]
    Unsupported(String),
    #[error("required slash command option is unavailable: {0}")]
    MissingOption(&'static str),
    #[error("slash command option must not be blank: {0}")]
    BlankOption(&'static str),
}

pub fn plan_slash(invocation: &SlashInvocation) -> Result<CommandAction, CommandPlanError> {
    let action = match invocation.name.as_str() {
        "help" => CommandAction::Help,
        "list" => CommandAction::List {
            limit: bounded(invocation.integer("limit"), 10, 1, 30),
        },
        "archived_list" => CommandAction::ArchivedList {
            limit: bounded(invocation.integer("limit"), 10, 1, 50),
        },
        "use" => CommandAction::Use {
            reference: required_string(invocation, "ref")?,
        },
        "status" => CommandAction::Status {
            reference: optional_reference(invocation)?,
        },
        "settings" => {
            if invocation.has_option("auto_reserve") {
                return Err(CommandPlanError::Unsupported(AUTO_RESERVE_REMOVED.into()));
            }
            let reference = optional_reference(invocation)?;
            let model = optional_string(invocation, "model")?;
            let effort = optional_string(invocation, "effort")?;
            let speed = optional_string(invocation, "speed")?;
            CommandAction::Settings {
                reference,
                model,
                effort,
                speed,
            }
        }

        "where" => CommandAction::Where,
        "stop" => CommandAction::Stop { reference: None },
        "context" => CommandAction::Context {
            all_threads: invocation.boolean("all_threads").unwrap_or(false),
            refresh: invocation.boolean("refresh").unwrap_or(false),
            limit: bounded(invocation.integer("limit"), 10, 1, 30),
        },
        "usage" => CommandAction::Usage {
            days: bounded(invocation.integer("days"), 7, 1, 30),
        },
        "new" => CommandAction::New {
            prompt: required_string(invocation, "prompt")?,
        },
        "ask" => CommandAction::Ask {
            prompt: required_string(invocation, "prompt")?,
        },
        "interview" => CommandAction::Interview {
            prompt: required_string(invocation, "prompt")?,
        },
        "doctor" => CommandAction::Doctor,
        "approval" => CommandAction::Approval,
        "runners" => CommandAction::Runners,
        "retract" => CommandAction::Retract {
            reference: optional_reference(invocation)?,
        },
        "mirror_check" => CommandAction::MirrorCheck,
        "bridge_sync" => CommandAction::BridgeSync {
            limit: invocation.integer("limit"),
        },
        "qa_buttons" => CommandAction::QaButtons,
        unknown => return Err(CommandPlanError::Unsupported(unknown.into())),
    };
    Ok(action)
}

fn required_string(
    invocation: &SlashInvocation,
    name: &'static str,
) -> Result<String, CommandPlanError> {
    let value = invocation
        .string(name)
        .map(str::trim)
        .ok_or(CommandPlanError::MissingOption(name))?;
    if value.is_empty() {
        return Err(CommandPlanError::BlankOption(name));
    }
    Ok(value.to_owned())
}

fn optional_reference(invocation: &SlashInvocation) -> Result<Option<String>, CommandPlanError> {
    invocation
        .string("ref")
        .map(|_| required_string(invocation, "ref"))
        .transpose()
}

fn optional_string(
    invocation: &SlashInvocation,
    name: &'static str,
) -> Result<Option<String>, CommandPlanError> {
    invocation
        .string(name)
        .map(|_| required_string(invocation, name))
        .transpose()
}

fn bounded(raw: Option<i64>, default: i64, minimum: i64, maximum: i64) -> u32 {
    u32::try_from(raw.unwrap_or(default).clamp(minimum, maximum)).unwrap_or_default()
}
