# Contributing to Kipo

Thank you for contributing to Kipo (`kp`). This guide covers local setup, offline verification, architectural boundaries, and pull request conventions.

## Prerequisites

* Rust `1.89` or newer via `rustup` (`stable` channel with `rustfmt` and `clippy`, matching [rust-toolchain.toml](rust-toolchain.toml)).
* Git and a POSIX shell (`sh` or `bash`) on macOS or Linux, or PowerShell on Windows.
* Optional: [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) for license and dependency source checks.

No external API keys, cloud accounts, or pre-downloaded model weights are needed to build or run the test suite.

## Build and test offline

Run the full verification suite from the repository root:

```sh
sh -n scripts/install.sh
cargo fmt --all -- --check
cargo clippy -p kipo --all-targets --no-default-features --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
KP_OFFLINE=1 KP_DISABLE_MODEL_DOWNLOAD=1 cargo test --workspace --all-features --locked
python3 scripts/verify-oss-readiness.py
```

To smoke-test the interactive CLI tour on an isolated temporary directory:

```sh
KP_OFFLINE=1 KP_DISABLE_MODEL_DOWNLOAD=1 cargo run -p kipo --bin kp -- demo
```

## Architectural boundaries

CI enforces five structural invariants in [.github/workflows/ci.yml](.github/workflows/ci.yml):

1. **SQLite isolation.** Only `kp-store` may depend directly on `rusqlite`.
2. **Decoupled external connectors.** `kp-connect` must never depend on `kp-store` or `rusqlite`. External connectors emit standard JSONL records matching [docs/INGEST-SCHEMA.md](docs/INGEST-SCHEMA.md).
3. **Minimal default footprint.** Default `kipo` builds exclude `kp-connect` (`--features connectors` opts in). Builds with `--no-default-features` exclude `fastembed` and `ort`.
4. **Rust safety lints.** The workspace forbids `unsafe` code (`unsafe_code = "forbid"`) and denies `clippy::unwrap_used` and `clippy::expect_used` across all targets. Return typed errors instead of panicking.
5. **Privacy regression guard.** Never commit literal personal home-directory paths, real tokens, or unredacted session transcripts.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the 8-crate dependency graph, SQLite schema, and provenance-semiring rules.

## Extending Kipo

### Add an embedding provider recipe

Provider recipes live as pure data entries in `crates/kp-store/src/providers.rs`. Add the provider metadata (`id`, default endpoint, auth environment variable, default model, supported dimensions, and batch limits), then add an offline wire test in `crates/kp-store/src/providers_tests.rs`.

### Add an external connector

Implement incremental cursor state under `crates/kp-connect/src/` and output validated `PublicIngestLine` JSONL records (`PublicContainerRoster` followed by `PublicIngestRecord`). Scrub text fields with `kp_model::SecretRedactor` before emitting JSONL.

## Pull request workflow

1. Create a focused branch from `main` for one logical change.
2. Add or update behavior-level tests that fail before your fix and pass after it.
3. Title your pull request with Conventional Commits syntax: `feat(scope): subject`, `fix(scope): subject`, `docs(scope): subject`, or `refactor(scope): subject`.
4. Run the offline verification commands listed above before opening the pull request.
5. Keep pull request descriptions under 40 lines using the sections in [.github/PULL_REQUEST_TEMPLATE.md](.github/PULL_REQUEST_TEMPLATE.md).

## AI-assisted contributions

Contributors may use AI coding agents when preparing patches, subject to three rules:

1. You must read, test, and understand every line in your pull request. Do not submit unreviewed agent output.
2. Disclose AI assistance in commit trailers (for example, `Assisted-by: Claude Code` or `Assisted-by: Codex`) when an agent wrote a substantial portion of the change.
3. Verify that generated tests assert observable behavior rather than tautological mocks.

## License

By submitting a contribution, you agree that your work is licensed under the [Apache License, Version 2.0](LICENSE). If you adapt algorithms or prompt structures from permissively licensed open-source projects, record the upstream license and copyright notice in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
