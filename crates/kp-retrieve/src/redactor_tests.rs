use super::*;
use crate::Result;

#[test]
fn all_required_secret_forms_are_masked_without_removing_normal_text() -> Result<()> {
    let redactor = SecretRedactor::new()?;
    let secrets = [
        "sk-secret123",
        "ghp_secret123",
        "github_pat_secret123",
        "AKIA0123456789ABCDEF",
        "xoxb-123-abc",
        "-----BEGIN RSA PRIVATE KEY-----\nsecret\n-----END RSA PRIVATE KEY-----",
        "Bearer eyJhbGci.abc.def",
        "api_key=secret123",
        "api_key=\"two words\"",
    ];
    for secret in secrets {
        assert_eq!(
            redactor.redact(&format!("before {secret} after")),
            "before [REDACTED] after"
        );
    }
    assert_eq!(
        redactor.redact("杭州 weather is fine"),
        "杭州 weather is fine"
    );
    Ok(())
}

#[test]
fn cjk_adjacent_secret_prefixes_are_redacted() -> Result<()> {
    let redactor = SecretRedactor::new()?;
    for (prefix, secret) in [
        ("密钥", "sk-proj-secret1234567890"),
        ("令牌", "ghp_1234567890abcdef"),
        ("令牌", "github_pat_1234567890abcdef"),
        ("秘钥", "AKIA1234567890ABCDEF"),
        ("令牌", "xoxb-1234567890-abcdef"),
        ("凭证", "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig"),
        ("配置", "api_key=supersecret123"),
    ] {
        assert_eq!(
            redactor.redact(&format!("{prefix}{secret} after")),
            format!("{prefix}[REDACTED] after")
        );
    }
    Ok(())
}

#[test]
fn ascii_words_containing_secret_prefixes_are_preserved() -> Result<()> {
    let redactor = SecretRedactor::new()?;
    for text in [
        "disk-1234567890",
        "prefixghp_1234567890abcdef",
        "myapi_key=ordinary",
    ] {
        assert_eq!(redactor.redact(text), text);
    }
    Ok(())
}

#[test]
fn truncated_private_keys_are_redacted_through_the_end_of_the_chunk() -> Result<()> {
    let redactor = SecretRedactor::new()?;
    for kind in [
        "PRIVATE KEY",
        "RSA PRIVATE KEY",
        "EC PRIVATE KEY",
        "ENCRYPTED PRIVATE KEY",
    ] {
        for body in ["", "\nsecret123\nmore secret material\n"] {
            assert_eq!(
                redactor.redact(&format!("before -----BEGIN {kind}-----{body}")),
                "before [REDACTED]"
            );
        }
    }
    assert_eq!(
        redactor.redact("before -----BEGIN PRIVATE KEY-----secret-----END PRIVATE KEY----- between -----BEGIN RSA PRIVATE KEY-----truncated"),
        "before [REDACTED] between [REDACTED]"
    );
    Ok(())
}
