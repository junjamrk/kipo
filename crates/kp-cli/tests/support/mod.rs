use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn run(path: &Path, args: &[&str]) -> Result<Value> {
    success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--path")
            .arg(path)
            .arg("--json")
            .args(args)
            .output()?,
    )
}

pub fn success(output: Output) -> Result<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}
