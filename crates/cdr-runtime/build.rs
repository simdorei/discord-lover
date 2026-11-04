use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CDR_BUILD_SOURCE_STATE");
    println!("cargo:rerun-if-changed=build.rs");
    let source = env::var("CDR_BUILD_SOURCE_STATE").ok();
    let (commit, tree, dirty) = match source.as_deref() {
        None => (None, None, "unknown"),
        Some(input) => {
            let parts: Vec<_> = input.split(':').collect();
            assert!(
                parts.len() == 3
                    && parts[..2].iter().all(|part| part.len() == 40
                        && part
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
                    && matches!(parts[2], "clean" | "dirty"),
                "invalid CDR_BUILD_SOURCE_STATE"
            );
            (Some(parts[0]), Some(parts[1]), parts[2])
        }
    };
    let info = serde_json::json!({
        "schema_version": 1,
        "package_version": env::var("CARGO_PKG_VERSION").expect("package version"),
        "source_commit": commit, "source_tree": tree, "source_state": dirty,
    });
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("build-info.json"),
        info.to_string(),
    )
    .expect("write build provenance");
}
