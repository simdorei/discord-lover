use std::process::ExitCode;
use std::time::Duration;

use cdr_app_server::APP_SERVER_STARTUP_TIMEOUT;
use cdr_runtime::config::RuntimeConfig;
use cdr_runtime::discord_runtime;
use cdr_runtime::runtime_paths::{RuntimePaths, discover_inputs};
use cdr_runtime::startup::{StartupArgs, config_summary, help_text, load_environment};

#[tokio::main]
async fn main() -> ExitCode {
    match Box::pin(run()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, error)) => {
            eprintln!("ERROR: {error}");
            ExitCode::from(code)
        }
    }
}

async fn run() -> Result<(), (u8, String)> {
    let raw_args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(windows)]
    if raw_args.len() == 1 && raw_args[0] == "--agy-control" {
        return cdr_runtime::agy::control::cli()
            .await
            .map_err(|error| (1, error.to_string()));
    }
    if raw_args.first().is_some_and(|arg| arg == "--admin") {
        let output = cdr_runtime::admin::run(raw_args.into_iter().skip(1))
            .await
            .map_err(|error| (1, error))?;
        println!("{output}");
        return Ok(());
    }
    let args = StartupArgs::parse(raw_args).map_err(|error| (2, error.to_string()))?;
    if args.help {
        println!("{}", help_text());
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(|error| {
        (
            1,
            format!("could not resolve runtime executable path: {error}"),
        )
    })?;
    let environment =
        load_environment(&args, &executable).map_err(|error| (1, error.to_string()))?;
    if args.restart_readiness {
        let root = std::env::current_dir().map_err(|error| {
            (
                1,
                format!("could not determine the working directory: {error}"),
            )
        })?;
        let inputs = discover_inputs(&environment, root).map_err(|error| (1, error.to_string()))?;
        let paths =
            RuntimePaths::resolve(&environment, &inputs).map_err(|error| (1, error.to_string()))?;
        cdr_runtime::restart_readiness::wait_for_restart(
            &paths,
            cdr_runtime::restart_readiness::RestartReadinessOptions {
                quiet: Duration::from_secs(args.restart_quiet_seconds),
                wait_timeout: Duration::from_secs(args.restart_wait_timeout_seconds),
                poll_interval: Duration::from_secs(5),
                request_timeout: Duration::from_secs(8),
                startup_timeout: APP_SERVER_STARTUP_TIMEOUT,
                close_timeout: Duration::from_secs(8),
            },
        )
        .await
        .map_err(|error| (1, error.to_string()))?;
        println!("restart_readiness_ok");
        return Ok(());
    }
    let config =
        RuntimeConfig::from_map(&environment, args.cli).map_err(|error| (1, error.to_string()))?;
    if args.cli.check_config {
        let root = std::env::current_dir().map_err(|error| (1, error.to_string()))?;
        let agy = cdr_runtime::agy::config::AgyConfig::from_environment(&environment, &root)
            .map_err(|error| (1, error.to_string()))?;
        if agy.is_some() && config.guild_id.is_none() {
            return Err((1, "DISCORD_GUILD_ID is required for AGY".into()));
        }
        println!("{}", config_summary(&config));
        println!("agy_enabled={}", agy.is_some());
        return Ok(());
    }
    if args.backup_store {
        let root = std::env::current_dir().map_err(|error| {
            (
                1,
                format!("could not determine the working directory: {error}"),
            )
        })?;
        let inputs = discover_inputs(&environment, root).map_err(|error| (1, error.to_string()))?;
        let paths =
            RuntimePaths::resolve(&environment, &inputs).map_err(|error| (1, error.to_string()))?;
        let backup = cdr_store::backup::snapshot(&paths.mirror_db)
            .map_err(|error| (1, error.to_string()))?;
        println!("backup_created path={}", backup.display());
        return Ok(());
    }
    discord_runtime::run(config, &environment)
        .await
        .map_err(|error| (1, error.to_string()))
}
