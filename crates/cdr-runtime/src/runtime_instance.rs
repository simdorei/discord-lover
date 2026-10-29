use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};

const LOCK_NAME: &str = ".codex_discord_rust.runtime.lock";
const HEARTBEAT_NAME: &str = ".codex_discord_rust.heartbeat";

#[derive(Debug, Error)]
pub enum RuntimeInstanceError {
    #[error("another Codex Discord Remote Rust runtime is already running")]
    AlreadyRunning,
    #[error("could not access runtime marker {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[cfg(windows)]
    #[error("Windows single-instance check failed: {0}")]
    Windows(cdr_windows_native::NativeError),
}

pub struct RuntimeInstanceGuard {
    lock_path: PathBuf,
    _filesystem_lock: fs::File,
    #[cfg(windows)]
    _native: cdr_windows_native::SingleInstance,
    #[cfg(unix)]
    _socket: std::os::unix::net::UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
}

impl RuntimeInstanceGuard {
    pub fn acquire(root: &Path) -> Result<Self, RuntimeInstanceError> {
        fs::create_dir_all(root).map_err(|source| io_error(root, source))?;
        // File locks cover other Windows sessions and canonical path aliases too.
        let exclusive_path = root.join(".codex_discord_rust.exclusive.lock");
        let filesystem_lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&exclusive_path)
            .map_err(|source| io_error(&exclusive_path, source))?;
        filesystem_lock
            .try_lock()
            .map_err(|_| RuntimeInstanceError::AlreadyRunning)?;
        let lock_path = root.join(LOCK_NAME);
        #[cfg(windows)]
        let native = acquire_windows(root)?;
        #[cfg(unix)]
        let socket_path = lock_path.with_extension("socket");
        #[cfg(unix)]
        let socket = acquire_unix(&socket_path)?;
        write_marker(
            &lock_path,
            &format!(
                "pid={}\nstarted_at={}\n",
                std::process::id(),
                unix_seconds()?
            ),
        )?;
        Ok(Self {
            lock_path,
            _filesystem_lock: filesystem_lock,
            #[cfg(windows)]
            _native: native,
            #[cfg(unix)]
            _socket: socket,
            #[cfg(unix)]
            socket_path,
        })
    }

    #[must_use]
    pub fn heartbeat_path(root: &Path) -> PathBuf {
        root.join(HEARTBEAT_NAME)
    }
}

impl Drop for RuntimeInstanceGuard {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.lock_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("runtime_lock_cleanup_failed: {error}");
        }
        #[cfg(unix)]
        if let Err(error) = fs::remove_file(&self.socket_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("runtime_socket_cleanup_failed: {error}");
        }
    }
}

pub async fn run_heartbeat(path: PathBuf, mut shutdown: watch::Receiver<bool>) {
    let marker = HeartbeatMarker { path };
    let mut timer = interval(Duration::from_secs(10));
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = timer.tick() => {
                if let Err(error) = write_marker(&marker.path, &format!("pid={}\nupdated_at={}\n", std::process::id(), unix_seconds().unwrap_or_default())) {
                    eprintln!("runtime_heartbeat_write_failed: {error}");
                }
            }
        }
    }
}

struct HeartbeatMarker {
    path: PathBuf,
}

impl Drop for HeartbeatMarker {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("runtime_heartbeat_cleanup_failed: {error}");
        }
    }
}

#[cfg(windows)]
fn acquire_windows(
    root: &Path,
) -> Result<cdr_windows_native::SingleInstance, RuntimeInstanceError> {
    let name = windows_mutex_name(root);
    cdr_windows_native::SingleInstance::acquire(&name).map_err(|error| match error {
        cdr_windows_native::NativeError::AlreadyRunning => RuntimeInstanceError::AlreadyRunning,
        other => RuntimeInstanceError::Windows(other),
    })
}

#[cfg(windows)]
fn windows_mutex_name(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().to_lowercase().as_bytes());
    format!("Local\\CodexDiscordBot_{}", &hex::encode(digest)[..16])
}

#[cfg(unix)]
fn acquire_unix(path: &Path) -> Result<std::os::unix::net::UnixListener, RuntimeInstanceError> {
    use std::os::unix::net::{UnixListener, UnixStream};

    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(RuntimeInstanceError::AlreadyRunning);
        }
        fs::remove_file(path).map_err(|source| io_error(path, source))?;
    }
    UnixListener::bind(path).map_err(|source| io_error(path, source))
}

fn write_marker(path: &Path, text: &str) -> Result<(), RuntimeInstanceError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
    temporary
        .write_all(text.as_bytes())
        .map_err(|source| io_error(path, source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| io_error(path, source))?;
    temporary
        .persist(path)
        .map_err(|error| io_error(path, error.error))?;
    Ok(())
}

fn unix_seconds() -> Result<u64, RuntimeInstanceError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|source| io_error(Path::new("system-clock"), std::io::Error::other(source)))
}

fn io_error(path: &Path, source: std::io::Error) -> RuntimeInstanceError {
    RuntimeInstanceError::Io {
        path: path.to_owned(),
        source,
    }
}

#[cfg(test)]
#[path = "runtime_instance_tests.rs"]
mod tests;
