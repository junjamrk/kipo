use super::SecretRedactor;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

#[test]
fn credential_catalog_covers_all_m2_families() -> Result {
    let redactor = SecretRedactor::new()?;
    let synthetic_twilio_key = ["SK", "0123456789abcdef", "0123456789abcdef"].concat();
    for credential in [
        "sk-ant-secret",
        "sk-secret",
        "ghp_secret",
        "gho_secret",
        "ghu_secret",
        "ghs_secret",
        "ghr_secret",
        "github_pat_secret",
        "glpat-secret",
        "AKIA0123456789ABCDEF",
        "ASIA0123456789ABCDEF",
        "AIzaSecret",
        "xoxb-secret",
        "xoxa-secret",
        "xoxp-secret",
        "xoxr-secret",
        "xoxs-secret",
        "npm_secret",
        "pypi-secret",
        "hf_secret",
        "sk_live_secret",
        "rk_live_secret",
        "SG.secret.token",
        synthetic_twilio_key.as_str(),
        "https://hooks.slack.com/services/T123/B456/secret",
        "mfa.abcdefghijklmnopqrstuvwxyz",
        "123456789:abcdefghijklmnopqrstuvwxyz0123456789",
        "Bearer eyJhbGci.abc.sig",
        "https://user:password@example.com",
        "api_key=secret",
        "secret=abc",
        "token=abc",
        "password=abc",
        "密码：abc",
        "密钥=abc",
        "令牌=abc",
        "-----BEGIN PRIVATE KEY-----\ntruncated",
    ] {
        assert_eq!(redactor.redact(credential), "[REDACTED]", "{credential}");
    }
    Ok(())
}

#[test]
fn structured_redaction_preserves_valid_json_and_masks_assignment_keys() -> Result {
    let redactor = SecretRedactor::new()?;
    let result = redactor.redact_json(&serde_json::json!({"api_key":"secret","nested":[{"password":"two words","text":"上海 gho_hidden"}]}));
    assert_eq!(
        result,
        serde_json::json!({"api_key":"[REDACTED]","nested":[{"password":"[REDACTED]","text":"上海 [REDACTED]"}]})
    );
    Ok(())
}
