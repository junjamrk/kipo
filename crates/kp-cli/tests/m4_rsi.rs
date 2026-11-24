mod support;

use std::process::Command;

use support::{run, Result};

#[test]
fn cli_records_dreams_promotes_and_rolls_back_independent_feedback() -> Result {
    let dir = tempfile::tempdir()?;
    run(dir.path(), &["init", "--owner", "alice"])?;
    let record = |session| {
        run(
            dir.path(),
            &[
                "episode",
                "record",
                "--kind",
                "human-correction",
                "--before",
                "Used unchecked retries. token=SECRET_EPISODE",
                "--after",
                "No, use a bounded retry and verify the result.",
                "--step-ref",
                "tool:3",
                "--session",
                session,
            ],
        )
    };
    assert_eq!(record("episode-1")?["episodes"], 1);
    let first = run(dir.path(), &["dream"])?;
    assert_eq!(first["bullets_created"], 1);
    assert!(first["promoted"].as_array().ok_or("promoted")?.is_empty());
    assert_eq!(record("episode-1")?["episodes"], 0);
    assert_eq!(run(dir.path(), &["dream"])?["processed"], 0);
    assert_eq!(record("episode-2")?["episodes"], 1);
    assert_eq!(
        run(dir.path(), &["dream"])?["promoted"]
            .as_array()
            .ok_or("promoted")?
            .len(),
        1
    );
    let skills = run(dir.path(), &["skill", "list"])?;
    assert_eq!(skills[0]["name"], "apply-explicit-correction");
    assert_eq!(skills[0]["exec_approved"], true);
    let path = dir.path().join("skills/apply-explicit-correction/SKILL.md");
    let text = std::fs::read_to_string(&path)?;
    assert!(!text.contains("SECRET_EPISODE") && !text.contains("Used unchecked retries"));
    run(dir.path(), &["member", "add", "bob"])?;
    let denied = Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(dir.path())
        .args([
            "--as",
            "bob",
            "--json",
            "skill",
            "approve",
            "apply-explicit-correction",
        ])
        .output()?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("permission denied"));
    assert_eq!(
        run(
            dir.path(),
            &["skill", "approve", "apply-explicit-correction"]
        )?["exec_approved"],
        true
    );
    let feedback = |task| {
        run(
            dir.path(),
            &[
                "feedback",
                "apply-explicit-correction",
                "--outcome",
                "hurt",
                "--task",
                task,
                "--model-family",
                "test",
            ],
        )
    };
    assert_eq!(feedback("task-1")?["counts"]["hurt"], 1);
    assert_eq!(feedback("task-1")?["recorded"], false);
    assert_eq!(feedback("task-2")?["rolled_back"], true);
    assert!(!path.exists());
    assert!(run(dir.path(), &["skill", "list"])?
        .as_array()
        .ok_or("skills")?
        .is_empty());
    assert!(run(dir.path(), &["dream"])?["promoted"]
        .as_array()
        .ok_or("promoted")?
        .is_empty());
    Ok(())
}

#[test]
fn cli_no_entry_keep_base_cloud_gate_and_slug_validation() -> Result {
    let dir = tempfile::tempdir()?;
    run(dir.path(), &["init"])?;
    for trace in ["", "same successful answer"] {
        assert_eq!(
            run(
                dir.path(),
                &[
                    "episode",
                    "record",
                    "--kind",
                    "human-correction",
                    "--before",
                    trace,
                    "--after",
                    trace
                ]
            )?["signal"],
            "NO_ENTRY"
        );
    }
    run(
        dir.path(),
        &[
            "episode",
            "record",
            "--kind",
            "human-correction",
            "--before",
            "a noisy turn",
            "--after",
            "thanks",
        ],
    )?;
    assert_eq!(run(dir.path(), &["dream"])?["kept_base"], 1);
    assert!(run(dir.path(), &["skill", "list"])?
        .as_array()
        .ok_or("skills")?
        .is_empty());
    for args in [
        vec![
            "dream",
            "--llm-url",
            "https://example.com/dream",
            "--llm-model",
            "model",
        ],
        vec!["skill", "approve", "../outside"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(dir.path())
            .args(args)
            .output()?;
        assert!(!output.status.success());
    }
    assert!(!dir.path().join("outside").exists());
    Ok(())
}
