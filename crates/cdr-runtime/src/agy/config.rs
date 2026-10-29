use super::{AgyError, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Optional local CLI configuration. No bot token or Google credentials live here.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgyConfig {
    pub executable: PathBuf,
    pub projects: Vec<Project>,
    #[serde(default)]
    pub category_id: Option<u64>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub auto_approve: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub channel_id: Option<u64>,
}

impl AgyConfig {
    pub fn from_environment(
        environment: &BTreeMap<String, String>,
        root: &Path,
    ) -> Result<Option<Self>> {
        let Some(path) = environment
            .get("DISCORD_AGY_CONFIG")
            .filter(|p| !p.trim().is_empty())
        else {
            return Ok(None);
        };
        let path = root.join(path);
        let settings = Settings::read(&path)?;
        // A path is only a location, never permission to launch the optional CLI.
        Ok(settings.enabled.then_some(settings.config))
    }

    pub fn validate(&self) -> Result<()> {
        if self.category_id == Some(0) {
            return Err(AgyError::Invalid("AGY category ID must be nonzero".into()));
        }
        let mut ids = BTreeSet::new();
        if !self.executable.is_absolute() || !self.executable.is_file() {
            return Err(AgyError::Invalid(
                "AGY executable must be an existing absolute path".into(),
            ));
        }
        if self.projects.is_empty() || self.projects.len() > 20 {
            return Err(AgyError::Invalid(
                "AGY requires 1–20 explicitly configured projects".into(),
            ));
        }
        let mut names = BTreeSet::new();
        for project in &self.projects {
            if let Some(id) = project.channel_id
                && (id == 0 || Some(id) == self.category_id || !ids.insert(id))
            {
                return Err(AgyError::Invalid(
                    "AGY project room IDs must be distinct and nonzero".into(),
                ));
            }
            if project.name.is_empty()
                || project.name.len() > 60
                || !project
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                || !names.insert(&project.name)
            {
                return Err(AgyError::Invalid("AGY project names must be unique lowercase letters, numbers or hyphens (1–60 characters)".into()));
            }
            if !project.cwd.is_absolute() || !project.cwd.is_dir() {
                return Err(AgyError::Invalid(format!(
                    "AGY project {} needs an existing absolute workspace path",
                    project.name
                )));
            }
        }
        if self
            .model
            .as_ref()
            .is_some_and(|m| m.trim().is_empty() || m.contains('\0'))
        {
            return Err(AgyError::Invalid(
                "AGY model must be nonempty when specified".into(),
            ));
        }
        Ok(())
    }
}

/// Persisted user intent is independent of observed readiness/authentication.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub revision: u64,
    #[serde(default)]
    pub enabled: bool,
    pub config: AgyConfig,
}

impl Settings {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = read_bounded(path)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        // The unpublished legacy development format never implied persisted On.
        let settings = if value.get("version").is_some() {
            serde_json::from_value::<Self>(value)?
        } else {
            Self {
                version: 1,
                revision: 0,
                enabled: false,
                config: serde_json::from_value(value)?,
            }
        };
        if settings.version != 1 {
            return Err(AgyError::Invalid("Unsupported AGY settings version".into()));
        }
        Ok(settings)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let parent = path
            .parent()
            .ok_or_else(|| AgyError::Invalid("AGY settings need a parent directory".into()))?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.as_file().sync_all()?;
        #[cfg(windows)]
        if path.try_exists()? {
            cdr_windows_native::copy_file_access_rules(path, file.path())?;
        }
        #[cfg(windows)]
        cdr_windows_native::atomic_replace(file.path(), path)?;
        #[cfg(not(windows))]
        std::fs::rename(file.path(), path)?;
        Ok(())
    }
}

pub(super) fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 128 * 1024 {
        return Err(AgyError::Invalid("AGY settings exceed 128 KiB".into()));
    }
    Ok(bytes)
}
