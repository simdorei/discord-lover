use super::support::{Install, repo, wait_until};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

// Pause only a temporary copy, immediately after the real durable phase write.
// The product has no test hooks; the copied tool is pinned before the run and
// stays byte-identical across interruption and resume.
pub struct PausedUpgrade {
    source: PathBuf,
}
impl PausedUpgrade {
    pub fn at(install: &Install, phase: &str) -> Self {
        let source = install.temp.path().join("updater source");
        fs::create_dir_all(source.join("scripts")).unwrap();
        for directory in ["", "scripts"] {
            for file in fs::read_dir(repo().join(directory)).unwrap() {
                let file = file.unwrap();
                if file.path().extension().is_some_and(|e| e == "ps1") {
                    fs::copy(file.path(), source.join(directory).join(file.file_name())).unwrap();
                }
            }
        }
        let state_path = source.join("scripts/CdrUpgradeState.ps1");
        let state_code = fs::read_to_string(&state_path).unwrap();
        let anchor = "Write-Host ('upgrade_phase='+$Phase)";
        assert_eq!(state_code.matches(anchor).count(), 1);
        let pause = format!(
            "{anchor}\n    if ($Phase -ceq '{phase}' -and [IO.File]::Exists((Join-Path $RepoRoot 'fixture-pause'))) {{\n        [IO.File]::WriteAllText((Join-Path $RepoRoot 'fixture-paused'), $Phase)\n        while ([IO.File]::Exists((Join-Path $RepoRoot 'fixture-pause'))) {{ Start-Sleep -Milliseconds 50 }}\n    }}"
        );
        fs::write(state_path, state_code.replace(anchor, &pause)).unwrap();
        fs::write(install.root.join("fixture-pause"), b"pause").unwrap();
        if phase == "rolling_back" {
            fs::write(install.root.join("fail-readiness"), b"fixture rejection").unwrap();
        }
        let out = install.root.join("pause-updater.out");
        let err = install.root.join("pause-updater.err");
        let mut child = install
            .command_from(&source)
            .stdout(Stdio::from(fs::File::create(&out).unwrap()))
            .stderr(Stdio::from(fs::File::create(&err).unwrap()))
            .spawn()
            .unwrap();
        wait_until(|| {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "updater exited before {phase}: {status}\n{}\n{}",
                    fs::read_to_string(&out).unwrap(),
                    fs::read_to_string(&err).unwrap()
                );
            }
            install.root.join("fixture-paused").is_file()
        });
        assert_eq!(install.state()["Phase"], phase);
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_file(install.root.join("fixture-pause")).unwrap();
        Self { source }
    }
    pub fn resume(&self, install: &Install) -> Command {
        let mut cmd = install.command_from(&self.source);
        cmd.arg("-Resume");
        cmd
    }
}

pub fn starts(install: &Install) -> usize {
    fs::read_to_string(install.root.join("fixture-starts.log"))
        .unwrap()
        .lines()
        .count()
}
