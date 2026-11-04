//! Embedded build input. Never infer the running source from a nearby checkout.
use sha2::{Digest, Sha256};
use std::io;
use std::path::Path;

pub const JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/build-info.json"));

pub fn artifact_sha256(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}
