use super::{
    AgyError, Result,
    blocking::BlockingWorker,
    config::{AgyConfig, Settings},
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

struct State {
    settings: Option<Settings>,
    fingerprint: Option<Vec<u8>>,
    error: Option<String>,
}
/// The lock is on the resolved target's sibling, surviving atomic replacement.
pub(super) struct Configuration {
    pub path: PathBuf,
    _writer: File,
    state: Mutex<State>,
}
impl Configuration {
    pub fn acquire(path: &Path) -> Result<Arc<Self>> {
        let path = if path.try_exists()? {
            path.canonicalize()?
        } else {
            path.parent()
                .ok_or_else(|| AgyError::Invalid("AGY settings path has no parent".into()))?
                .canonicalize()?
                .join(
                    path.file_name()
                        .ok_or_else(|| AgyError::Invalid("AGY settings filename missing".into()))?,
                )
        };
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".cdr-agy-writer.lock");
        let writer = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(PathBuf::from(lock_path))?;
        writer.try_lock().map_err(|_| {
            AgyError::Invalid("다른 브릿지가 같은 AGY 설정 파일을 관리 중입니다.".into())
        })?;
        let fingerprint = fingerprint(&path)?;
        let (settings, error) = if fingerprint.is_some() {
            match Settings::read(&path) {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(e.to_string())),
            }
        } else {
            (None, None)
        };
        Ok(Arc::new(Self {
            path,
            _writer: writer,
            state: Mutex::new(State {
                settings,
                fingerprint,
                error,
            }),
        }))
    }
    pub fn snapshot(&self) -> Result<Option<Settings>> {
        let state = self
            .state
            .lock()
            .map_err(|_| AgyError::Invalid("AGY configuration lock unavailable".into()))?;
        if let Some(error) = &state.error {
            return Err(AgyError::Invalid(error.clone()));
        }
        Ok(state.settings.clone())
    }
    pub fn reload(&self) -> Result<Option<Settings>> {
        let fingerprint = fingerprint(&self.path)?;
        let settings = if fingerprint.is_some() {
            Some(Settings::read(&self.path)?)
        } else {
            None
        };
        *self
            .state
            .lock()
            .map_err(|_| AgyError::Invalid("AGY configuration lock unavailable".into()))? = State {
            settings: settings.clone(),
            fingerprint,
            error: None,
        };
        Ok(settings)
    }
    pub fn save(
        &self,
        expected: u64,
        config: Option<AgyConfig>,
        enabled: Option<bool>,
    ) -> Result<Settings> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AgyError::Invalid("AGY configuration lock unavailable".into()))?;
        let revision = state.settings.as_ref().map_or(0, |s| s.revision);
        if expected != revision || fingerprint(&self.path)? != state.fingerprint {
            return Err(AgyError::Invalid(
                "AGY 설정이 변경되었습니다. 다시 읽은 뒤 저장해 주세요.".into(),
            ));
        }
        let supplied = config.is_some();
        let config = config
            .or_else(|| state.settings.as_ref().map(|s| s.config.clone()))
            .ok_or_else(|| {
                AgyError::Invalid("먼저 AGY 실행 파일과 프로젝트를 설정해 주세요.".into())
            })?;
        // Turning Off remains possible when the executable/workspace was removed.
        if supplied || enabled != Some(false) {
            config.validate()?;
        }
        let settings = Settings {
            version: 1,
            revision: revision
                .checked_add(1)
                .ok_or_else(|| AgyError::Invalid("AGY settings revision exhausted".into()))?,
            enabled: enabled.unwrap_or_else(|| state.settings.as_ref().is_some_and(|s| s.enabled)),
            config,
        };
        settings.write(&self.path)?;
        state.fingerprint = fingerprint(&self.path)?;
        state.settings = Some(settings.clone());
        state.error = None;
        Ok(settings)
    }
    pub async fn save_async(
        self: &Arc<Self>,
        worker: &BlockingWorker,
        expected: u64,
        config: Option<AgyConfig>,
        enabled: Option<bool>,
    ) -> Result<Settings> {
        let writer = Arc::clone(self);
        worker
            .call(move || writer.save(expected, config, enabled))
            .await
    }
}
fn fingerprint(path: &Path) -> Result<Option<Vec<u8>>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let bytes = super::config::read_bounded(path)?;
    if bytes.len() > 128 * 1024 {
        return Err(AgyError::Invalid("AGY settings exceed 128 KiB".into()));
    }
    Ok(Some(Sha256::digest(bytes).to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_target_lock_revision_and_external_edit_are_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("agy.json");
        let first = Configuration::acquire(&path).unwrap();
        assert!(Configuration::acquire(&temp.path().join(".").join("agy.json")).is_err());
        let config = AgyConfig {
            executable: std::env::current_exe().unwrap(),
            projects: vec![super::super::config::Project {
                name: "test".into(),
                cwd: temp.path().into(),
                channel_id: None,
            }],
            category_id: None,
            model: None,
            auto_approve: false,
        };
        let saved = first.save(0, Some(config), Some(false)).unwrap();
        assert_eq!(saved.revision, 1);
        assert!(!saved.enabled);
        assert!(first.save(0, None, Some(true)).is_err());
        std::fs::write(&path, b"external edit").unwrap();
        assert!(first.save(1, None, Some(false)).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"external edit");
    }
}
