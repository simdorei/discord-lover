//! Offline upgrade inspection: no gateway, CLI, migration, login or queue writes.
use super::args::Args;
use crate::{
    agy::config::Settings,
    config::{CliOptions, RuntimeConfig},
    runtime_paths::store_paths,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

pub(super) fn run(args: &Args, root: &Path) -> Result<String, String> {
    inspect(root, args.command == "backup-upgrade").map_err(|e| format!("UPGRADE_HOLD: {e}"))
}

fn inspect(root: &Path, backup: bool) -> Result<String, Box<dyn std::error::Error>> {
    let root = root.canonicalize()?;
    let env_path = root.join(".env");
    let env_bytes = std::fs::read(&env_path)?;
    let environment = super::project_environment(&root)?;
    let _config = RuntimeConfig::from_map(&environment, CliOptions::default())?;
    let home = environment
        .get("USERPROFILE")
        .or_else(|| environment.get("HOME"));
    let (configured_root, database) =
        store_paths::resolve(&environment, &root, home.map(Path::new))?;
    if root.join(configured_root).canonicalize()? != root {
        return Err("CODEX_DISCORD_ROOT differs from the selected installation".into());
    }
    // Relative runtime paths are relative to the selected installation, not the updater.
    let database = root.join(database).canonicalize()?;
    let compatibility: serde_json::Value = serde_json::from_str(
        &super::recovery_capability::inspect(&database, Some(&env_path))?,
    )?;
    let settings_path = root.join(
        environment
            .get("DISCORD_AGY_CONFIG")
            .filter(|p| !p.trim().is_empty())
            .map_or("codex-agy.json", String::as_str),
    );
    let settings_hash = if settings_path.try_exists()? {
        if Settings::read(&settings_path)?.enabled {
            return Err(
                "AGY is enabled; this installer only upgrades a Codex-only/AGY-Off installation"
                    .into(),
            );
        }
        Some(hex::encode(Sha256::digest(std::fs::read(&settings_path)?)))
    } else {
        None
    };
    // Bind effective overrides as well as the bytes on disk. Values never leave this command.
    let relevant: BTreeMap<_, _> = environment
        .iter()
        .filter(|(key, _)| {
            (key.starts_with("CODEX_")
                || key.starts_with("DISCORD_")
                || matches!(key.as_str(), "HOME" | "USERPROFILE"))
                && !matches!(key.as_str(), "CODEX_THREAD_ID" | "CODEX_SESSION_ID")
        })
        .collect();
    let fingerprint = hex::encode(Sha256::digest(serde_json::to_vec(&relevant)?));
    let snapshot = if backup {
        Some(cdr_store::backup::snapshot(&database)?)
    } else {
        None
    };
    Ok(serde_json::to_string(&json!({
        "protocol": "discord-lover-upgrade-v1", "read_only": !backup,
        "compatible": true, "root": root, "database": database,
        "environment_sha256": hex::encode(Sha256::digest(env_bytes)),
        "effective_environment_sha256": fingerprint,
        "agy_enabled": false, "agy_settings": settings_path,
        "agy_settings_sha256": settings_hash,
        "store_schema": compatibility["store_schema"],
        "admission_authorized": false, "recovery_authorized": false,
        "backup": snapshot
    }))?)
}
