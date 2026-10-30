use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

pub(super) struct Args {
    pub command: String,
    values: BTreeMap<String, String>,
    pub dry_run: bool,
    pub input_stdin: bool,
    pub input_lines: bool,
    pub files: Vec<String>,
}

impl Args {
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut arguments = arguments.into_iter().map(|arg| {
            arg.into_string()
                .map_err(|_| "admin arguments must be UTF-8".to_owned())
        });
        let command = arguments.next().ok_or("missing admin command")??;
        let allowed: &[&str] = match command.as_str() {
            "setup-discord" => &["--bot-id", "--dry-run", "--input-stdin", "--input-lines"],
            "configure-install" => &["--codex-home", "--codex-exe"],
            "verify-plugin-inventory" => &[
                "--marketplace-inventory",
                "--plugin-inventory",
                "--plugin-manifest",
                "--marketplace-name",
                "--plugin-id",
            ],
            "discover-codex" => &["--codex-exe"],
            "backup-store"
            | "active-queue-count"
            | "retire-automatic-reserve"
            | "inspect-upgrade"
            | "backup-upgrade" => &[],
            "authorize-saved-final" => &["--request-file"],
            "list-threads" => &["--limit"],
            "archive-thread" => &["--thread-id"],
            "inspect-new-first-reply" => &["--database", "--job-id"],
            "recover-writer" | "recover-tools" => &[
                "--thread-id",
                "--channel-id",
                "--owner-user-id",
                "--dry-run",
            ],
            "inspect-async-recovery" => &["--database", "--thread-id", "--dry-run"],
            "check-recovery-compatibility" => &["--database", "--env"],
            "send-attachment" => &[
                "--channel-id",
                "--thread-ref",
                "--work-thread",
                "--content",
                "--content-file",
            ],
            unknown => return Err(format!("unknown admin command: {unknown}")),
        };
        let mut parsed = Self {
            command,
            values: BTreeMap::new(),
            dry_run: false,
            input_stdin: false,
            input_lines: false,
            files: Vec::new(),
        };
        while let Some(arg) = arguments.next() {
            let key = arg?;
            if parsed.command == "send-attachment" {
                if key == "--" {
                    parsed
                        .files
                        .extend(arguments.collect::<Result<Vec<_>, _>>()?);
                    break;
                }
                if !key.starts_with('-') {
                    parsed.files.push(key);
                    continue;
                }
            }
            if key != "--repo-root" && !allowed.contains(&key.as_str()) {
                return Err(format!("unknown option for {}: {key}", parsed.command));
            }
            if parsed.values.contains_key(&key) {
                return Err(format!("duplicate admin option: {key}"));
            }
            let value = match key.as_str() {
                "--dry-run" => {
                    parsed.dry_run = true;
                    String::new()
                }
                "--input-stdin" => {
                    parsed.input_stdin = true;
                    String::new()
                }
                "--input-lines" => {
                    parsed.input_lines = true;
                    String::new()
                }
                _ => arguments
                    .next()
                    .ok_or_else(|| format!("missing value after {key}"))??,
            };
            parsed.values.insert(key, value);
        }
        if usize::from(parsed.dry_run)
            + usize::from(parsed.input_stdin)
            + usize::from(parsed.input_lines)
            > 1
        {
            return Err("--dry-run, --input-stdin and --input-lines cannot be combined".into());
        }
        Ok(parsed)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn required(&self, name: &str) -> Result<&str, String> {
        self.value(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("missing required option {name}"))
    }

    pub fn root(&self) -> Result<PathBuf, String> {
        let current = std::env::current_dir().map_err(|e| e.to_string())?;
        Ok(self
            .value("--repo-root")
            .map_or_else(|| current.clone(), |root| current.join(root)))
    }
}
