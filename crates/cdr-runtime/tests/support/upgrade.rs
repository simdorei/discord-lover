#![allow(dead_code)]
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}
pub fn hash(path: &Path) -> String {
    hex::encode_upper(Sha256::digest(fs::read(path).unwrap()))
}
fn fixture() -> &'static Path {
    static BINARY: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &BINARY
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("fixture.exe");
            let output = Command::new("rustc")
                .args(["--edition=2024", "-o"])
                .arg(&path)
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upgrade_runtime.rs"),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (temp, path)
        })
        .1
}
pub struct Install {
    pub temp: tempfile::TempDir,
    pub root: PathBuf,
    pub candidate: PathBuf,
    pub previous: Vec<u8>,
    pub environment: Vec<u8>,
    pub old: Child,
}
impl Install {
    pub fn new(guarded: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("기존 설치");
        fs::create_dir_all(root.join("target/release")).unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        for name in [
            "codex-discord-rust-watchdog.ps1",
            "codex-discord-rust-control.ps1",
            "codex-discord-rust-drain.ps1",
            "codex-discord-tray.ps1",
            "codex-discord-tray-runtime.ps1",
            "codex-discord-tray-restart-runtime.ps1",
        ] {
            fs::copy(repo().join(name), root.join(name)).unwrap();
        }
        for file in fs::read_dir(repo().join("scripts")).unwrap() {
            let file = file.unwrap();
            if file.path().extension().is_some_and(|e| e == "ps1") {
                fs::copy(file.path(), root.join("scripts").join(file.file_name())).unwrap();
            }
        }
        let installed = root.join("target/release/cdr-runtime.exe");
        fs::copy(fixture(), &installed).unwrap();
        let candidate = temp.path().join("candidate.exe");
        fs::copy(fixture(), &candidate).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&candidate)
            .unwrap()
            .write_all(b"candidate identity")
            .unwrap();
        let codex_home = temp.path().join("기존 Codex home");
        fs::create_dir_all(codex_home.join("sessions")).unwrap();
        fs::write(
            codex_home.join("sessions/preserved.jsonl"),
            b"original rollout",
        )
        .unwrap();
        let environment = format!(
            "# preserved settings\r\nDISCORD_BOT_TOKEN=fixture-secret-never-connect\r\nDISCORD_GUILD_ID=42\r\nDISCORD_ALLOWED_USER_IDS=43\r\nDISCORD_ALLOWED_CHANNEL_IDS=44\r\nCODEX_HOME={}\r\n",
            codex_home.display()
        ).into_bytes();
        fs::write(root.join(".env"), &environment).unwrap();
        fs::write(root.join(".codex_discord_runtime"), b"rust\r\n").unwrap();
        fs::create_dir(root.join(".codex-discord-attachments")).unwrap();
        fs::write(
            root.join(".codex-discord-attachments/sentinel"),
            b"attachment bytes",
        )
        .unwrap();
        let db = cdr_store::schema::open_initialized(&root.join("discord_mirror.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE upgrade_preservation(value TEXT); INSERT INTO upgrade_preservation VALUES('대화 기록');").unwrap();
        db.execute_batch(
            "INSERT INTO mirror_projects VALUES ('project', 'existing project', 44, 1.0);
             INSERT INTO mirror_threads VALUES ('thread', 'project', 'existing conversation', 44, 45, 1.0);
             INSERT INTO codex_session_mirror_offsets(codex_thread_id,rollout_path,cursor,updated_at)
             VALUES ('thread', 'preserved.jsonl', 123, 1.0);",
        ).unwrap();
        drop(db);
        if guarded {
            let cap = root.join("capabilities.json");
            fs::write(&cap,serde_json::json!({"protocol":"cdr-artifact-capabilities-v1","artifact_sha256":hash(&installed).to_lowercase(),
                "async_resolution_max_format":1,"async_recovery_policy_max_format":1}).to_string()).unwrap();
            let guard = root.join(".codex_discord_rust.compatibility.json");
            fs::write(&guard,serde_json::json!({"protocol":"cdr-runtime-launch-v1","root":root,"candidate_path":installed,
                "candidate_sha256":hash(&installed).to_lowercase(),"environment_path":root.join(".env"),
                "environment_sha256":hash(&root.join(".env")).to_lowercase(),"database_path":root.join("discord_mirror.sqlite"),
                "capability_path":cap,"capability_sha256":hash(&cap).to_lowercase()}).to_string()).unwrap();
            fs::write(
                root.join(".codex_discord_rust.compatibility.required"),
                hash(&guard).to_lowercase(),
            )
            .unwrap();
        }
        let previous = fs::read(&installed).unwrap();
        let mut old_cmd = Command::new(&installed);
        environment_for(&mut old_cmd);
        let old = old_cmd
            .current_dir(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until(|| root.join(".codex_discord_rust.heartbeat").is_file());
        Self {
            temp,
            root,
            candidate,
            previous,
            environment,
            old,
        }
    }
    pub fn command(&self) -> Command {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(repo().join("update.ps1"))
            .arg("-TargetRoot")
            .arg(&self.root)
            .arg("-CandidatePath")
            .arg(&self.candidate)
            .args([
                "-SkipBuild",
                "-WaitTimeoutSeconds",
                "8",
                "-StartupTimeoutSeconds",
                "15",
            ]);
        environment_for(&mut cmd);
        cmd
    }
    pub fn run(&self) -> Output {
        bounded(self.command())
    }
    pub fn waiting_upgrade(&self) -> Child {
        let out = self.root.join("fixture-updater.out");
        let err = self.root.join("fixture-updater.err");
        let mut command = self.command();
        command
            .stdout(fs::File::create(&out).unwrap())
            .stderr(fs::File::create(&err).unwrap());
        let mut child = command.spawn().unwrap();
        wait_until(|| {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "updater exited before drain: {status}\n{}\n{}",
                    fs::read_to_string(&out).unwrap(),
                    fs::read_to_string(&err).unwrap()
                );
            }
            self.root.join(".codex_discord_rust.drain.prepare").exists()
        });
        child
    }
    pub fn state(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.root.join(".codex_discord_rust.upgrade.json")).unwrap(),
        )
        .unwrap()
    }
    pub fn assert_preserved(&self) {
        assert_eq!(fs::read(self.root.join(".env")).unwrap(), self.environment);
        assert_eq!(
            fs::read(
                self.temp
                    .path()
                    .join("기존 Codex home/sessions/preserved.jsonl")
            )
            .unwrap(),
            b"original rollout"
        );
        assert_eq!(
            fs::read(self.root.join(".codex_discord_runtime")).unwrap(),
            b"rust\r\n"
        );
        assert_eq!(
            fs::read(self.root.join(".codex-discord-attachments/sentinel")).unwrap(),
            b"attachment bytes"
        );
        let db = rusqlite::Connection::open_with_flags(
            self.root.join("discord_mirror.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let value: String = db
            .query_row("SELECT value FROM upgrade_preservation", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "대화 기록");
        let mapping: (i64, i64, i64) = db
            .query_row(
                "SELECT p.discord_channel_id,t.discord_thread_id,o.cursor
                 FROM mirror_projects p JOIN mirror_threads t USING(project_key)
                 JOIN codex_session_mirror_offsets o USING(codex_thread_id)
                 WHERE t.codex_thread_id='thread'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(mapping, (44, 45, 123));
    }
}
impl Drop for Install {
    fn drop(&mut self) {
        let _ = fs::write(self.root.join("fixture-exit"), b"exit");
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.root.join(".codex_discord_rust.runtime.lock").exists()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(100));
        }
        if self.old.try_wait().ok().flatten().is_none() {
            let _ = self.old.kill();
        }
        let _ = self.old.wait();
    }
}
pub fn environment_for(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CODEX_")
            || key.to_string_lossy().starts_with("DISCORD_")
        {
            command.env_remove(key);
        }
    }
    command.env(
        "CDR_UPGRADE_REAL_RUNTIME",
        env!("CARGO_BIN_EXE_cdr-runtime"),
    );
}
pub fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(Instant::now() < deadline, "fixture deadline exceeded");
        thread::sleep(Duration::from_millis(100));
    }
}
pub fn bounded(mut command: Command) -> Output {
    // A surviving Windows grandchild can inherit pipe handles even with its own
    // redirected streams. Files let us observe the updater's exit independently.
    let logs = tempfile::tempdir().unwrap();
    let out_path = logs.path().join("stdout");
    let err_path = logs.path().join("stderr");
    command
        .stdout(fs::File::create(&out_path).unwrap())
        .stderr(fs::File::create(&err_path).unwrap());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_mins(2);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("updater deadline exceeded")
        }
        thread::sleep(Duration::from_millis(100));
    };
    Output {
        status,
        stdout: fs::read(out_path).unwrap(),
        stderr: fs::read(err_path).unwrap(),
    }
}
pub fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
