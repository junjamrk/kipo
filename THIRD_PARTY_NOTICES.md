# Third-Party Notices & License Attributions

**Kipo** (`kp`) incorporates adapted algorithms, prompt designs, schema contracts, and sync patterns from permissively licensed open-source software (Apache-2.0 and MIT). All adapted components are rewritten in 100% safe Rust without C FFI, and original copyright notices and Apache-2.0 `NOTICE` attributions are preserved below.

---

## 1. Adapted Open-Source Components & Prompt Designs

### 1.1 Graphiti (`getzep/graphiti`)
- **Upstream Repository**: <https://github.com/getzep/graphiti>
- **License**: Apache License, Version 2.0
- **Copyright & Notice**:
  > Copyright 2024–2026 Zep Software, Inc.
  >
  > Licensed under the Apache License, Version 2.0 (the "License").
  > Adapted concepts in `kp` (`kp-derive`, `kp-model`): temporal edge validity extraction (`valid_at` / `invalid_at` anchored to `reference_time`, where future plans take statement time rather than event date and soft deadlines never populate `invalid_at`), contradiction identification (`contradicted_facts`), node deduplication resolution (`NodeResolutions`), and 3-gram MinHash/LSH Jaccard deduplication helpers.

### 1.2 Mem0 (`mem0ai/mem0`)
- **Upstream Repository**: <https://github.com/mem0ai/mem0>
- **License**: Apache License, Version 2.0
- **Copyright & Notice**:
  > Copyright 2023–2026 Mem0 AI (Taranjeet Singh et al.)
  >
  > Licensed under the Apache License, Version 2.0 (the "License").
  > Adapted concepts in `kp` (`kp-derive`, `kp-model`): additive memory extraction prompt framing (observation date vs. current date, recent-memory deduplication context) and structured write-operation decision semantics (`Add` / `SupersedeOwn` / `None`).

### 1.3 claude-mem (`thedotmack/claude-mem` — `main` branch Apache-2.0 only)
- **Upstream Repository**: <https://github.com/thedotmack/claude-mem> (`main` branch post-relicensing to Apache-2.0; AGPL-3.0 tags `v5.0.0`–`v10.0.0` were strictly excluded and never consulted or copied)
- **License**: Apache License, Version 2.0
- **Copyright & Notice**:
  > Copyright 2025–2026 Alex Newman (`thedotmack`) and contributors.
  >
  > Licensed under the Apache License, Version 2.0 (the "License").
  > Adapted concepts in `kp` (`kp-ingest`, `kp-derive`): non-blocking `"async": true` configuration for capture hooks (`PostToolUse`, `Stop`, `SessionEnd`), unconditional `exit 0` hook safety invariant so agent turns are never blocked, child subprocess isolation and environment sanitization (stripping `CLAUDE_CODE_*` / `CLAUDECODE`, disabling recursive hooks/MCP/tools, isolated working directory), and the `<skip_summary/>` empty-turn sentinel.

### 1.4 Onyx (`onyx-dot-app/onyx` — MIT modules outside `ee/` only)
- **Upstream Repository**: <https://github.com/onyx-dot-app/onyx> (strictly restricted to MIT-licensed modules outside `backend/ee/**`; proprietary `backend/ee/**` code was never consulted or copied)
- **License**: MIT License
- **Copyright**: Copyright (c) 2023–2026 Danswer, Inc. (Onyx)
- **Adapted Scope in `kp` (`kp-connect`, `kp-model`)**: external container ACL ceiling modeling (`ExternalAccess` / `DocumentAccess` "audience follows the source" upper-bound semantics and cross-source group namespace prefixing in `backend/onyx/access/{models.py, utils.py}`) and GitHub connector rate-limit header backoff patterns. Derived-fact semiring intersection and live `source_scopes` permission evaluation are original to `kp`.

### 1.5 LightRAG (`HKUDS/LightRAG`)
- **Upstream Repository**: <https://github.com/HKUDS/LightRAG>
- **License**: MIT License
- **Copyright**: Copyright (c) 2024–2026 Data Intelligence Lab @ University of Hong Kong (HKUDS)
- **Adapted Scope in `kp` (`kp-derive`)**: document entity/relationship extraction prompt organization and iterative gleaning continuation structure for long-form technical documents and meeting transcripts.

### 1.6 RAGFlow (`infiniflow/ragflow`)
- **Upstream Repository**: <https://github.com/infiniflow/ragflow>
- **License**: Apache License, Version 2.0
- **Copyright & Notice**:
  > Copyright 2024–2026 InfiniFlow, Inc.
  >
  > Licensed under the Apache License, Version 2.0 (the "License").
  > Adapted concepts in `kp` (`kp-connect`): GitHub connector `since`-window incremental polling and resumable cursor checkpoint progression.

### 1.7 WeKnora (`Tencent/WeKnora`)
- **Upstream Repository**: <https://github.com/Tencent/WeKnora>
- **License**: MIT License
- **Copyright**: Copyright (c) 2025–2026 Tencent
- **Adapted Scope in `kp` (`kp-connect`)**: Feishu/Lark incremental connector checkpointing, deletion reconciliation, wiki/drive `obj_edit_time` high-water tracking, and Feishu error code `99991400` rate-limit cooldown/backoff mechanics.

### 1.8 Lark Suite CLI (`larksuite/cli`)
- **Upstream Repository**: <https://github.com/larksuite/cli>
- **License**: MIT License
- **Copyright**: Copyright (c) 2024–2026 Lark Suite Contributors
- **Adapted Scope in `kp` (`kp-connect`)**: read-only `lark-cli` command invocation patterns and Feishu message, chat member, wiki, drive permission, and event payload shapes.

### 1.9 Additional Permissively Adapted Algorithms
- **lemmalog** (MIT License): Semi-naive positive Datalog fixpoint and distinct-support evaluation patterns adapted in `kp-derive`.
- **SAG-Wiki** (MIT License): Greedy Gram-Schmidt DPP result diversification, query clue expansion, UTF-8/chardetng document decoding precedence, and bounded Office XML/ZIP extraction patterns adapted in `kp-retrieve` and `kp-ingest`.
- **harness2** (Apache License 2.0): Sleep-time contrastive attribution prompt framing (`INV` / `DOM` / `KEEP_BASE`) and read-only skill quarantine rules adapted in `kp-model` and `kp-ingest`.
- **omnigraph-policy** (MIT License): Server-authoritative principal and typed resource authorization structure adapted in `kp-net`.

### 1.10 GBrain (`garrytan/gbrain`)
- **Upstream Repository**: <https://github.com/garrytan/gbrain>
- **License**: MIT License
- **Copyright**: Copyright (c) 2025–2026 Garry Tan and contributors
- **Adapted Scope in `kp` (`kp-store`, `kp-retrieve`, `kipo`)**: Pure-data multi-provider embedding Recipe registry (`id`, `implementation`, `base_url_default`, `auth_env`, `default_model`, `default_dims`, `model_dims`, `dims_options`, `max_input_tokens`, `max_batch_tokens`, `max_batch_items`, `chars_per_token`, explicit-only local server opt-in), per-provider dimension option resolution (`dims.ts`), degenerate zero-norm/non-finite vector screening (`embedding-guard.ts`), and single-dimension library metadata locking.

### 1.11 Key Permissively Licensed Rust Crate Dependencies
- **`fastembed` 7.8.0** (`Apache-2.0`, <https://github.com/Anush008/fastembed-rs>): Safe Rust local text embedding tokenization, pooling, and user-defined ONNX session orchestration (`ort-load-dynamic` + `fancy-regex`) in `kp-store`.
- **`ort` 2.0.0-rc.15** (`MIT OR Apache-2.0`, <https://github.com/pykeio/ort>): Safe Rust dynamic runtime bindings (`load-dynamic`) for Microsoft ONNX Runtime (`MIT`, <https://github.com/microsoft/onnxruntime>) in `kp-store`.
- **`tokenizers` 0.23.2** (`Apache-2.0`, <https://github.com/huggingface/tokenizers>): Pure-Rust WordPiece / Unigram / BPE tokenization (`fancy-regex`) used by `fastembed`.
- **`jiff` 0.2.46** (`MIT OR Unlicense`, <https://github.com/BurntSushi/jiff>): Civil calendar arithmetic, ISO-8601 / RFC 3339 timestamp parsing, and IANA time zone / DST midnight boundary resolution in `kp-model`.
- **`shlex` 2.0.1** (`MIT OR Apache-2.0`, <https://github.com/comex/rust-shlex>): POSIX shell tokenizing and operator segment splitting for read-only `PreToolUse` command guard evaluation in `kp-retrieve`.
- **`rustix` 1.1.6** (`Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`, <https://github.com/bytecodealliance/rustix>): Safe Rust POSIX process credential inspection (`geteuid`) for private `0700` `kp daemon` UDS runtime directories in `kp-net`.
- **`toml_edit` 0.25** (`MIT OR Apache-2.0`, <https://github.com/toml-rs/toml>): Format-preserving TOML configuration editing for `.codex/config.toml` MCP registration in `kp-ingest`.
- **`webpki-roots` 1.0** (`CDLA-Permissive-2.0`, <https://github.com/rustls/webpki-roots>): Mozilla root CA certificate data bundle used transitively by `reqwest` (`rustls-tls`) for outbound HTTPS verification.

---

## 2. License Texts

### 2.1 Apache License, Version 2.0 (Summary & Reference)

Licensed under the Apache License, Version 2.0 (the "License"); you may not use these components except in compliance with the License. You may obtain a copy of the License at:

<http://www.apache.org/licenses/LICENSE-2.0>

Unless required by applicable law or agreed to in writing, software distributed under the License is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the License for the specific language governing permissions and limitations under the License.

### 2.2 MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
