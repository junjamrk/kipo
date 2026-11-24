//! Pre-persistence and pre-egress credential scrubber (`SecretRedactor`).
//!
//! Architectural note (`docs/ARCHITECTURE.md` security exemption):
//! Core crates remain platform-agnostic in their domain models and storage schemas, but
//! [`SecretRedactor`] intentionally retains concrete token prefixes and webhook signatures
//! (such as `ghp_`, `github_pat_`, `glpat-`, `sk-ant-`, `AKIA`, `xoxb-`, and Slack incoming
//! webhook URLs). These credential patterns are an explicit security-redaction exemption and
//! MUST NEVER be removed, renamed, or weakened for literal platform-name scrubbing, as doing
//! so would allow live secrets in raw inputs or session logs to persist unredacted or egress.

use regex::Regex;
use serde_json::Value;

pub struct SecretRedactor {
    patterns: Vec<Regex>,
}

impl SecretRedactor {
    pub fn new() -> Result<Self, regex::Error> {
        let patterns = [
            r"(?s)-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----(?:.*?-----END (?:[A-Z0-9]+ )*PRIVATE KEY-----|.*$)",
            r"sk-ant-[A-Za-z0-9_-]+", r"sk-[A-Za-z0-9_-]+",
            r"ghp_[A-Za-z0-9_]+", r"gho_[A-Za-z0-9_]+", r"ghu_[A-Za-z0-9_]+",
            r"ghs_[A-Za-z0-9_]+", r"ghr_[A-Za-z0-9_]+", r"github_pat_[A-Za-z0-9_]+",
            r"glpat-[A-Za-z0-9_-]+", r"AKIA[A-Z0-9]{12,}", r"ASIA[A-Z0-9]{12,}",
            r"AIza[A-Za-z0-9_-]+", r"xoxb-[A-Za-z0-9-]+", r"xoxa-[A-Za-z0-9-]+",
            r"xoxp-[A-Za-z0-9-]+", r"xoxr-[A-Za-z0-9-]+", r"xoxs-[A-Za-z0-9-]+",
            r"npm_[A-Za-z0-9]+", r"pypi-[A-Za-z0-9_-]+", r"hf_[A-Za-z0-9]+",
            r"sk_live_[A-Za-z0-9]+", r"rk_live_[A-Za-z0-9]+",
            r"SG\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+", r"SK[0-9a-fA-F]{32}",
            r"https://hooks\.slack\.com/services/[A-Za-z0-9/_-]+",
            r"(?:mfa\.[A-Za-z0-9_-]{20,}|[A-Za-z0-9_-]{23,28}\.[A-Za-z0-9_-]{6}\.[A-Za-z0-9_-]{27,})",
            r"[0-9]{6,12}:[A-Za-z0-9_-]{30,}",
            r"(?i)Bearer\s+[A-Za-z0-9_+/.=-]+",
            r"(?i)https?://[^\s/:@]+:[^\s/@]+@[^\s/]+",
            r#"(?i)(?:api[_-]?key|secret|token|password|passwd)\s*[\"']?\s*[:=：]\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|[^\s,;&，；。]+)"#,
            r"do[pr]_v1_[a-f0-9]{32,}", r"sq0(?:atp|csp)-[A-Za-z0-9_-]+",
            r"NRAK-[A-Z0-9]+", r"hvs\.[A-Za-z0-9_-]+",
        ].into_iter().map(|pattern| Regex::new(&if pattern.starts_with("(?s)") { pattern.to_owned() } else { format!("(?-u:\\b){pattern}") })).collect::<Result<Vec<_>, _>>()?;
        let mut patterns = patterns;
        patterns.push(Regex::new(
            r#"(?:密码|密钥|令牌)\s*[:=：]\s*(?:"[^"\r\n]*"|'[^'\r\n]*'|[^\s,;&，；。]+)"#,
        )?);
        Ok(Self { patterns })
    }

    pub fn redact(&self, text: &str) -> String {
        self.patterns.iter().fold(text.to_owned(), |text, pattern| {
            pattern.replace_all(&text, "[REDACTED]").into_owned()
        })
    }

    /// Redact values and keys structurally, so quoted assignments cannot corrupt JSON.
    pub fn redact_json(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(text)),
            Value::Array(values) => {
                Value::Array(values.iter().map(|v| self.redact_json(v)).collect())
            }
            Value::Object(fields) => Value::Object(
                fields
                    .iter()
                    .map(|(key, value)| {
                        let probe = format!("{key}=credential");
                        let value = if self.redact(&probe) != probe {
                            Value::String("[REDACTED]".into())
                        } else {
                            self.redact_json(value)
                        };
                        (self.redact(key), value)
                    })
                    .collect(),
            ),
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
        }
    }
}

#[cfg(test)]
#[path = "redactor_tests.rs"]
mod tests;
