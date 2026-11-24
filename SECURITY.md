# Security policy

## Supported versions

| Version | Supported |
| --- | --- |
| `0.1.x` | Yes |

## Reporting a vulnerability

Do not report security vulnerabilities in public GitHub issues, pull requests, or discussions.

Report vulnerabilities privately through [GitHub Private Vulnerability Reporting](https://github.com/junjamrk/kipo/security/advisories/new) on the `junjamrk/kipo` repository.

Include the following details in your report:

1. Output of `kp --version` and your operating system.
2. Minimal reproduction steps or a self-contained script using synthetic test inputs.
3. Which security boundary failed and what an unauthorized actor or external endpoint can observe.

We acknowledge new reports within 72 hours, share a triage decision within 7 days, and coordinate disclosure once a fix and regression test ship in a tagged release.

## Security model and enforced boundaries

Kipo enforces five technical security boundaries in code:

1. **Read-time provenance semiring non-interference.** Derived facts (`CollisionWarning`, `Synthesis`, `Alias`, `PlaybookBullet`, `SkillVersion`) never copy audience lists or validity windows. Every read path (`kp search`, `kp why`, `kp log`, `kp export`, MCP, Web Inspector, and Community HTTP endpoints) evaluates `(viewer, time)` across the justification graph. Adding, modifying, or withdrawing facts invisible to a viewer produces byte-identical responses for that viewer.
2. **Pre-persistence and pre-egress secret redaction.** `SecretRedactor` scrubs recognized API keys, bearer tokens, cloud credentials, and webhook URLs before writing text to `raw_blobs`, `chunks`, or `facts`, and before sending prompts or embeddings to external HTTP endpoints.
3. **`RawOnly` zero-egress policy.** Setting `KP_MODEL_POLICY=raw_only` or per-record `"model_policy": {"mode": "raw_only"}` blocks cloud embedding and external generative LLM or synthesis requests for those sources.
4. **Loopback and Unix socket isolation.** `kp daemon` places its Unix Domain Socket inside a `0700` user-private directory (`0600` socket) and checks peer UID credentials. `kp inspect` binds exclusively to `127.0.0.1`, exchanges capability tokens into `HttpOnly; SameSite=Strict` cookies, enforces a strict Content Security Policy (`default-src 'none'`, no `'unsafe-inline'`), and exposes zero write routes.
5. **Quarantined community skills and SSRF guards.** Imported skills default to `exec_approved = false` until the local library owner runs `kp skill approve <slug>`. Web URL ingestion (`kp add https://...`) blocks loopback, RFC 1918/6890 private ranges, local domains, DNS rebinding to private IPs, and HTTPS-to-HTTP redirect downgrades.

## Explicit non-goals and operational boundaries

Treat the following mechanisms according to their actual threat model:

* **`PreToolUse` command guards are cooperative guardrails, not an OS sandbox.** `kp hook pre-tool-use` parses structured `CommandGuardSpec` rules and common shell wrappers (`shlex`) to catch accidental execution of forbidden commands in Claude Code and Codex. It does not replace container isolation, kernel sandboxes (`sandbox-exec`, `seccomp`, `landlock`), or database IAM permissions against an adversarial prompt-injected shell.
* **Pattern-based secret redaction is defense in depth.** `SecretRedactor` scrubs known credential formats, high-entropy bearer tokens, and common secret assignments, but cannot detect arbitrary custom passphrases. Rotate any real secret accidentally pasted into an agent session, then run `kp purge` to erase the source and write an HMAC tombstone.
* **Community HTTP servers require TLS termination outside localhost.** `kp serve --community` binds to `127.0.0.1:8080` by default. When exposing a Community Library across machines, place it behind an SSH tunnel or a TLS-terminating reverse proxy.

## Verifying release artifacts

Every official release archive on GitHub Releases includes:

1. `SHA256SUMS`, verified automatically by `scripts/install.sh` and `scripts/install.ps1` before binary staging.
2. Cryptographic GitHub Artifact Attestations generated via Sigstore. Verify any downloaded archive with the GitHub CLI:

```sh
gh attestation verify kipo-aarch64-apple-darwin.tar.gz --repo junjamrk/kipo
```
