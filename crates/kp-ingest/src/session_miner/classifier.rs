use std::collections::{BTreeMap, BTreeSet};

use kp_model::{ContrastKind, SecretRedactor};

use super::{
    human_correction_attribution, scrub_session_privacy, FailureCategory, MinedCandidate,
    PendingFailure, SessionEvent, MAX_EPISODES_PER_SKILL_PER_SESSION,
};
use crate::Result;

pub(super) fn is_timeout_or_interrupt_noise(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("command timed out")
        || lower.contains("operation timed out")
        || lower.contains("user interrupted")
        || lower.contains("cancelled by user")
        || lower.contains("canceled by user")
        || lower.contains("killed by signal 2")
        || lower.contains("sigint")
}

pub(super) fn has_meaningful_failure_signal(output: &str, cmd_hint: Option<&str>) -> bool {
    if output.contains("No output from the command") || is_timeout_or_interrupt_noise(output) {
        return false;
    }
    let lower = output.to_lowercase();
    let has_diag = output.contains("error[E")
        || lower.contains("error: could not compile")
        || lower.contains("test result: failed")
        || lower.contains("panicked at ")
        || lower.contains("implied by `-d warnings`")
        || lower.contains("traceback (most recent call last)")
        || lower.contains("modulenotfounderror")
        || lower.contains("importerror:")
        || lower.contains("assertionerror")
        || lower.contains("error ts")
        || lower.contains("cannot find module")
        || lower.contains("referenceerror:")
        || lower.contains("typeerror:")
        || lower.contains("--- fail:")
        || lower.contains("undefined:")
        || lower.contains("relation \"")
        || lower.contains("migration failed")
        || lower.contains("syntax error")
        || lower.contains("checksum mismatch")
        || lower.contains("permissiondenied")
        || lower.contains("unresolved import")
        || lower.contains("mismatched types")
        || lower.contains("cannot find ")
        || lower.contains("error:");
    if has_diag {
        return true;
    }
    let is_search_probe = cmd_hint.is_some_and(|cmd| {
        let trimmed = cmd.trim_start();
        trimmed.starts_with("grep ")
            || trimmed.starts_with("rg ")
            || trimmed.starts_with("test ")
            || trimmed.starts_with("[ ")
    });
    !is_search_probe && !output.trim().is_empty()
}

pub(super) fn classify_command_output(output: &str, cmd_hint: Option<&str>) -> (bool, bool) {
    if is_timeout_or_interrupt_noise(output) {
        return (false, false);
    }
    let mut saw_zero = false;
    let mut saw_nonzero = false;
    for prefix in [
        "Command exited with code ",
        "Process exited with code ",
        "Exit code: ",
        "exit code: ",
    ] {
        let mut cursor = output;
        while let Some(pos) = cursor.find(prefix) {
            let after = &cursor[pos + prefix.len()..];
            let digits: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '-')
                .collect();
            if let Ok(code) = digits.parse::<i64>() {
                if code == 0 {
                    saw_zero = true;
                } else {
                    saw_nonzero = true;
                }
            }
            cursor = after;
        }
    }

    let explicit_diag_fail = output.contains("error[E")
        || output.contains("error: could not compile")
        || output.contains("test result: FAILED")
        || output.contains("Traceback (most recent call last)")
        || output.contains("ModuleNotFoundError:")
        || output.contains("Cannot find module ")
        || output.contains("error TS")
        || output.contains("--- FAIL:")
        || (output.contains("panicked at ") && output.contains("FAILED"));

    if explicit_diag_fail || (saw_nonzero && has_meaningful_failure_signal(output, cmd_hint)) {
        return (true, false);
    }
    let explicit_ok = saw_zero
        || output.contains("test result: ok.")
        || output.contains("Finished `test` profile")
        || output.contains("Finished `dev` profile")
        || output.contains("Finished `release` profile")
        || (output.contains(" passed") && !output.contains(" failed"))
        || output.contains("Test Suites: ")
            && output.contains("passed")
            && !output.contains("failed")
        || output.contains("PASS\n")
        || output.starts_with("ok  \t");
    (false, explicit_ok && !saw_nonzero)
}

fn classify_failure_category(output: &str) -> FailureCategory {
    let lower = output.to_lowercase();
    if lower.contains("prisma")
        || lower.contains("sqlx")
        || lower.contains("alembic")
        || lower.contains("relation \"")
        || lower.contains("migration failed")
        || lower.contains("pgrst")
    {
        return FailureCategory::DatabaseSchemaAndMigration;
    }
    if lower.contains("modulenotfounderror")
        || lower.contains("importerror:")
        || (lower.contains("traceback (most recent call last)")
            && (lower.contains("typeerror:")
                || lower.contains("attributeerror:")
                || lower.contains("valueerror:")))
    {
        return FailureCategory::PythonImportAndType;
    }
    if output.contains("error TS")
        || lower.contains("cannot find module")
        || lower.contains("tsconfig")
        || lower.contains("referenceerror:")
    {
        return FailureCategory::JsTsModuleAndType;
    }
    if output.contains("--- FAIL:")
        || (lower.contains("undefined:") && lower.contains(".go:"))
        || (lower.contains("cannot use ") && lower.contains(".go:"))
    {
        return FailureCategory::GoCompileAndTest;
    }
    if output.contains("error[E0308]")
        || output.contains("error[E0277]")
        || output.contains("error[E0599]")
        || output.contains("error[E0502]")
        || output.contains("error[E0507]")
        || output.contains("error[E0382]")
        || output.contains("error[E0061]")
        || lower.contains("mismatched types")
        || lower.contains("the trait bound")
        || lower.contains("borrow of moved value")
    {
        return FailureCategory::RustTypeAndBorrow;
    }
    if output.contains("error[E0425]")
        || output.contains("error[E0432]")
        || output.contains("error[E0433]")
        || output.contains("error[E0603]")
        || output.contains("error[E0624]")
        || lower.contains("unresolved import")
        || lower.contains("cannot find ")
        || lower.contains("is private")
    {
        return FailureCategory::RustVisibilityAndCfg;
    }
    if lower.contains("implied by `-d warnings`")
        || lower.contains("clippy::")
        || lower.contains("error: unnecessary use")
        || lower.contains("error: this let-binding has unit value")
        || lower.contains("dead_code")
        || lower.contains("unused import")
    {
        return FailureCategory::ClippyStrictWarnings;
    }
    if output.contains("test result: FAILED")
        || lower.contains("panicked at ")
        || lower.contains("assertion `left == right` failed")
        || lower.contains("assertion failed:")
        || lower.contains("assertionerror")
        || lower.contains("fail ")
    {
        return FailureCategory::TestFailureTargeted;
    }
    FailureCategory::CliCommandRecovery
}

fn is_build_or_test_verification_output(output: &str, cmd_hint: Option<&str>) -> bool {
    if output.contains("test result: ok.")
        || output.contains("Finished `")
        || output.contains("Checking ")
        || output.contains("Compiling ")
        || (output.contains(" passed") && !output.contains(" failed"))
        || output.starts_with("ok  \t")
    {
        return true;
    }
    cmd_hint.is_some_and(|cmd| {
        let lower = cmd.to_lowercase();
        lower.contains("cargo ")
            || lower.contains("-cargo")
            || lower.contains("rustc")
            || lower.contains("pytest")
            || lower.contains("python")
            || lower.contains("uv run")
            || lower.contains("npm ")
            || lower.contains("pnpm ")
            || lower.contains("yarn ")
            || lower.contains("vitest")
            || lower.contains("jest")
            || lower.contains("tsc")
            || lower.contains("go test")
            || lower.contains("go build")
            || lower.contains("prisma ")
            || lower.contains("sqlx ")
            || lower.contains("alembic ")
    })
}

fn select_diagnostic_quote_line(output: &str) -> Option<String> {
    let priorities: &[fn(&str) -> bool] = &[
        |l| l.starts_with("error[E"),
        |l| {
            l.starts_with("error:")
                && !l.starts_with("error: could not compile")
                && !l.starts_with("error: test failed")
        },
        |l| l.contains("panicked at ") || (l.starts_with("test ") && l.ends_with("FAILED")),
        |l| l.starts_with("error: could not compile") || l.starts_with("test result: FAILED"),
        |l| {
            l.contains("error:")
                || l.contains("Error:")
                || l.starts_with("Command exited with code ")
        },
    ];
    for pred in priorities {
        for raw_line in output.lines() {
            let cleaned = sanitize_single_quote_line(raw_line);
            if cleaned.chars().count() >= 8 && pred(&cleaned) {
                return Some(cleaned);
            }
        }
    }
    output
        .lines()
        .map(sanitize_single_quote_line)
        .find(|l| l.chars().count() >= 8)
}

fn sanitize_single_quote_line(line: &str) -> String {
    let collapsed = line
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_chars(&collapsed, 160)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    text.chars().take(max_chars).collect()
}

fn is_benign_non_correction(lower: &str) -> bool {
    const BENIGN_PREFIXES: &[&str] = &[
        "no problem",
        "no worries",
        "no need",
        "no rush",
        "no hurry",
        "don't worry",
        "do not worry",
        "don't mind",
        "never mind",
        "stop worrying",
        "没问题",
        "不用谢",
        "不客气",
        "不用担心",
        "不要紧",
    ];
    BENIGN_PREFIXES.iter().any(|p| lower.starts_with(p))
}

fn is_explicit_human_correction(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.chars().count() > 600
        || kp_retrieve::is_low_signal_ack(trimmed)
    {
        return false;
    }
    let lower = trimmed.to_lowercase();
    if is_benign_non_correction(&lower) {
        return false;
    }
    if crate::signals::is_correction(trimmed) {
        return true;
    }
    let zh_prefixes = [
        "不要",
        "先别",
        "别用",
        "改用",
        "改成",
        "更正",
        "请改为",
        "请不要",
        "严禁",
        "禁止",
        "不是这样",
        "不对，",
        "不对,",
    ];
    if zh_prefixes.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    let en_prefixes = [
        "don't use ",
        "do not use ",
        "don't run ",
        "do not run ",
        "never run ",
        "never use ",
        "stop using ",
        "switch to ",
        "use ",
        "wrong:",
        "correction:",
    ];
    en_prefixes.iter().any(|p| lower.starts_with(p)) && lower.contains("instead")
        || lower.starts_with("don't use ")
        || lower.starts_with("do not use ")
        || lower.starts_with("don't run ")
        || lower.starts_with("do not run ")
        || lower.starts_with("never run ")
        || lower.starts_with("never use ")
        || lower.starts_with("correction:")
}

fn select_correction_quote_line(text: &str) -> Option<String> {
    for line in text.lines() {
        let cleaned = sanitize_single_quote_line(line);
        if cleaned.chars().count() >= 6 && is_explicit_human_correction(&cleaned) {
            return Some(cleaned);
        }
    }
    let fallback = sanitize_single_quote_line(text);
    (fallback.chars().count() >= 6).then_some(fallback)
}

pub(super) fn extract_contrast_candidates(
    events: &[SessionEvent],
    redactor: &SecretRedactor,
) -> Result<Vec<MinedCandidate>> {
    let mut out = Vec::new();
    let mut per_skill_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen_quotes: BTreeSet<(String, String)> = BTreeSet::new();
    let mut pending_failure: Option<PendingFailure> = None;
    let mut last_assistant: Option<(String, String)> = None;

    for event in events {
        match event {
            SessionEvent::AssistantTurn {
                step_label,
                text,
                has_code_mutation,
            } => {
                if *has_code_mutation {
                    if let Some(ref mut pf) = pending_failure {
                        pf.mutated_after_failure = true;
                    }
                }
                if !text.is_empty() {
                    last_assistant = Some((step_label.clone(), truncate_chars(text, 240)));
                }
            }
            SessionEvent::UserTurn { step_label, text } => {
                if let Some((ref prev_step, ref prev_text)) = last_assistant {
                    if is_explicit_human_correction(text) {
                        if let Some(quote) = select_correction_quote_line(text) {
                            let attribution = human_correction_attribution()?;
                            let slug_key = attribution.skill_slug.as_str().to_owned();
                            let count = per_skill_counts.get(&slug_key).copied().unwrap_or(0);
                            if count < MAX_EPISODES_PER_SKILL_PER_SESSION
                                && seen_quotes.insert((slug_key.clone(), quote.clone()))
                            {
                                let prev_clean = sanitize_single_quote_line(prev_text);
                                let snippet = scrub_session_privacy(
                                    &format!(
                                        "[assistant {prev_step}] {prev_clean}\n[user {step_label}] {quote}"
                                    ),
                                    redactor,
                                );
                                let quote_scrubbed = scrub_session_privacy(&quote, redactor);
                                if snippet.contains(&quote_scrubbed) {
                                    *per_skill_counts.entry(slug_key).or_insert(0) += 1;
                                    out.push(MinedCandidate {
                                        signal_kind: ContrastKind::HumanCorrection,
                                        step_ref: step_label.clone(),
                                        before_excerpt: truncate_chars(&prev_clean, 200),
                                        after_excerpt: quote_scrubbed.clone(),
                                        verbatim_quote: quote_scrubbed,
                                        snippet_text: snippet,
                                        attribution,
                                        observed_commands: Vec::new(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            SessionEvent::ToolExecution {
                step_label,
                command_hint,
                output,
                failed,
                succeeded,
                is_code_mutation,
                ..
            } => {
                if *is_code_mutation {
                    if let Some(ref mut pf) = pending_failure {
                        pf.mutated_after_failure = true;
                    }
                }
                if *failed {
                    if let Some(quote) = select_diagnostic_quote_line(output) {
                        let category = classify_failure_category(output);
                        let excerpt = sanitize_single_quote_line(&truncate_chars(output, 260));
                        pending_failure = Some(PendingFailure {
                            step_label: step_label.clone(),
                            category,
                            verbatim_quote: scrub_session_privacy(&quote, redactor),
                            failure_excerpt: scrub_session_privacy(&excerpt, redactor),
                            mutated_after_failure: false,
                            failed_command: command_hint
                                .as_deref()
                                .map(|c| scrub_session_privacy(c, redactor)),
                        });
                    }
                } else if *succeeded {
                    let can_recover = pending_failure.as_ref().is_some_and(|pf| {
                        if pf.category.requires_build_or_test_verification() {
                            pf.mutated_after_failure
                                && is_build_or_test_verification_output(
                                    output,
                                    command_hint.as_deref(),
                                )
                        } else {
                            true
                        }
                    });
                    if can_recover {
                        if let Some(pf) = pending_failure.take() {
                            let attribution = pf.category.to_attribution()?;
                            let slug_key = attribution.skill_slug.as_str().to_owned();
                            let count = per_skill_counts.get(&slug_key).copied().unwrap_or(0);
                            if count < MAX_EPISODES_PER_SKILL_PER_SESSION
                                && seen_quotes.insert((slug_key.clone(), pf.verbatim_quote.clone()))
                            {
                                let ok_line = output
                                    .lines()
                                    .map(sanitize_single_quote_line)
                                    .find(|l| {
                                        l.contains("test result: ok")
                                            || l.contains("Finished ")
                                            || l.contains(" passed")
                                            || l.contains("exited with code 0")
                                            || l.contains("Exit code: 0")
                                    })
                                    .unwrap_or_else(|| {
                                        sanitize_single_quote_line(&truncate_chars(output, 160))
                                    });
                                let ok_scrubbed = scrub_session_privacy(&ok_line, redactor);
                                let snippet = scrub_session_privacy(
                                    &format!(
                                        "[failure {}] {}\n[recovery {}] {}",
                                        pf.step_label, pf.verbatim_quote, step_label, ok_scrubbed
                                    ),
                                    redactor,
                                );
                                let mut observed_commands = Vec::new();
                                if let Some(fc) = pf.failed_command {
                                    if !fc.trim().is_empty() {
                                        observed_commands.push(fc);
                                    }
                                }
                                if let Some(rc) = command_hint.as_deref() {
                                    let rc_clean = scrub_session_privacy(rc, redactor);
                                    if !rc_clean.trim().is_empty()
                                        && !observed_commands.contains(&rc_clean)
                                    {
                                        observed_commands.push(rc_clean);
                                    }
                                }
                                if snippet.contains(&pf.verbatim_quote) {
                                    *per_skill_counts.entry(slug_key).or_insert(0) += 1;
                                    out.push(MinedCandidate {
                                        signal_kind: ContrastKind::FailureToRecovery,
                                        step_ref: format!("{}->{}", pf.step_label, step_label),
                                        before_excerpt: truncate_chars(&pf.failure_excerpt, 200),
                                        after_excerpt: truncate_chars(&ok_scrubbed, 200),
                                        verbatim_quote: pf.verbatim_quote,
                                        snippet_text: snippet,
                                        attribution,
                                        observed_commands,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(out)
}
