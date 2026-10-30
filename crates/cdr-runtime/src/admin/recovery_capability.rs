//! Launch compatibility is a read-only check, never an admission/recovery grant.
use super::Args;
use cdr_store::async_resolution::admission_order;
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::{path::Path, time::Duration};
mod configured;

pub(super) fn run(args: &Args, root: &Path) -> Result<String, String> {
    let path = root.join(args.required("--database")?);
    let environment = configured::check(args, &path)
        .map_err(|error| format!("RECOVERY_COMPATIBILITY_HOLD: {error}"))?;
    inspect(&path, environment.as_deref())
        .map_err(|error| format!("RECOVERY_COMPATIBILITY_HOLD: {error}"))
}

fn inspect(path: &Path, environment: Option<&Path>) -> Result<String, Box<dyn std::error::Error>> {
    // Missing files, unsupported schemas and unreadable requirements are not
    // interpreted as a fresh installation. No open_initialized/migration here.
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(3))?;
    db.execute_batch("PRAGMA query_only=ON; BEGIN")?;
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if schema != cdr_store::schema::LATEST_STORE_SCHEMA_VERSION {
        return Err("unsupported store schema; no migration was attempted".into());
    }
    let table_exists = |name: &str| -> rusqlite::Result<bool> {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?)",
            [name],
            |r| r.get(0),
        )
    };
    let ledger = table_exists("cdr_async_execution_obligations")?;
    let registry = table_exists("cdr_runtime_capability_requirements")?;
    let [
        required,
        required_policy,
        required_consent,
        required_abandonment,
        required_order,
    ] = read_required_formats(&db, registry)?;
    if required > 0 && !ledger {
        return Err("required execution ledger is missing".into());
    }
    let policies = table_exists("cdr_async_recovery_policies")?;
    if required_policy > 0 && !policies {
        return Err("required recovery policy ledger is missing".into());
    }
    if policies {
        if required_policy == 0 {
            return Err("recovery policy schema has no persisted capability".into());
        }
        let unsupported:bool=db.query_row(
            "SELECT EXISTS(SELECT 1 FROM cdr_async_recovery_policies WHERE format_version<1 OR format_version>?)",
            [required_policy],|r|r.get(0))?;
        if unsupported {
            return Err("recovery policy evidence exceeds its persisted capability".into());
        }
    }
    if ledger {
        let (count, minimum, maximum): (i64, Option<i64>, Option<i64>) = db.query_row(
            "SELECT COUNT(*),MIN(format_version),MAX(format_version) FROM cdr_async_execution_obligations",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if count > 0 && (!registry || required == 0) {
            return Err(
                "execution evidence exists without its persisted capability requirement".into(),
            );
        }
        if minimum.is_some_and(|v| v < 1) || maximum.is_some_and(|v| v > required) {
            return Err("execution evidence exceeds the persisted/supported format".into());
        }
    }
    cdr_store::async_resolution::publication::check_compatibility_in(&db, required_consent)?;
    cdr_store::async_resolution::abandonment::check_compatibility_in(&db, required_abandonment)?;
    admission_order::check_compatibility_in(&db, required_order)?;
    Ok(serde_json::to_string(&json!({
        "required_recovery_admission_order_format":required_order,
        "supported_recovery_admission_order_format":admission_order::FORMAT_VERSION,
        "new_requests_release_supported":false,
        "required_recovery_publication_consent_format":required_consent,
        "required_recovery_abandonment_format":required_abandonment,
        "supported_recovery_abandonment_format":cdr_store::async_resolution::abandonment::FORMAT_VERSION,
        "abandonment_apply_supported":false,
        "supported_recovery_publication_consent_format":cdr_store::async_resolution::publication::FORMAT_VERSION,
        "protocol":"cdr-recovery-compatibility-v1", "read_only":true,
        "compatible":true, "database":path.canonicalize()?.to_string_lossy(),
        "configured_environment_verified":environment.is_some(),
        "environment":environment.map(|value|value.to_string_lossy()),
        "store_schema":schema,
        "required_async_resolution_format":required,
        "supported_async_resolution_format":cdr_store::async_resolution::FORMAT_VERSION,
        "required_async_recovery_policy_format":required_policy,
        "supported_async_recovery_policy_format":cdr_store::async_resolution::RECOVERY_POLICY_FORMAT_VERSION,
        "reviewed_policy_installed":cdr_store::async_resolution::reviewed_policy_installed_in(&db)?,
        "special_dispatch_supported":false,
        "admission_authorized":false, "recovery_authorized":false,
        "note":"format compatibility only; maintain the real deployment/control lock through launch"
    }))?)
}

fn read_required_formats(
    db: &Connection,
    registry: bool,
) -> Result<[i64; 5], Box<dyn std::error::Error>> {
    let mut required = 0_i64;
    let mut required_policy = 0_i64;
    let mut required_consent = 0_i64;
    let mut required_abandonment = 0_i64;
    let mut required_order = 0_i64;
    if registry {
        let mut statement = db.prepare("SELECT component,format_version FROM cdr_runtime_capability_requirements ORDER BY component LIMIT 65")?;
        let rows = statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() > 64 {
            return Err("capability requirements exceed supported bound".into());
        }
        for (component, version) in rows {
            if component == cdr_store::async_question::FREE_TEXT_COMPONENT
                && version == cdr_store::async_question::FREE_TEXT_FORMAT_VERSION
            {
                let present: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('cdr_async_questions') WHERE name='answer_text')", [], |r|r.get(0))?;
                if !present {
                    return Err("required free-text answer storage is missing".into());
                }
                continue;
            }
            if component == admission_order::COMPONENT && version == admission_order::FORMAT_VERSION
            {
                required_order = version;
                continue;
            }
            if component == cdr_store::async_resolution::abandonment::COMPONENT
                && version == cdr_store::async_resolution::abandonment::FORMAT_VERSION
            {
                required_abandonment = version;
                continue;
            }
            if component == cdr_store::async_resolution::publication::COMPONENT
                && version == cdr_store::async_resolution::publication::FORMAT_VERSION
            {
                required_consent = version;
                continue;
            }
            if component == cdr_store::async_resolution::RECOVERY_POLICY_COMPONENT
                && (1..=cdr_store::async_resolution::RECOVERY_POLICY_FORMAT_VERSION)
                    .contains(&version)
            {
                required_policy = version;
                continue;
            }
            if component != "async_resolution"
                || !(1..=cdr_store::async_resolution::FORMAT_VERSION).contains(&version)
            {
                return Err(format!(
                    "artifact does not support persisted capability {component} version {version}"
                )
                .into());
            }
            required = version;
        }
    }
    Ok([
        required,
        required_policy,
        required_consent,
        required_abandonment,
        required_order,
    ])
}
