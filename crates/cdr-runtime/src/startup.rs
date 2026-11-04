use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{CliOptions, ConfigError, RuntimeConfig, merge_env_text};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupArgs {
    pub cli: CliOptions,
    pub env_path: Option<PathBuf>,
    pub backup_store: bool,
    pub restart_readiness: bool,
    pub restart_quiet_seconds: u64,
    pub restart_wait_timeout_seconds: u64,
    pub help: bool,
}

#[derive(Debug, Error)]
pub enum StartupError {
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("could not read environment file {path}: {source}")]
    ReadEnv {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CliError {
    #[error("unknown argument: {0}")]
    Unknown(String),
    #[error("missing path after --env")]
    MissingEnvPath,
    #[error("argument is not valid UTF-8")]
    InvalidUtf8,
    #[error("missing value after {0}")]
    MissingValue(&'static str),
    #[error("invalid non-negative integer after {name}: {value}")]
    InvalidSeconds { name: &'static str, value: String },
    #[error("restart timing options require --restart-readiness")]
    RestartOptionsWithoutReadiness,
}

impl StartupArgs {
    pub fn parse<I>(args: I) -> Result<Self, CliError>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut parsed = Self {
            cli: CliOptions::default(),
            env_path: None,
            backup_store: false,
            restart_readiness: false,
            restart_quiet_seconds: 90,
            restart_wait_timeout_seconds: 900,
            help: false,
        };
        let mut has_restart_timing = false;
        let mut args = args.into_iter();
        while let Some(raw) = args.next() {
            let argument = raw.into_string().map_err(|_| CliError::InvalidUtf8)?;
            match argument.as_str() {
                "--no-message-content" => parsed.cli.no_message_content = true,
                "--check-config" => parsed.cli.check_config = true,
                "--backup-store" => parsed.backup_store = true,
                "--restart-readiness" => parsed.restart_readiness = true,
                "--restart-quiet-seconds" => {
                    has_restart_timing = true;
                    parsed.restart_quiet_seconds =
                        parse_seconds(args.next(), "--restart-quiet-seconds")?;
                }
                "--restart-wait-timeout-seconds" => {
                    has_restart_timing = true;
                    parsed.restart_wait_timeout_seconds =
                        parse_seconds(args.next(), "--restart-wait-timeout-seconds")?;
                }
                "--help" | "-h" => parsed.help = true,
                "--env" => {
                    parsed.env_path =
                        Some(PathBuf::from(args.next().ok_or(CliError::MissingEnvPath)?));
                }
                unknown => return Err(CliError::Unknown(unknown.to_owned())),
            }
        }
        if has_restart_timing && !parsed.restart_readiness {
            return Err(CliError::RestartOptionsWithoutReadiness);
        }
        Ok(parsed)
    }
}

fn parse_seconds(raw: Option<OsString>, name: &'static str) -> Result<u64, CliError> {
    let value = raw.ok_or(CliError::MissingValue(name))?;
    let value = value.into_string().map_err(|_| CliError::InvalidUtf8)?;
    value
        .parse()
        .map_err(|_| CliError::InvalidSeconds { name, value })
}

pub fn load_config(
    args: &StartupArgs,
    executable_path: &Path,
) -> Result<RuntimeConfig, StartupError> {
    let env = load_environment(args, executable_path)?;
    RuntimeConfig::from_map(&env, args.cli).map_err(StartupError::from)
}

pub fn load_environment(
    args: &StartupArgs,
    executable_path: &Path,
) -> Result<BTreeMap<String, String>, StartupError> {
    let mut env = process_environment();
    let env_path = args
        .env_path
        .clone()
        .unwrap_or_else(|| default_env_path(executable_path));
    merge_env_file(&mut env, &env_path)?;
    Ok(env)
}

pub fn merge_env_file(env: &mut BTreeMap<String, String>, path: &Path) -> Result<(), StartupError> {
    match fs::read_to_string(path) {
        Ok(text) => {
            merge_env_text(env, &text);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StartupError::ReadEnv {
            path: path.to_owned(),
            source,
        }),
    }
}

#[must_use]
pub fn default_env_path(executable_path: &Path) -> PathBuf {
    executable_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".env")
}

#[must_use]
pub fn config_summary(config: &RuntimeConfig) -> String {
    format!(
        "config_valid token=[REDACTED] guild={} channels={:?} users={:?} message_content={} qa_commands={}",
        config
            .guild_id
            .map_or_else(|| "-".into(), |id| id.to_string()),
        config.allowed_channel_ids,
        config.allowed_user_ids,
        config.enable_message_content,
        config.qa_commands,
    )
}

#[must_use]
pub fn help_text() -> &'static str {
    "Codex Discord Remote Rust runtime\n\nOptions:\n  --build-info                       Print embedded build provenance (standalone; no environment or DB)\n  --no-message-content               Use slash commands only\n  --check-config                     Validate configuration without connecting\n  --backup-store                     Create and verify an online cutover DB snapshot\n  --restart-readiness                Wait for fail-closed Rust restart readiness\n  --restart-quiet-seconds N          Required idle age for restart (default: 90)\n  --restart-wait-timeout-seconds N   Maximum quiet wait (default: 900)\n  --env PATH                         Load a specific environment file\n  -h, --help                         Show this help"
}

fn process_environment() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}
