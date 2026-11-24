## Why

<!-- State the user-visible problem or goal in 1-3 sentences. Link any issue. -->

## What changed

<!-- Concrete summary of data shapes, boundaries, or CLI behavior changed. -->

## Verification

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy -p kipo --all-targets --no-default-features --locked -- -D warnings`
- [ ] `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- [ ] `KP_OFFLINE=1 KP_DISABLE_MODEL_DOWNLOAD=1 cargo test --workspace --all-features --locked`
- [ ] `python3 scripts/verify-oss-readiness.py`
