# Support

## Documentation

Before opening an issue, check the project guides:

* [README.md](README.md) and [README.zh-CN.md](README.zh-CN.md): installation, quick start, and command reference.
* [TUTORIAL.md](TUTORIAL.md): step-by-step walkthrough from a single personal library to multi-member community sync and `kp dream` skill promotion.
* [docs/INGEST-SCHEMA.md](docs/INGEST-SCHEMA.md): Draft 2020-12 JSON Schema and wire rules for `kp ingest`, MCP `kp_note`, and `kp-connector`.
* [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): crate boundaries, SQLite tables, provenance semiring invariants, and security model.

## Built-in diagnostics

Run these read-only commands to inspect library state, background daemon health, and embedding configuration before filing a bug:

```sh
kp --version
kp status --path ~/.kp/personal --json
kp daemon status --path ~/.kp/personal --json
kp providers list
kp providers test --path ~/.kp/personal --json
```

Include `--json` output (with any personal paths or project names removed) when reporting unexpected behavior.

## Where to ask

* **Reproducible bugs:** open a [Bug Report](https://github.com/junjamrk/kipo/issues/new?template=bug_report.yml).
* **Feature proposals and workflow design:** open a [Feature Request](https://github.com/junjamrk/kipo/issues/new?template=feature_request.yml) or start a [GitHub Discussion](https://github.com/junjamrk/kipo/discussions).
* **Security vulnerabilities:** follow [SECURITY.md](SECURITY.md) and use [GitHub Private Vulnerability Reporting](https://github.com/junjamrk/kipo/security/advisories/new).
