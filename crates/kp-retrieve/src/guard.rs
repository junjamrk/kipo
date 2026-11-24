use std::path::{Component, Path, PathBuf};

use kp_model::{
    fact_action_concept_id, system_time_zone, ActorId, CommunityConcept, ConceptKind, EpochMs,
    Fact, FactAccess, FactId, FactKind, GovernedScope, MemberRole, MembershipRelation, TimeZone,
    TrustTier,
};
use kp_store::{CodeLocus, Store, VisibleGuardCandidate, VisibleGuardPremise};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prompt_budget::{
    format_citation_line, format_fact_kp_ref, strip_turn_tags_and_assistant_lines,
};
use crate::Result;

/// Read-only `PreToolUse` check input evaluated against `Store` without mutating state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardCheckInput {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Value,
    #[serde(default)]
    pub cwd: Option<String>,
    pub viewer: ActorId,
    pub at: EpochMs,
    #[serde(skip, default = "system_time_zone")]
    pub time_zone: TimeZone,
}

/// One matched active hard rule or derived `CollisionWarning` with formatted provenance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardHit {
    pub fact_id: FactId,
    pub kp_ref: String,
    pub formatted_reason: String,
}

/// Read-only verdict returned by [`evaluate_tool_guards`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardVerdict {
    pub hits: Vec<GuardHit>,
}

impl GuardVerdict {
    pub fn is_blocked(&self) -> bool {
        !self.hits.is_empty()
    }

    pub fn matched_fact_ids(&self) -> Vec<FactId> {
        self.hits.iter().map(|h| h.fact_id.clone()).collect()
    }

    pub fn allow_refs(&self) -> Vec<String> {
        self.hits.iter().map(|h| h.kp_ref.clone()).collect()
    }

    pub fn joined_reason(&self) -> String {
        self.hits
            .iter()
            .map(|h| h.formatted_reason.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Normalized file paths and shell command strings extracted from a tool invocation payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTargets {
    pub files: Vec<String>,
    pub commands: Vec<String>,
}

impl ToolTargets {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.commands.is_empty()
    }
}

/// Returns `true` for file-mutating and command-executing tools across Claude Code and Codex.
pub fn is_guarded_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Bash"
            | "Edit"
            | "Write"
            | "MultiEdit"
            | "NotebookEdit"
            | "apply_patch"
            | "exec_command"
            | "shell"
            | "shell_command"
            | "local_shell"
            | "command_execution"
    )
}

/// Extracts target file paths and command invocations from either an outer Hook payload
/// (containing `"tool_input"`) or an inner `tool_input` JSON value directly.
pub fn extract_tool_targets(payload_or_input: &Value) -> ToolTargets {
    let has_nested = payload_or_input
        .get("tool_input")
        .is_some_and(|v| !v.is_null());
    let inner = if has_nested {
        &payload_or_input["tool_input"]
    } else {
        payload_or_input
    };

    let mut files = Vec::new();
    let mut commands = Vec::new();

    let mut push_file = |raw: &str| {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && trimmed != "/dev/null" && !files.iter().any(|f| f == trimmed) {
            files.push(trimmed.to_owned());
        }
    };
    let mut push_cmd = |raw: String| {
        let trimmed = raw.trim();
        if !trimmed.is_empty() && !commands.iter().any(|c| c == trimmed) {
            commands.push(trimmed.to_owned());
        }
    };

    for key in ["file_path", "path", "notebook_path"] {
        if let Some(val) = inner
            .get(key)
            .and_then(Value::as_str)
            .or_else(|| payload_or_input.get(key).and_then(Value::as_str))
        {
            push_file(val);
        }
    }

    for container in if has_nested {
        vec![inner, payload_or_input]
    } else {
        vec![inner]
    } {
        if let Some(edits) = container.get("edits").and_then(Value::as_array) {
            for edit in edits {
                for key in ["file_path", "path"] {
                    if let Some(fp) = edit.get(key).and_then(Value::as_str) {
                        push_file(fp);
                    }
                }
            }
        }
    }

    let mut extract_patch_files = |patch_str: &str| {
        for line in patch_str.lines() {
            for prefix in [
                "*** Update File: ",
                "*** Add File: ",
                "*** Delete File: ",
                "+++ b/",
                "--- a/",
            ] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    push_file(rest);
                }
            }
        }
    };

    if let Some(raw_patch) = inner.as_str() {
        extract_patch_files(raw_patch);
    }
    for patch_key in ["patch", "input"] {
        if let Some(patch) = inner
            .get(patch_key)
            .and_then(Value::as_str)
            .or_else(|| payload_or_input.get(patch_key).and_then(Value::as_str))
        {
            extract_patch_files(patch);
        }
    }

    for key in ["command", "cmd"] {
        let field = inner.get(key).or_else(|| payload_or_input.get(key));
        if let Some(s) = field.and_then(Value::as_str) {
            push_cmd(s.to_owned());
        } else if let Some(arr) = field.and_then(Value::as_array) {
            let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
            if !parts.is_empty() {
                let joined =
                    shlex::try_join(parts.iter().copied()).unwrap_or_else(|_| parts.join(" "));
                push_cmd(joined);
            }
        }
    }

    ToolTargets { files, commands }
}

/// Splits a shell command string into POSIX-unquoted argv segments using [`shlex::Shlex`],
/// breaking on `&&`, `||`, `;`, `|`, and `\n` operator boundaries (including unspaced operators
/// and trailing whitespace fallback on malformed quotes) and recursively splitting `sh`/`bash`/`zsh -c <script>` once.
pub fn split_shell_command_segments(cmd: &str) -> Vec<Vec<String>> {
    split_shell_command_segments_inner(cmd, true)
}

fn split_shell_command_segments_inner(cmd: &str, allow_subshell_recurse: bool) -> Vec<Vec<String>> {
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let preprocessed = preprocess_unquoted_newlines(trimmed);
    let mut shl = shlex::Shlex::new(&preprocessed);
    let mut raw_segments: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for token in shl.by_ref() {
        feed_token_with_operators(&token, &mut raw_segments, &mut current);
    }

    if shl.had_error {
        let tail = find_unclosed_tail(&preprocessed);
        for fallback_tok in tail.split_whitespace() {
            let cleaned = fallback_tok.trim_matches(['\'', '"', '\\']);
            if !cleaned.is_empty() {
                feed_token_with_operators(cleaned, &mut raw_segments, &mut current);
            }
        }
    }

    if !current.is_empty() {
        raw_segments.push(current);
    }

    if !allow_subshell_recurse {
        return raw_segments;
    }

    let mut out = Vec::with_capacity(raw_segments.len());
    for seg in raw_segments {
        if let Some(script) = extract_subshell_c_script(&seg) {
            let nested = split_shell_command_segments_inner(script, false);
            if nested.is_empty() {
                out.push(seg);
            } else {
                out.extend(nested);
            }
        } else {
            out.push(seg);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuoteScanState {
    Normal { at_word_start: bool },
    Single { word_start: usize },
    Double { word_start: usize },
}

/// Converts unquoted, unescaped `\n` / `\r` (and `#` comments at word start) into ` ; `
/// while leaving quoted strings and `\\\n` line continuations untouched for [`shlex::Shlex`].
fn preprocess_unquoted_newlines(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len() + 8);
    let mut chars = cmd.char_indices().peekable();
    let mut state = QuoteScanState::Normal {
        at_word_start: true,
    };

    while let Some((idx, ch)) = chars.next() {
        match state {
            QuoteScanState::Normal { at_word_start } => match ch {
                '\\' => {
                    out.push('\\');
                    if let Some((_, next_ch)) = chars.next() {
                        out.push(next_ch);
                        state = QuoteScanState::Normal {
                            at_word_start: next_ch == '\n' && at_word_start,
                        };
                    }
                }
                '\'' => {
                    out.push('\'');
                    state = QuoteScanState::Single { word_start: idx };
                }
                '"' => {
                    out.push('"');
                    state = QuoteScanState::Double { word_start: idx };
                }
                '#' if at_word_start => {
                    for (_, c2) in chars.by_ref() {
                        if c2 == '\n' {
                            out.push_str(" ; ");
                            break;
                        }
                    }
                    state = QuoteScanState::Normal {
                        at_word_start: true,
                    };
                }
                '\n' | '\r' => {
                    out.push_str(" ; ");
                    state = QuoteScanState::Normal {
                        at_word_start: true,
                    };
                }
                other => {
                    out.push(other);
                    let next_word_start = matches!(other, ' ' | '\t' | ';' | '|' | '&');
                    state = QuoteScanState::Normal {
                        at_word_start: next_word_start,
                    };
                }
            },
            QuoteScanState::Single { word_start } => {
                out.push(ch);
                if ch == '\'' {
                    state = QuoteScanState::Normal {
                        at_word_start: false,
                    };
                } else {
                    state = QuoteScanState::Single { word_start };
                }
            }
            QuoteScanState::Double { word_start } => {
                out.push(ch);
                if ch == '\\' {
                    if let Some((_, next_ch)) = chars.next() {
                        out.push(next_ch);
                    }
                    state = QuoteScanState::Double { word_start };
                } else if ch == '"' {
                    state = QuoteScanState::Normal {
                        at_word_start: false,
                    };
                } else {
                    state = QuoteScanState::Double { word_start };
                }
            }
        }
    }
    out
}

/// Locates the unparsed tail starting from the word that caused [`shlex::Shlex`] `had_error`.
fn find_unclosed_tail(s: &str) -> &str {
    let mut chars = s.char_indices().peekable();
    let mut word_start = 0usize;
    let mut in_word = false;

    while let Some((idx, ch)) = chars.next() {
        match ch {
            ' ' | '\t' | '\n' => {
                in_word = false;
            }
            '\'' => {
                if !in_word {
                    word_start = idx;
                    in_word = true;
                }
                let mut closed = false;
                for (_, c2) in chars.by_ref() {
                    if c2 == '\'' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return &s[word_start..];
                }
            }
            '"' => {
                if !in_word {
                    word_start = idx;
                    in_word = true;
                }
                let mut closed = false;
                while let Some((_, c2)) = chars.next() {
                    if c2 == '\\' {
                        if chars.next().is_none() {
                            return &s[word_start..];
                        }
                    } else if c2 == '"' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return &s[word_start..];
                }
            }
            '\\' => {
                if !in_word {
                    word_start = idx;
                    in_word = true;
                }
                if chars.next().is_none() {
                    return &s[word_start..];
                }
            }
            _ => {
                if !in_word {
                    word_start = idx;
                    in_word = true;
                }
            }
        }
    }
    ""
}

fn find_first_shell_operator(s: &str) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'&' if bytes.get(i + 1) == Some(&b'&') => return Some((i, 2)),
            b'|' if bytes.get(i + 1) == Some(&b'|') => return Some((i, 2)),
            b'|' => return Some((i, 1)),
            b';' => return Some((i, 1)),
            b'\n' => return Some((i, 1)),
            b'\r' => {
                let len = if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                return Some((i, len));
            }
            _ => i += 1,
        }
    }
    None
}

fn feed_token_with_operators(
    token: &str,
    segments: &mut Vec<Vec<String>>,
    current: &mut Vec<String>,
) {
    let Some((first_pos, first_len)) = find_first_shell_operator(token) else {
        if !token.is_empty() {
            current.push(token.to_owned());
        }
        return;
    };

    let had_whitespace = token.chars().any(char::is_whitespace);
    let push_piece = |piece: &str, current: &mut Vec<String>| {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            return;
        }
        if had_whitespace && piece.chars().any(char::is_whitespace) {
            let mut inner_shl = shlex::Shlex::new(trimmed);
            let words: Vec<String> = inner_shl.by_ref().collect();
            if inner_shl.had_error || words.is_empty() {
                current.extend(trimmed.split_whitespace().map(str::to_owned));
            } else {
                current.extend(words);
            }
        } else {
            current.push(piece.to_owned());
        }
    };

    push_piece(&token[..first_pos], current);
    if !current.is_empty() {
        segments.push(std::mem::take(current));
    }

    let mut rest = &token[first_pos + first_len..];
    while let Some((pos, len)) = find_first_shell_operator(rest) {
        push_piece(&rest[..pos], current);
        if !current.is_empty() {
            segments.push(std::mem::take(current));
        }
        rest = &rest[pos + len..];
    }
    push_piece(rest, current);
}

fn is_env_assignment(tok: &str) -> bool {
    let Some((name, _)) = tok.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(idx, b)| (idx > 0 && b.is_ascii_digit()) || b.is_ascii_alphabetic() || b == b'_')
}

fn strip_leading_env_and_wrappers(seg: &[String]) -> &[String] {
    let mut idx = 0usize;
    while idx < seg.len() {
        let tok = seg[idx].as_str();
        if is_env_assignment(tok) || matches!(tok, "env" | "command" | "nohup") {
            idx += 1;
            continue;
        }
        if tok == "sudo" {
            idx += 1;
            while idx < seg.len() && seg[idx].starts_with('-') {
                if matches!(seg[idx].as_str(), "-u" | "-g" | "-C" | "-D" | "-R" | "-T") {
                    idx = (idx + 2).min(seg.len());
                } else {
                    idx += 1;
                }
            }
            continue;
        }
        break;
    }
    &seg[idx..]
}

fn extract_subshell_c_script(seg: &[String]) -> Option<&str> {
    let trimmed = strip_leading_env_and_wrappers(seg);
    let bin = trimmed.first()?;
    let base = Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(bin.as_str());
    if !matches!(base, "sh" | "bash" | "zsh") {
        return None;
    }
    let mut i = 1usize;
    while i < trimmed.len() {
        let arg = trimmed[i].as_str();
        if arg == "-c"
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.len() >= 2
                && arg[1..].bytes().all(|b| b.is_ascii_alphabetic())
                && arg.ends_with('c'))
        {
            return trimmed.get(i + 1).map(String::as_str);
        }
        if !arg.starts_with('-') {
            return None;
        }
        i += 1;
    }
    None
}

/// Evaluates active viewer-visible hard constraints, decisions, guarded negative rules, and derived
/// `CollisionWarning`s against `input` in strictly read-only mode (`&Store`).
pub fn evaluate_tool_guards(store: &Store, input: &GuardCheckInput) -> Result<GuardVerdict> {
    if !is_guarded_tool(&input.tool_name) {
        return Ok(GuardVerdict::default());
    }
    let targets = extract_tool_targets(&input.tool_input);
    if targets.is_empty() {
        return Ok(GuardVerdict::default());
    }

    let candidates = store.visible_guard_candidates(&input.viewer, input.at)?;
    if candidates.is_empty() {
        return Ok(GuardVerdict::default());
    }

    let needs_memberships = candidates.iter().any(|c| {
        matches!(
            c.fact.governed_scope,
            Some(GovernedScope::RelationGroup { .. })
        ) || c.premises.iter().any(|p| {
            matches!(
                p.fact.governed_scope,
                Some(GovernedScope::RelationGroup { .. })
            )
        })
    });
    let memberships: Vec<MembershipRelation> = if needs_memberships {
        store
            .visible_facts_by_kinds(&input.viewer, input.at, &[FactKind::Membership])?
            .into_iter()
            .filter_map(|f| serde_json::from_value::<MembershipRelation>(f.object).ok())
            .collect()
    } else {
        Vec::new()
    };

    let effective_cwd = input
        .cwd
        .as_deref()
        .or_else(|| input.tool_input.get("cwd").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty());

    let shell_segments: Vec<Vec<String>> = targets
        .commands
        .iter()
        .flat_map(|cmd| split_shell_command_segments(cmd))
        .map(|seg| seg.into_iter().map(|t| t.to_lowercase()).collect())
        .collect();

    let owner = store.owner_actor();
    let mut hits = Vec::new();

    for cand in &candidates {
        if cand.fact.trust_tier == TrustTier::ExternalUntrusted || !is_hard_constraint(&cand.fact) {
            continue;
        }
        if !is_authorized_guard(owner, &input.viewer, cand, &memberships) {
            continue;
        }
        if guard_candidate_matches(
            cand,
            effective_cwd,
            &targets.files,
            &targets.commands,
            &shell_segments,
        ) {
            let kp_ref = format_fact_kp_ref(&cand.fact.id);
            let formatted_reason =
                format_guard_reason(cand, &input.viewer, input.at, &input.time_zone, &kp_ref)?;
            hits.push(GuardHit {
                fact_id: cand.fact.id.clone(),
                kp_ref,
                formatted_reason,
            });
        }
    }

    Ok(GuardVerdict { hits })
}

fn scope_applies_to_viewer(
    scope: &GovernedScope,
    viewer: &ActorId,
    memberships: &[MembershipRelation],
    author_is_privileged: bool,
) -> bool {
    match scope {
        GovernedScope::SelfOnly { actor } => actor == viewer,
        GovernedScope::Actors { actors } => actors.contains(viewer),
        GovernedScope::RelationGroup {
            anchor,
            group_concept_id,
        } => {
            anchor == viewer
                || memberships.iter().any(|rel| {
                    &rel.anchor == anchor
                        && &rel.member == viewer
                        && &rel.group_concept_id == group_concept_id
                })
        }
        GovernedScope::Resource { .. } => author_is_privileged,
    }
}

fn is_authorized_guard(
    owner: &ActorId,
    viewer: &ActorId,
    cand: &VisibleGuardCandidate,
    memberships: &[MembershipRelation],
) -> bool {
    let fact = &cand.fact;
    if fact.kind == FactKind::CollisionWarning {
        if let Some(gs) = &fact.governed_scope {
            if !scope_applies_to_viewer(gs, viewer, memberships, true) {
                return false;
            }
        } else {
            for prem in &cand.premises {
                if matches!(
                    prem.fact.kind,
                    FactKind::Constraint | FactKind::Decision | FactKind::PlaybookBullet
                ) {
                    if let Some(p_gs) = &prem.fact.governed_scope {
                        if !scope_applies_to_viewer(p_gs, viewer, memberships, true) {
                            return false;
                        }
                    }
                }
            }
        }
        return true;
    }

    let author_is_privileged = &fact.author == owner
        || fact.source_scope_id.is_some()
        || matches!(
            cand.author_role,
            Some(MemberRole::Owner | MemberRole::Maintainer)
        );
    if let Some(gs) = &fact.governed_scope {
        return scope_applies_to_viewer(gs, viewer, memberships, author_is_privileged);
    }
    author_is_privileged
}

fn is_hard_constraint(fact: &Fact) -> bool {
    if fact.kind == FactKind::CollisionWarning {
        return true;
    }
    if !matches!(
        fact.kind,
        FactKind::Constraint | FactKind::Decision | FactKind::ClosedNegative
    ) {
        return false;
    }
    if fact.object.get("hard_guard").and_then(Value::as_bool) == Some(true)
        || fact.object.get("hard").and_then(Value::as_bool) == Some(true)
        || fact
            .object
            .get("enforcement")
            .and_then(Value::as_str)
            .is_some_and(|m| matches!(m, "ask" | "hard" | "guard"))
        || fact
            .object
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        || fact
            .object
            .get("forbidden_commands")
            .and_then(Value::as_array)
            .is_some_and(|arr| !arr.is_empty())
        || fact
            .object
            .get("required_flags")
            .and_then(Value::as_array)
            .is_some_and(|arr| !arr.is_empty())
        || fact
            .object
            .get("file_path")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    {
        return true;
    }
    if fact.kind == FactKind::ClosedNegative {
        return false;
    }
    if fact.governed_scope.is_some()
        || fact.source_scope_id.is_some()
        || fact_action_concept_id(fact).is_some()
    {
        return true;
    }
    let pred = fact.predicate.to_lowercase();
    if pred.contains("guard") || pred.contains("policy") {
        return true;
    }
    let text = format!(
        "{} {} {}",
        fact.subject,
        fact.object,
        fact.source_anchor
            .as_ref()
            .and_then(|a| a.verbatim_quote.as_deref())
            .unwrap_or("")
    )
    .to_lowercase();
    [
        "禁止",
        "严禁",
        "绝不",
        "不得",
        "不要",
        "暂停",
        "只允许",
        "must not",
        "do not",
        "don't",
        "never ",
        "forbidden",
        "instead of ",
    ]
    .iter()
    .any(|kw| text.contains(kw))
}

fn guard_candidate_matches(
    cand: &VisibleGuardCandidate,
    cwd: Option<&str>,
    file_targets: &[String],
    raw_commands: &[String],
    shell_segments: &[Vec<String>],
) -> bool {
    if single_fact_matches_targets(
        &cand.fact,
        &cand.code_loci,
        &cand.linked_concepts,
        cwd,
        file_targets,
        raw_commands,
        shell_segments,
    ) {
        return true;
    }

    if cand.fact.kind == FactKind::CollisionWarning {
        for prem in &cand.premises {
            if prem.fact.trust_tier == TrustTier::ExternalUntrusted {
                continue;
            }
            if matches!(
                prem.fact.kind,
                FactKind::Constraint
                    | FactKind::Decision
                    | FactKind::ClosedNegative
                    | FactKind::Event
            ) && single_fact_matches_targets(
                &prem.fact,
                &prem.code_loci,
                &cand.linked_concepts,
                cwd,
                file_targets,
                raw_commands,
                shell_segments,
            ) {
                return true;
            }
        }
    }

    false
}

fn single_fact_matches_targets(
    fact: &Fact,
    code_loci: &[CodeLocus],
    linked_concepts: &[CommunityConcept],
    cwd: Option<&str>,
    file_targets: &[String],
    raw_commands: &[String],
    shell_segments: &[Vec<String>],
) -> bool {
    // `code_anchors`: `scoped -> anchored` strictly respected.
    if !code_loci.is_empty() {
        return code_loci.iter().any(|locus| {
            anchor_matches_targets(locus, cwd, file_targets, raw_commands, shell_segments)
        });
    }

    if let Some(GovernedScope::Resource { target }) = &fact.governed_scope {
        let res = target.trim();
        if !res.is_empty() && resource_or_tag_matches(res, cwd, file_targets, shell_segments) {
            return true;
        }
    }

    let subj = fact.subject.trim();
    if !subj.is_empty()
        && (subj.contains('/') || subj.contains('.'))
        && !is_generic_subject_placeholder(subj)
        && any_file_or_segment_matches_path(subj, cwd, file_targets, shell_segments)
    {
        return true;
    }

    for tag in &fact.tags {
        let trimmed = tag.trim();
        if !trimmed.is_empty()
            && (trimmed.contains('/') || trimmed.contains('.') || trimmed.contains(' '))
            && resource_or_tag_matches(trimmed, cwd, file_targets, shell_segments)
        {
            return true;
        }
    }

    for path_key in ["file_path", "target", "resource"] {
        if fact.kind == FactKind::CollisionWarning && path_key == "target" {
            continue;
        }
        if let Some(fp) = fact.object.get(path_key).and_then(Value::as_str) {
            let fp = fp.trim();
            if path_key == "target" && !fp.contains('/') && !fp.contains('.') {
                continue;
            }
            if !fp.is_empty()
                && any_file_or_segment_matches_path(fp, cwd, file_targets, shell_segments)
            {
                return true;
            }
        }
    }

    let explicit_allowed = json_string_list(&fact.object, "allowed_commands");
    let required_flags = json_string_list(&fact.object, "required_flags");
    let mut explicit_forbidden = json_string_list(&fact.object, "forbidden_commands");
    if let Some(cmd_pat) = fact.object.get("command").and_then(Value::as_str) {
        let cmd_pat = cmd_pat.trim();
        if !cmd_pat.is_empty() {
            explicit_forbidden.push(cmd_pat.to_owned());
        }
    }
    if !explicit_forbidden.is_empty() {
        for seg in shell_segments {
            if explicit_allowed
                .iter()
                .any(|allow| segment_head_matches_pattern(seg, allow))
            {
                continue;
            }
            if explicit_forbidden
                .iter()
                .any(|pat| command_pattern_matches_single_segment(pat, seg))
                && (required_flags.is_empty()
                    || !segment_satisfies_required_flags(seg, &required_flags))
            {
                return true;
            }
        }
        if !required_flags.is_empty() {
            return false;
        }
    }

    let corpus = collect_fact_policy_corpus(fact, linked_concepts);
    let corpus_lower = corpus.to_lowercase();

    // 1. Protected file / relative path mentions inside policy text matched strictly on path-component boundaries
    //    (so `lib.rs` matches `src/lib.rs` and `lib.rs`, NEVER `src/stdlib.rs`).
    for file_tok in extract_protected_file_tokens(&corpus) {
        if any_file_or_segment_matches_path(&file_tok, cwd, file_targets, shell_segments) {
            return true;
        }
    }

    if shell_segments.is_empty() {
        return false;
    }

    let inferred_req_flags = extract_without_required_flags(&corpus);
    let filtered_storage: Vec<Vec<String>>;
    let active_segments: &[Vec<String>] = if inferred_req_flags.is_empty() {
        shell_segments
    } else {
        filtered_storage = shell_segments
            .iter()
            .filter(|seg| !segment_satisfies_required_flags(seg, &inferred_req_flags))
            .cloned()
            .collect();
        if filtered_storage.is_empty() {
            return false;
        }
        &filtered_storage
    };

    if matches_nl_prohibitive_cli_rule(&corpus, &explicit_allowed, active_segments) {
        return true;
    }

    // 2. Explicit multi-token CLI command phrases mentioned in policy text.
    const EXPLICIT_CMD_PHRASES: &[&[&str]] = &[
        &["gh", "pr", "merge"],
        &["git", "merge"],
        &["git", "push", "--force"],
        &["git", "push", "-f"],
        &["git", "reset", "--hard"],
        &["git", "clean", "-f"],
        &["rm", "-rf"],
        &["cargo", "publish"],
        &["npm", "publish"],
        &["kubectl", "apply"],
        &["kubectl", "delete"],
        &["terraform", "destroy"],
        &["sqlx", "migrate"],
        &["prisma", "migrate"],
        &["alembic", "upgrade"],
        &["helm", "upgrade"],
    ];
    for seq in EXPLICIT_CMD_PHRASES {
        let phrase = seq.join(" ");
        if corpus_lower.contains(&phrase)
            && active_segments
                .iter()
                .any(|seg| segment_matches_cli_sequence(seg, seq))
        {
            return true;
        }
    }

    // 3. Bilingual developer CLI verb & `CommunityConcept` (`action_concept_id`) alignment.
    let has_git_push_action_concept = linked_concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::Action)
        .any(|c| {
            let lbl = c.canonical_label.to_lowercase();
            (lbl.contains("推送") && !lbl.contains("强推") && !lbl.contains("强制"))
                || (lbl.contains("push") && !lbl.contains("force"))
        });

    let has_deploy_action_concept = linked_concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::Action)
        .any(|c| {
            let lbl = c.canonical_label.to_lowercase();
            lbl.contains("deploy")
                || lbl.contains("rollout")
                || lbl.contains("部署")
                || lbl.contains("上线")
        });

    let has_publish_action_concept = linked_concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::Action)
        .any(|c| {
            let lbl = c.canonical_label.to_lowercase();
            lbl.contains("publish") || lbl.contains("release") || lbl.contains("发版")
        });

    let has_migrate_action_concept = linked_concepts
        .iter()
        .filter(|c| c.kind == ConceptKind::Action)
        .any(|c| {
            let lbl = c.canonical_label.to_lowercase();
            lbl.contains("migrate") || lbl.contains("migration") || lbl.contains("迁移")
        });

    let mentions_force_push = [
        "force push",
        "force-push",
        "push --force",
        "push -f",
        "强推",
        "强制推送",
    ]
    .iter()
    .any(|kw| corpus_lower.contains(kw))
        || ((corpus_lower.contains("push") || corpus_lower.contains("推送"))
            && (corpus_lower.contains("force") || corpus_lower.contains("强制")));

    for seg in active_segments {
        let argv = strip_leading_env_and_wrappers(seg);
        if argv.is_empty() {
            continue;
        }
        if is_merge_segment(argv)
            && ["合并", "合入", "merge"]
                .iter()
                .any(|kw| corpus_lower.contains(kw))
        {
            return true;
        }
        if is_force_push_segment(argv) && mentions_force_push {
            return true;
        }
        if is_git_push_segment(argv)
            && !mentions_force_push
            && (has_git_push_action_concept
                || [
                    "禁止推送",
                    "暂停推送",
                    "不要推送",
                    "停止推送",
                    "不得推送",
                    "no push",
                    "do not push",
                    "don't push",
                ]
                .iter()
                .any(|kw| corpus_lower.contains(kw)))
        {
            return true;
        }
        if is_migrate_segment(argv)
            && (has_migrate_action_concept
                || ["migrate", "migration", "迁移"]
                    .iter()
                    .any(|kw| corpus_lower.contains(kw)))
        {
            return true;
        }
        if is_deploy_segment(argv)
            && (has_deploy_action_concept
                || ["deploy", "rollout", "部署", "上线"]
                    .iter()
                    .any(|kw| corpus_lower.contains(kw)))
        {
            return true;
        }
        if is_publish_segment(argv)
            && (has_publish_action_concept
                || ["publish", "release", "发版"]
                    .iter()
                    .any(|kw| corpus_lower.contains(kw)))
        {
            return true;
        }
        if is_destructive_segment(argv)
            && [
                "reset --hard",
                "clean -f",
                "rm -rf",
                "删除",
                "drop table",
                "truncate",
                "destroy",
            ]
            .iter()
            .any(|kw| corpus_lower.contains(kw))
        {
            return true;
        }
    }

    false
}

fn binary_basename(tok: &str) -> &str {
    Path::new(tok)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tok)
}

/// Extracts the git subcommand from `argv`, skipping global git options (`-C <dir>`, `-c <k=v>`, etc.).
fn git_subcommand(argv: &[String]) -> Option<&str> {
    let bin = binary_basename(argv.first()?.as_str());
    if bin != "git" {
        return None;
    }
    let mut i = 1usize;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if matches!(
            arg,
            "-c" | "-C" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env"
        ) {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(arg);
    }
    None
}

fn segment_matches_cli_sequence(seg: &[String], seq: &[&str]) -> bool {
    let argv = strip_leading_env_and_wrappers(seg);
    if argv.is_empty() || seq.is_empty() {
        return false;
    }
    if seq[0] == "git" && seq.len() >= 2 {
        if git_subcommand(argv) != Some(seq[1]) {
            return false;
        }
        return seq[2..]
            .iter()
            .all(|flag| argv[1..].iter().any(|a| a == flag));
    }
    if argv.len() < seq.len() {
        return false;
    }
    if binary_basename(&argv[0]) == seq[0]
        && argv[1..seq.len()]
            .iter()
            .zip(&seq[1..])
            .all(|(a, b)| a == b)
    {
        return true;
    }
    argv.windows(seq.len())
        .any(|win| win.iter().zip(seq).all(|(a, b)| a == b))
}

fn command_pattern_matches_segments(cmd_pat: &str, shell_segments: &[Vec<String>]) -> bool {
    shell_segments
        .iter()
        .any(|seg| command_pattern_matches_single_segment(cmd_pat, seg))
}

fn command_pattern_matches_single_segment(cmd_pat: &str, seg: &[String]) -> bool {
    let pat_segs = split_shell_command_segments(cmd_pat);
    let Some(first) = pat_segs.first() else {
        return false;
    };
    let lowered: Vec<String> = first.iter().map(|s| s.to_lowercase()).collect();
    let refs: Vec<&str> = lowered.iter().map(String::as_str).collect();
    segment_matches_cli_sequence(seg, &refs)
}

fn segment_head_matches_pattern(seg: &[String], cmd_pat: &str) -> bool {
    let pat_segs = split_shell_command_segments(cmd_pat);
    let Some(first) = pat_segs.first() else {
        return false;
    };
    let pat_argv: Vec<String> = strip_leading_env_and_wrappers(first)
        .iter()
        .map(|s| s.to_lowercase())
        .collect();
    let argv = strip_leading_env_and_wrappers(seg);
    if argv.is_empty() || pat_argv.is_empty() {
        return false;
    }
    if pat_argv[0] == "git" && pat_argv.len() >= 2 {
        if git_subcommand(argv) != Some(pat_argv[1].as_str()) {
            return false;
        }
        return pat_argv[2..]
            .iter()
            .all(|flag| argv[1..].iter().any(|a| a == flag));
    }
    if binary_basename(&argv[0]) != binary_basename(&pat_argv[0]) || argv.len() < pat_argv.len() {
        return false;
    }
    let mut cursor = 1usize;
    for req in &pat_argv[1..] {
        if req.starts_with('-') {
            if !argv[1..]
                .iter()
                .any(|a| a == req || a.starts_with(&format!("{req}=")))
            {
                return false;
            }
        } else {
            let Some(pos) = argv[cursor..].iter().position(|a| a == req) else {
                return false;
            };
            cursor += pos + 1;
        }
    }
    true
}

fn segment_satisfies_required_flags(seg: &[String], required_flags: &[String]) -> bool {
    let argv = strip_leading_env_and_wrappers(seg);
    required_flags.iter().all(|flag| {
        let f = flag.trim().to_lowercase();
        f.is_empty()
            || argv
                .iter()
                .any(|tok| tok == &f || tok.starts_with(&format!("{f}=")))
    })
}

fn json_string_list(obj: &Value, key: &str) -> Vec<String> {
    obj.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn extract_without_required_flags(corpus: &str) -> Vec<String> {
    const WITHOUT_CUES: &[&str] = &["without ", "不带", "未带", "未指定", "缺少", "always pass "];
    let lower = corpus.to_lowercase();
    let mut flags = Vec::new();
    for cue in WITHOUT_CUES {
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find(cue) {
            let after_idx = search_from + rel + cue.len();
            search_from = after_idx;
            let tail = corpus[after_idx..].trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '「')
            });
            let tok = tail
                .split(|c: char| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '`' | '\'' | '"' | '」' | ',' | '.' | ';' | ')' | '，' | '。' | '='
                        )
                })
                .next()
                .unwrap_or("");
            if tok.starts_with('-')
                && tok.len() >= 2
                && tok[1..]
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                && !flags.iter().any(|f: &String| f.eq_ignore_ascii_case(tok))
            {
                flags.push(tok.to_lowercase());
            }
        }
    }
    flags
}

fn is_likely_cli_command_span(span: &str) -> bool {
    let words: Vec<&str> = span.split_whitespace().collect();
    let Some(&first) = words.first() else {
        return false;
    };
    if first.starts_with('-') || first.contains("://") {
        return false;
    }
    let bin = binary_basename(first).to_ascii_lowercase();
    const KNOWN_BINS: &[&str] = &[
        "npm",
        "pnpm",
        "yarn",
        "bun",
        "npx",
        "pip",
        "pip3",
        "uv",
        "poetry",
        "pytest",
        "python",
        "python3",
        "cargo",
        "rustup",
        "rustc",
        "go",
        "git",
        "gh",
        "docker",
        "podman",
        "kubectl",
        "helm",
        "terraform",
        "sqlx",
        "prisma",
        "alembic",
        "diesel",
        "knex",
        "flyway",
        "make",
        "just",
        "bazel",
        "gradle",
        "mvn",
        "curl",
        "wget",
        "rm",
        "chmod",
        "chown",
        "psql",
        "mysql",
        "redis-cli",
        "playwright",
        "vitest",
        "jest",
    ];
    if KNOWN_BINS.contains(&bin.as_str()) {
        return words.len() >= 2 || matches!(bin.as_str(), "yarn" | "npm" | "pip" | "pip3");
    }
    words.len() >= 2
        && bin
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && words[1..]
            .iter()
            .all(|w| w.bytes().all(|b| b.is_ascii_graphic()))
        && words[1..].iter().any(|w| {
            w.starts_with('-')
                || w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        })
}

fn extract_backticked_commands(clause: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = clause;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        let inside = after[..end].trim();
        if is_likely_cli_command_span(inside) {
            out.push(inside.to_owned());
        }
        rest = &after[end + 1..];
    }
    out
}

fn extract_inline_ascii_command_after_cue(clause: &str, cues: &[&str]) -> Vec<String> {
    let lower = clause.to_lowercase();
    let mut out = Vec::new();
    for cue in cues {
        let Some(pos) = lower.find(cue) else {
            continue;
        };
        let tail = clause[pos + cue.len()..].trim_start_matches(|c: char| {
            c.is_whitespace() || matches!(c, ':' | '：' | '"' | '\'' | '`')
        });
        let ascii_tokens: Vec<&str> = tail
            .split_whitespace()
            .map(|t| {
                t.trim_matches(|c: char| {
                    matches!(
                        c,
                        '`' | '\'' | '"' | ',' | '.' | ';' | ')' | '(' | '，' | '。' | '；'
                    )
                })
            })
            .take_while(|t| {
                !t.is_empty()
                    && t.is_ascii()
                    && !matches!(
                        t.to_ascii_lowercase().as_str(),
                        "in" | "on"
                            | "for"
                            | "instead"
                            | "directly"
                            | "without"
                            | "or"
                            | "and"
                            | "to"
                            | "when"
                            | "unless"
                            | "please"
                            | "because"
                    )
            })
            .take(5)
            .collect();
        let joined = ascii_tokens.join(" ");
        if is_likely_cli_command_span(&joined) {
            out.push(joined);
        }
    }
    out
}

fn matches_nl_prohibitive_cli_rule(
    corpus: &str,
    explicit_allowed: &[String],
    active_segments: &[Vec<String>],
) -> bool {
    let (nl_prohibited, nl_allowed) = extract_prohibited_and_allowed_cli_phrases(corpus);
    if nl_prohibited.is_empty() {
        return false;
    }
    for seg in active_segments {
        if explicit_allowed
            .iter()
            .chain(nl_allowed.iter())
            .any(|allow| segment_head_matches_pattern(seg, allow))
        {
            continue;
        }
        if nl_prohibited
            .iter()
            .any(|forbid| segment_head_matches_pattern(seg, forbid))
        {
            return true;
        }
    }
    false
}

fn extract_prohibited_and_allowed_cli_phrases(corpus: &str) -> (Vec<String>, Vec<String>) {
    const PROHIBIT_CUES: &[&str] = &[
        "严禁使用",
        "禁止使用",
        "严禁执行",
        "禁止执行",
        "严禁运行",
        "禁止运行",
        "不要使用",
        "不要用",
        "别用",
        "不要执行",
        "不要运行",
        "不得使用",
        "不得执行",
        "切勿使用",
        "切勿执行",
        "严禁",
        "禁止",
        "绝不",
        "do not use",
        "don't use",
        "do not run",
        "don't run",
        "never run",
        "never use",
        "must not run",
        "must not use",
        "forbid ",
    ];
    const ALLOW_CUES: &[&str] = &[
        "必须改用",
        "请改用",
        "统一改用",
        "改用",
        "必须统一使用",
        "统一使用",
        "必须使用",
        "请使用",
        "应该使用",
        "应当使用",
        "switch to",
        "use ",
        "prefer ",
        "always run",
        "always use",
    ];

    let mut prohibited = Vec::new();
    let mut allowed = Vec::new();

    for raw_clause in corpus.split(['\n', '。', '；', ';', '，', ',']) {
        let clause = raw_clause.trim();
        if clause.is_empty() {
            continue;
        }
        let lower = clause.to_lowercase();
        if let Some(instead_idx) = lower.find("instead of ") {
            let left = &clause[..instead_idx];
            let right = &clause[instead_idx + "instead of ".len()..];
            for cmd in extract_backticked_commands(left)
                .into_iter()
                .chain(extract_inline_ascii_command_after_cue(left, ALLOW_CUES))
            {
                if !allowed.contains(&cmd) {
                    allowed.push(cmd);
                }
            }
            for cmd in extract_backticked_commands(right)
                .into_iter()
                .chain(extract_inline_ascii_command_after_cue(right, &[""]))
            {
                if !prohibited.contains(&cmd) {
                    prohibited.push(cmd);
                }
            }
            continue;
        }

        let has_prohibit = PROHIBIT_CUES.iter().any(|c| lower.contains(c));
        let has_allow = !has_prohibit && ALLOW_CUES.iter().any(|c| lower.contains(c));
        if has_prohibit {
            for cmd in extract_backticked_commands(clause).into_iter().chain(
                extract_inline_ascii_command_after_cue(clause, PROHIBIT_CUES),
            ) {
                if !prohibited.contains(&cmd) {
                    prohibited.push(cmd);
                }
            }
        } else if has_allow {
            for cmd in extract_backticked_commands(clause)
                .into_iter()
                .chain(extract_inline_ascii_command_after_cue(clause, ALLOW_CUES))
            {
                if !allowed.contains(&cmd) {
                    allowed.push(cmd);
                }
            }
        }
    }
    prohibited.retain(|p| !allowed.iter().any(|a| a.eq_ignore_ascii_case(p)));
    (prohibited, allowed)
}

fn is_merge_segment(argv: &[String]) -> bool {
    if git_subcommand(argv) == Some("merge") {
        return true;
    }
    segment_matches_cli_sequence(argv, &["gh", "pr", "merge"])
        || segment_matches_cli_sequence(argv, &["glab", "mr", "merge"])
        || segment_matches_cli_sequence(argv, &["hub", "merge"])
}

fn is_git_push_segment(argv: &[String]) -> bool {
    git_subcommand(argv) == Some("push")
}

fn is_force_push_segment(argv: &[String]) -> bool {
    is_git_push_segment(argv)
        && argv[1..].iter().any(|arg| {
            matches!(arg.as_str(), "--force" | "-f" | "--force-with-lease")
                || (arg.starts_with('+') && arg.len() > 1)
        })
}

fn is_migrate_segment(argv: &[String]) -> bool {
    let bin = binary_basename(&argv[0]);
    bin == "migrate"
        || segment_matches_cli_sequence(argv, &["sqlx", "migrate"])
        || segment_matches_cli_sequence(argv, &["diesel", "migration"])
        || segment_matches_cli_sequence(argv, &["sea-orm-cli", "migrate"])
        || segment_matches_cli_sequence(argv, &["refinery", "migrate"])
        || segment_matches_cli_sequence(argv, &["prisma", "migrate"])
        || segment_matches_cli_sequence(argv, &["prisma", "db", "push"])
        || segment_matches_cli_sequence(argv, &["alembic", "upgrade"])
        || segment_matches_cli_sequence(argv, &["alembic", "downgrade"])
        || segment_matches_cli_sequence(argv, &["flyway", "migrate"])
        || segment_matches_cli_sequence(argv, &["knex", "migrate"])
        || (matches!(bin, "python" | "python3")
            && argv.iter().any(|a| a.ends_with("manage.py"))
            && argv.iter().any(|a| a == "migrate"))
}

fn is_deploy_segment(argv: &[String]) -> bool {
    let bin = binary_basename(&argv[0]);
    bin == "deploy"
        || segment_matches_cli_sequence(argv, &["kubectl", "apply"])
        || segment_matches_cli_sequence(argv, &["kubectl", "rollout"])
        || segment_matches_cli_sequence(argv, &["helm", "upgrade"])
        || segment_matches_cli_sequence(argv, &["helm", "install"])
        || segment_matches_cli_sequence(argv, &["terraform", "apply"])
        || segment_matches_cli_sequence(argv, &["fly", "deploy"])
        || segment_matches_cli_sequence(argv, &["vercel", "deploy"])
}

fn is_publish_segment(argv: &[String]) -> bool {
    segment_matches_cli_sequence(argv, &["cargo", "publish"])
        || segment_matches_cli_sequence(argv, &["npm", "publish"])
        || segment_matches_cli_sequence(argv, &["pnpm", "publish"])
        || segment_matches_cli_sequence(argv, &["yarn", "publish"])
        || segment_matches_cli_sequence(argv, &["gh", "release"])
        || segment_matches_cli_sequence(argv, &["twine", "upload"])
}

fn is_destructive_segment(argv: &[String]) -> bool {
    let bin = binary_basename(&argv[0]);
    if bin == "rm" && argv.len() >= 2 {
        return true;
    }
    if git_subcommand(argv) == Some("reset") && argv.iter().any(|a| a == "--hard") {
        return true;
    }
    if git_subcommand(argv) == Some("clean")
        && argv.iter().any(|a| {
            a == "--force" || (a.starts_with('-') && !a.starts_with("--") && a.contains('f'))
        })
    {
        return true;
    }
    if git_subcommand(argv) == Some("branch")
        && argv
            .iter()
            .any(|a| matches!(a.as_str(), "-d" | "-D" | "--delete"))
    {
        return true;
    }
    if segment_matches_cli_sequence(argv, &["kubectl", "delete"])
        || segment_matches_cli_sequence(argv, &["terraform", "destroy"])
    {
        return true;
    }
    let joined = argv.join(" ");
    joined.contains("drop table") || joined.contains("truncate table")
}

fn normalize_rel_path(raw: &str) -> String {
    let unified = raw.trim().replace('\\', "/");
    unified
        .strip_prefix("./")
        .unwrap_or(&unified)
        .to_lowercase()
}

/// Strict path-component suffix comparison (`src/stdlib.rs` NEVER matches `lib.rs`, while `src/lib.rs` DOES).
fn path_target_matches(candidate: &str, policy_path: &str) -> bool {
    let cand_norm = normalize_rel_path(candidate);
    let pol_norm = normalize_rel_path(policy_path).trim_matches('/').to_owned();
    if cand_norm.is_empty() || pol_norm.is_empty() {
        return false;
    }
    Path::new(&cand_norm).ends_with(Path::new(&pol_norm))
}

fn any_file_or_segment_matches_path(
    policy_path: &str,
    cwd: Option<&str>,
    file_targets: &[String],
    shell_segments: &[Vec<String>],
) -> bool {
    for f in file_targets {
        if path_target_matches(f, policy_path) {
            return true;
        }
        if let Some(base) = cwd {
            let joined = Path::new(base).join(f);
            if path_target_matches(&joined.to_string_lossy(), policy_path) {
                return true;
            }
        }
    }
    for seg in shell_segments {
        for tok in seg {
            if path_target_matches(tok, policy_path) {
                return true;
            }
        }
    }
    false
}

fn resource_or_tag_matches(
    pattern: &str,
    cwd: Option<&str>,
    file_targets: &[String],
    shell_segments: &[Vec<String>],
) -> bool {
    if pattern.contains(' ') {
        return command_pattern_matches_segments(pattern, shell_segments);
    }
    if any_file_or_segment_matches_path(pattern, cwd, file_targets, shell_segments) {
        return true;
    }
    let pat_lc = pattern.to_lowercase();
    shell_segments
        .iter()
        .any(|seg| seg.iter().any(|tok| tok == &pat_lc))
}

fn is_generic_subject_placeholder(subj: &str) -> bool {
    matches!(
        subj.to_lowercase().as_str(),
        "general" | "rule" | "policy" | "constraint" | "note" | "default"
    )
}

fn resolve_contained_path(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let canon_root = root.canonicalize().ok()?;
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        canon_root.join(candidate)
    };
    let mut normalized = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut ancestor = normalized.as_path();
    loop {
        if let Ok(canon_anc) = ancestor.canonicalize() {
            let suffix = normalized.strip_prefix(ancestor).ok()?;
            let resolved = if suffix.as_os_str().is_empty() {
                canon_anc
            } else {
                canon_anc.join(suffix)
            };
            if resolved.starts_with(&canon_root) && resolved != canon_root {
                return Some(resolved);
            }
            return None;
        }
        ancestor = ancestor.parent()?;
    }
}

fn anchor_matches_targets(
    locus: &CodeLocus,
    cwd: Option<&str>,
    file_targets: &[String],
    raw_commands: &[String],
    shell_segments: &[Vec<String>],
) -> bool {
    let root = Path::new(&locus.repo_root);
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let expected = canon_root.join(&locus.file_path);
    let base_cwd = cwd.map(Path::new).unwrap_or(root);

    for file in file_targets {
        let candidate = base_cwd.join(file);
        if let Some(resolved) = resolve_contained_path(root, &candidate) {
            if resolved == expected {
                return true;
            }
        } else if Path::new(file).is_relative()
            && base_cwd == root
            && Path::new(file) == Path::new(&locus.file_path)
        {
            return true;
        }
    }

    let in_repo = cwd.is_none_or(|c| {
        Path::new(c)
            .canonicalize()
            .unwrap_or_else(|_| Path::new(c).to_path_buf())
            == canon_root
    });
    let expected_str = expected.to_string_lossy();
    for cmd in raw_commands {
        if cmd.contains(expected_str.as_ref()) {
            return true;
        }
    }
    if in_repo {
        for seg in shell_segments {
            if seg
                .iter()
                .any(|tok| path_target_matches(tok, &locus.file_path))
            {
                return true;
            }
        }
    }
    false
}

fn collect_value_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => {
            if !s.trim().is_empty() {
                out.push(s.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_value_strings(item, out);
            }
        }
        Value::Object(fields) => {
            for v in fields.values() {
                collect_value_strings(v, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_fact_policy_corpus(fact: &Fact, linked_concepts: &[CommunityConcept]) -> String {
    let mut parts = Vec::new();
    if !fact.subject.trim().is_empty() && !is_generic_subject_placeholder(&fact.subject) {
        parts.push(fact.subject.clone());
    }
    collect_value_strings(&fact.object, &mut parts);
    if let Some(quote) = fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.verbatim_quote.as_deref())
    {
        if !quote.trim().is_empty() {
            parts.push(quote.to_owned());
        }
    }
    for tag in &fact.tags {
        if !tag.trim().is_empty() {
            parts.push(tag.clone());
        }
    }
    if let Some(GovernedScope::Resource { target }) = &fact.governed_scope {
        if !target.trim().is_empty() {
            parts.push(target.clone());
        }
    }
    let direct_ids = kp_model::fact_concept_ids(fact);
    for concept in linked_concepts {
        if (direct_ids.is_empty() || direct_ids.contains(&concept.id))
            && !concept.canonical_label.trim().is_empty()
        {
            parts.push(concept.canonical_label.clone());
        }
    }
    parts.join("\n")
}

fn extract_protected_file_tokens(corpus: &str) -> Vec<String> {
    const PROTECTED_EXTS: &[&str] = &[
        ".sql", ".rs", ".toml", ".yaml", ".yml", ".json", ".lock", ".env", ".sh", ".py", ".ts",
        ".tsx", ".js", ".jsx", ".go", ".md", ".ipynb",
    ];
    let mut tokens = Vec::new();
    for raw in corpus.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '\''
                    | '"'
                    | '“'
                    | '”'
                    | '‘'
                    | '’'
                    | '「'
                    | '」'
                    | '('
                    | ')'
                    | '（'
                    | '）'
                    | '['
                    | ']'
                    | ','
                    | '，'
                    | '。'
                    | ';'
                    | '；'
                    | ':'
                    | '：'
            )
    }) {
        let token = raw.trim_matches('/');
        if token.len() < 3 {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        let has_ext = PROTECTED_EXTS.iter().any(|ext| lower.ends_with(ext));
        let looks_like_rel_path = token.contains('/')
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b));
        if (has_ext || looks_like_rel_path) && !tokens.iter().any(|t| t == token) {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

fn fact_reference_time(fact: &Fact, fallback: EpochMs) -> EpochMs {
    match &fact.access {
        FactAccess::Base { stamp, .. } => {
            if stamp.recorded_at.0 != 0 {
                stamp.recorded_at
            } else if stamp.valid.valid_from.0 != 0 {
                stamp.valid.valid_from
            } else {
                fallback
            }
        }
        FactAccess::Derived { recorded_at, .. } => {
            if recorded_at.0 != 0 {
                *recorded_at
            } else {
                fallback
            }
        }
    }
}

fn extract_fact_quote_or_claim(fact: &Fact, linked_concepts: &[CommunityConcept]) -> String {
    let quote = fact
        .source_anchor
        .as_ref()
        .and_then(|a| a.verbatim_quote.as_deref())
        .map(strip_turn_tags_and_assistant_lines)
        .map(|q| q.trim().to_owned())
        .filter(|q| !q.is_empty());

    if fact.kind == FactKind::Membership {
        if let Some(q) = &quote {
            return format!("「{q}」");
        }
        if let Ok(rel) = serde_json::from_value::<MembershipRelation>(fact.object.clone()) {
            let group_label = linked_concepts
                .iter()
                .find(|c| c.id == rel.group_concept_id)
                .map(|c| strip_turn_tags_and_assistant_lines(&c.canonical_label))
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            return match group_label {
                Some(lbl) => format!("{} 属于 {} 的「{lbl}」", rel.member, rel.anchor),
                None => format!("{} 属于 {} 的关联成员组", rel.member, rel.anchor),
            };
        }
    }

    let raw_value_str = match &fact.object {
        Value::String(s) => s.trim().to_owned(),
        Value::Object(map) => [
            "claim_text",
            "claim",
            "reason",
            "rule",
            "advice",
            "summary",
            "text",
            "message",
            "verbatim_quote",
            "quote",
        ]
        .iter()
        .find_map(|k| map.get(*k).and_then(Value::as_str).map(str::trim))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| fact.object.to_string()),
        other => other.to_string(),
    };
    let value_str = strip_turn_tags_and_assistant_lines(&raw_value_str);

    match quote {
        Some(q)
            if value_str.is_empty()
                || value_str.contains(q.as_str())
                || q.contains(value_str.as_str()) =>
        {
            format!("「{q}」")
        }
        Some(q) => format!("「{q}」 ({value_str})"),
        None if !value_str.is_empty() => value_str,
        None => strip_turn_tags_and_assistant_lines(&fact.subject),
    }
}

fn format_guard_reason(
    cand: &VisibleGuardCandidate,
    viewer: &ActorId,
    at: EpochMs,
    time_zone: &TimeZone,
    kp_ref: &str,
) -> Result<String> {
    let main_body = extract_fact_quote_or_claim(&cand.fact, &cand.linked_concepts);
    let main_review = (viewer == &cand.fact.author)
        .then_some(cand.pending_ambiguous_question.as_deref())
        .flatten();
    let mut lines = vec![format_citation_line(
        fact_reference_time(&cand.fact, at),
        time_zone,
        &cand.fact.author,
        &cand.source_label,
        kp_ref,
        &main_body,
        main_review,
    )?];

    if cand.fact.kind == FactKind::CollisionWarning {
        for prem in &cand.premises {
            lines.push(format_premise_line(
                prem,
                &cand.linked_concepts,
                viewer,
                at,
                time_zone,
            )?);
        }
    }

    Ok(lines.join("\n"))
}

fn format_premise_line(
    prem: &VisibleGuardPremise,
    linked_concepts: &[CommunityConcept],
    viewer: &ActorId,
    at: EpochMs,
    time_zone: &TimeZone,
) -> Result<String> {
    let p_ref = format_fact_kp_ref(&prem.fact.id);
    let p_body = extract_fact_quote_or_claim(&prem.fact, linked_concepts);
    let p_review = (viewer == &prem.fact.author)
        .then_some(prem.pending_ambiguous_question.as_deref())
        .flatten();
    let line = format_citation_line(
        fact_reference_time(&prem.fact, at),
        time_zone,
        &prem.fact.author,
        &prem.source_label,
        &p_ref,
        &p_body,
        p_review,
    )?;
    Ok(format!("  ↳ {line}"))
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
