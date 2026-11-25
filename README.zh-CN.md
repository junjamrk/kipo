<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/kipo-hero-dark.svg">
    <img alt="Kipo (kp)：面向 AI 编程智能体与研发团队的本地优先记忆数据库" src="docs/assets/kipo-hero-light.svg" width="100%">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/junjamrk/kipo/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/junjamrk/kipo/ci.yml?branch=main&label=CI" alt="CI 状态"></a>
  <a href="https://github.com/junjamrk/kipo/releases"><img src="https://img.shields.io/github/v/release/junjamrk/kipo?include_prereleases&label=Release" alt="最新版本"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="许可证：Apache-2.0"></a>
  <a href="rust-toolchain.toml"><img src="https://img.shields.io/badge/Rust-1.89%2B-orange.svg" alt="Rust 1.89+"></a>
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <strong>简体中文</strong> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#为什么选择-kipo">为什么选择 Kipo</a> ·
  <a href="#架构总览">架构总览</a> ·
  <a href="#核心工作流">核心工作流</a> ·
  <a href="#命令速查表">命令速查表</a> ·
  <a href="TUTORIAL.md">实战教程</a> ·
  <a href="examples/showcase/README.md">基准与真实案例回放</a> ·
  <a href="docs/ARCHITECTURE.md">架构规范</a> ·
  <a href="CONTRIBUTING.md">参与贡献</a>
</p>

# Kipo (`kp`)

<img align="right" src="docs/assets/kipo-logo.svg" width="76" height="76" alt="Kipo 独立品牌标志">

**Kipo (`kp`)** 是面向 AI 编程智能体（Claude Code、Codex、Cursor、Windsurf）与工程团队的本地优先记忆基础设施。它将脱敏后的 L0 原文、双时态结构化事实、读时溯源半环（Provenance Semiring）证明树以及睡眠期演化的 `SKILL.md` 技能统一存放在单文件 SQLite 数据库（`kp.sqlite`）中，无需绑定任何云服务即可离线运行与按权限协同。

<br clear="right">

<p align="center">
  <img src="docs/assets/kipo-terminal-demo.svg" alt="Kipo 终端动画演示：初始化、接入智能体、上下文召回、PreToolUse 危险命令守卫与即时撤回" width="100%">
</p>

## 2 分 22 秒架构与实战视频导览

<p align="center">
  <a href="https://github.com/junjamrk/kipo/raw/main/docs/assets/kipo-explainer.mp4">
    <img src="docs/assets/kipo-explainer-preview.gif" alt="Kipo 八幕架构解说视频动态预览" width="100%">
  </a>
</p>

<p align="center">
  <a href="https://github.com/junjamrk/kipo/raw/main/docs/assets/kipo-explainer.mp4">
    <img src="docs/assets/kipo-video-poster.svg" alt="点击播放 2 分 22 秒 Kipo 全程中文配音架构导览视频（720p HD MP4）" width="100%">
  </a>
</p>

<p align="center">
  <a href="https://github.com/junjamrk/kipo/raw/main/docs/assets/kipo-explainer.mp4"><strong>播放完整 2 分 22 秒中文配音视频（720p HD MP4）</strong></a> ·
  <a href="docs/assets/kipo-explainer.mp4">在仓库中查看视频文件</a> ·
  <a href="TUTORIAL.md">运行 5 分钟离线交互体验（<code>kp demo</code>）</a>
</p>

---

## 快速开始

### 1. 安装 `kp`

**Homebrew（macOS 与 Linux）**

```sh
brew install junjamrk/tap/kipo
```

**一键预编译包安装（落盘前强制校验 `SHA256SUMS` 与二进制自检）**

```sh
# macOS 与 Linux（支持 x86_64 / aarch64，Linux 需 glibc >= 2.28）
curl -fsSL https://raw.githubusercontent.com/junjamrk/kipo/main/scripts/install.sh | sh

# Windows PowerShell（支持 x86_64 / arm64）
irm https://raw.githubusercontent.com/junjamrk/kipo/main/scripts/install.ps1 | iex
```

> [!TIP]
> 所有官方 GitHub Release 归档包均附带 Sigstore 构建溯源签名，可通过 `gh attestation verify kipo-<target>.tar.gz --repo junjamrk/kipo` 在本地验证供应链完整性。

**使用 Cargo 从源码构建**

```sh
# 核心 CLI + 本地 ONNX 向量嵌入（生成 kp 二进制）
cargo install --git https://github.com/junjamrk/kipo kipo

# 同时安装解耦外部连接器（启用 kp connector 子命令及独立 kp-connector 二进制）
cargo install --git https://github.com/junjamrk/kipo kipo --features connectors
cargo install --git https://github.com/junjamrk/kipo kp-connect
```

### 2. 运行 5 分钟零密钥离线全流程体验

无需任何外部 API Key 或网络连接，单条命令即可完整体验双宿主 Hook 接入、公开 JSONL 导入、`StalePremise` 真值维护、`<= 250 tokens` 上下文召回、双端 `PreToolUse` 命令守卫、Markdown 无损导出写回以及 `carol` 零泄漏隔离：

```sh
kp demo
# 保留示例知识库并打开本机 127.0.0.1 四栏只读查看器：
kp demo --dir ./kipo-demo
kp inspect --path ./kipo-demo/demo-lib --as alice
```

### 3. 三条命令接入日常编程工具

```sh
kp init --path ~/.kp/personal --owner alice --name "Alice 个人库"
kp connect claude-code --project . --path ~/.kp/personal --as alice
kp connect codex --project . --path ~/.kp/personal --as alice
```

同样支持一键写入 Cursor 与 Windsurf 的 MCP 配置：

```sh
kp connect cursor --project . --path ~/.kp/personal --as alice
kp connect windsurf --global --path ~/.kp/personal --as alice
```

---

## 为什么选择 Kipo

现有 Agent 记忆方案往往走向两个极端：要么把静态 Markdown 规则全量塞进每次提示词，要么把原始对话上传到共享云端向量库，一旦成员退群、规则过期或前提撤回，派生摘要便持续越权泄露。

| 对比维度 | 静态规则文件 (`CLAUDE.md`) | 共享云端向量 RAG | Kipo (`kp`) |
| --- | --- | --- | --- |
| **存储形态** | 无索引纯文本 | 依赖外部向量库与图数据库服务 | 单文件本地 `kp.sqlite`（WAL 模式） |
| **派生结论权限** | 无（全仓库可见） | 写入时静态拷贝 ACL，退群后残留 | 读时沿溯源半环实时求值，零拷贝 ACL |
| **撤回与擦除 (`forget` / `purge`)** | 跨仓库手工搜索删改 | 旧摘要向量持续滞留直至重算 | `0 ms` 级联失效 + HMAC 墓碑防复活 |
| **提示词上下文开销** | 随文件膨胀线性增长 | 单次动辄注入数千 tokens | 硬上限 `<= 250 tokens` 且 `<= 3` 条 |
| **危险命令防呆** | 仅靠提示词软约束 | 无 | `PreToolUse` 实时拦截（`ask` / `deny` + `kp allow`） |
| **离线与零外发模式** | 支持 | 强依赖云端接口 | 100% 离线可用，`RawOnly` 下外部请求数为 `0` |

<p align="center">
  <img src="docs/assets/kipo-semiring.svg" alt="传统共享知识库静态拷贝权限与 Kipo 读时溯源半环隔离机制对比图" width="100%">
</p>

Kipo 的三项核心工程设计：

1. **单文件 SQLite 同构引擎（个人库 = 1 人社区）。** 个人库与团队社区库运行完全一致的 Rust 引擎与 `kp.sqlite` 表结构：落盘前脱敏的内容寻址 `raw_blobs`、`jieba-rs` 中英文分词 FTS5（`contentless_delete=1`）、本地 ONNX 向量、双时态 `facts` 与 JTMS `justifications` 证明表。
2. **读时溯源半环与严格非干涉（Non-Interference）。** 受众集合（`aud`）与生效时间窗（`[valid_from, valid_until)`）绝不拷贝到派生结论上。碰撞告警、限定受众综合、概念对齐与沉淀技能全部在读取时沿证明树联合检查 `(viewer, time)`。成员移出群组、时间窗到期或执行 `kp forget` / `kp purge` 后，依赖结论立即不可见，未授权成员看到的搜索、日志与错误响应逐字节保持一致。
3. **平台无关内核与闭合 JSONL 写入契约。** 核心 Crate 不耦合任何聊天软件或工单系统字段。Claude Code 与 Codex Hooks、`kp-connect` 外部连接器、MCP `kp_note` 与自定义脚本统一通过 [docs/INGEST-SCHEMA.md](docs/INGEST-SCHEMA.md) 定义的 Draft 2020-12 JSON Schema 写入（未知字段直接拒绝，防止策略参数拼写错误静默放行）。

---

## 架构总览

<p align="center">
  <img src="docs/assets/kipo-architecture.svg" alt="Kipo 四层架构图：外部输入源、治理与脱敏边界、单文件 SQLite 同构内核与按身份隔离的读取接口" width="100%">
</p>

Workspace 由 8 个无环 Rust Crate 组成（全局启用 `unsafe_code = "forbid"`，禁止 `clippy::unwrap_used` 与 `clippy::expect_used`），编译为单一主程序 `kp` 与可选的独立连接器 `kp-connector`：

```text
kipo/
├── Cargo.toml                  # Workspace 清单（Apache-2.0，MSRV 1.89，零 unsafe）
├── scripts/
│   ├── install.sh              # POSIX 安装脚本（含 SHA-256 校验与宿主自检）
│   ├── install.ps1             # Windows PowerShell 安装脚本（含 SHA-256 校验）
│   └── verify-oss-readiness.py # 文档链接、锚点、SVG、工作流与隐私门禁校验脚本
├── docs/
│   ├── ARCHITECTURE.md         # SQLite 表结构、半环不变量与安全模型详解
│   └── INGEST-SCHEMA.md        # 公开 JSONL 写入契约与 Draft 2020-12 JSON Schema
└── crates/
    ├── kp-model/               # 领域新类型、溯源半环、PublicIngestLine、SecretRedactor
    ├── kp-store/               # 单文件 WAL SQLite、中英文 FTS5、ONNX 与多云向量、导出写回
    ├── kp-derive/              # 成批抽取、Datalog 规则、JTMS 级联失效、why() 证明树
    ├── kp-retrieve/            # 先过滤后排序混合检索（BM25+向量+RRF+DPP）、命令守卫、Judge
    ├── kp-ingest/              # 智能体 Hooks、tree-sitter 符号锚点、文档与网页路由、kp dream
    ├── kp-net/                 # 0700 私有 UDS 守护进程、127.0.0.1 Web 查看器、社区 Axum 服务
    ├── kp-connect/             # 零存储依赖外部连接器（GitHub、飞书 lark-cli、历史会话回填）
    └── kp-cli/                 # 包名 kipo -> 二进制 kp（CLI、演示、守护进程、MCP、社区服务）
```

---

## 核心工作流

### 1. 零阻塞 Hooks、受限上下文注入与双端 `PreToolUse` 守卫

* **零阻塞后台捕获。** 首次触发 Hook 时自动在后台拉起 `0700` 私有 Unix Socket `kp daemon` 并立即返回，绝不卡住智能体回合。会话原文经 `SecretRedactor` 脱敏后落盘，并按增量游标切批异步抽取。
* **硬预算提示词召回。** `UserPromptSubmit` 最多注入 `250 tokens` / `3` 条高相关记忆，每条均附带说话人原话出处（`↳`）；无关提示词直接返回 `{}`。
* **结构化 `PreToolUse` 命令守卫。** 写入时解析的 `CommandGuardSpec` 及显式 `[forbid: <cmd>]`、`[allow: <cmd>]`、`[require_flag: <cmd> <flag>]` 标签会在工具执行前自动核验：
  * **Claude Code：** 返回 `permissionDecision = "ask"` 并展示原话证据链。
  * **Codex：** 返回 `permissionDecision = "deny"`，同时给出库所有者专用的 10 分钟单次放行命令 `kp allow <kp-ref>`。
* **低打扰人工治理。** 作用范围存在歧义的规则默认收窄为仅本人可见（`SelfOnly`），仅在影响其他成员时挂入 `kp review`；执行 `kp reject <fact_id> --reason "..."` 可撤回错误抽取并写入双钥匙粘性拒收记录，重抽同一会话不会复活。

### 2. 多格式文档、代码符号锚点与网页导入（`kp add` 与 `kp verify`）

支持导入 Markdown、源代码（`rs`、`py`、`ts`、`tsx`、`js`、`jsx`、`go`）、DOCX、PPTX、XLSX、PDF、HTML 及带 SSRF 防护的 `http://` / `https://` 网页，持久化前统一执行凭证脱敏并保留精确页码、表格坐标或代码符号锚点：

```sh
kp add ./docs ./notes/runbook.docx --path ~/.kp/personal --as alice
kp add https://example.com/runbook.html --path ~/.kp/personal --as alice

# 将接口契约锚定到具体 Rust 函数符号，并在代码重构后检测签名漂移：
kp remember --subject settle_invoice --predicate kp.code.api_contract \
  --value "结算前必须校验当前版本的请求字段" \
  --repo . --file src/billing.rs --symbol settle_invoice --path ~/.kp/personal
kp verify --repo . --path ~/.kp/personal --json
```

遇到扫描件或字库损坏的 PDF 页面时，`kp add` 会标记 `needs_ocr: true` 并隔离乱码页，避免污染索引。

### 3. 公开 JSONL 写入契约与解耦连接器（`kp ingest` 与 `kp-connector`）

可从文件、`stdin`、stdio MCP（`kp mcp`）或完全不依赖 `kp-store` 的 `kp-connector` 流式写入容器名录及 `upsert`、`supersede`、`withdraw` 记录：

```sh
cat <<'JSONL' | kp ingest - --path ~/.kp/personal --as alice --json
{"container_id":"scope.release","label":"发布值班组","members":["alice","bob"],"all_library_members":false,"synced_at":1795089600000}
{"source":{"kind":"chat","uri":"chat://release/msg-101","external_id":"msg-101","container_id":"scope.release"},"speaker":"bob","audience":"container_members","occurred_at":"2026-11-22T10:00:00Z","content":"周五 18:00 UTC 进入数据库冻结窗口，周一前暂停 schema 变更。","op":"upsert"}
JSONL

# 增量同步 GitHub 协作者、Issue 与 PR（内置 ETag 缓存与 since 游标）：
kp-connector --state ~/.kp/connectors/state.json scope add \
  --kind github --scope-id repo.kipo --upstream junjamrk/kipo --label "Kipo Repo"
kp-connector --state ~/.kp/connectors/state.json sync --kind github --ingested-by alice \
  | kp ingest - --path ~/.kp/personal --as alice --json
```

内置连接器覆盖 **GitHub REST API**、**飞书 / Lark（`lark-cli`）**（群消息、撤回与编辑、退群权限自动收缩、知识库与云文档）以及 **Claude Code / Codex 历史会话回填**。

### 4. 默认本地 ONNX 向量与国内外多服务商 Recipe

`kp init` 默认配置本地 ONNX 模型（`local:bge-small-zh-v1.5`，在 `~/.kp/models/` 下校验 SHA-256，离线时自动无感回退至 `jieba-rs` BM25 检索），同时内置 `dashscope`、`volcengine`、`zhipu`、`openai`、`voyage`、`google`、`openrouter`、`ollama` 等向量 Recipe 与单库维度锁：

```sh
kp providers list
kp providers env dashscope
kp providers test --path ~/.kp/personal --json
kp embed --stale --to local:bge-small-zh-v1.5 --path ~/.kp/personal --json
```

### 5. 本机 `127.0.0.1` 四栏只读查看网页与 Markdown 无损写回

通过严格绑定 `127.0.0.1` 的四栏 Web Inspector 查看“记住了什么、为什么记住、发生了什么变化、待确认事项”，或导出为带 `base_seq` 乐观锁保护的本地 Markdown 目录直接编辑后写回：

```sh
kp inspect --path ~/.kp/personal --as alice

kp export ./memory-md --path ~/.kp/personal --as alice --json
# 在编辑器中修改或删除 ./memory-md/ 下的条目后写回：
kp apply ./memory-md --path ~/.kp/personal --as alice --json
kp log --limit 20 --path ~/.kp/personal --json
```

### 6. 睡眠期巩固与受治理技能演化（`kp dream`）

`kp dream` 从编程会话、经确认的多轮群聊决策与多格式文档中提炼事实、写入期 `CommandGuardSpec` 守卫规则，并晋升为 `<library>/skills/<slug>/SKILL.md` 技能包：

```sh
kp dream --sessions ~/.codex/sessions ~/.claude/projects --path ~/.kp/personal --json
kp skill list --path ~/.kp/personal --json
kp feedback check-api-contract --outcome helped --task pr-142 --path ~/.kp/personal --json
```

* **零配置模型路由。** 自动复用本机已登录的 `claude` 或 `codex` CLI，也可指定云端兼容接口；在 `KP_MODEL_POLICY=raw_only` 下零外发网络请求，仅建立 L0 原文索引，绝不凭空编造规则。
* **证据门槛晋升与自动回滚。** 技能需累计至少 `2` 个独立任务的 `helped` 验证且 `hurt == 0` 才会生成 `SKILL.md`；当 `hurt >= 2`、或 `hurt >= 1` 且 `hurt >= helped`、或基础前提撤回导致有效支撑不足时，自动撤销技能并清理投影文件。从社区同步的外部技能默认处于只读隔离状态（`exec_approved: false`），须由本机库所有者显式执行 `kp skill approve <slug>` 后方可启用。

### 7. 同构多成员社区协作（`kp serve --community` 与 `kp sync`）

使用同一个 `kp` 二进制即可启动支持 HTTP REST、Streamable HTTP MCP（`/mcp`）与 A2A（`/.well-known/agent.json`、`/a2a`）的共享社区库：

```sh
kp init --community --owner admin --name "研发协作社区" --path ./team-lib
kp member add alice --path ./team-lib --json
kp member add bob --path ./team-lib --json
kp serve --community --listen 127.0.0.1:8080 --path ./team-lib

# Alice 本机执行一次 join，后台守护进程即通过持久化 outbox 自动同步多方可见事实
kp community join http://127.0.0.1:8080 --token "$ALICE_TOKEN" --path ~/.kp/personal --json
```

端到端双人自然语言协作与 Carol 隔离演练详见 [TUTORIAL.md](TUTORIAL.md)。

---

## 命令速查表

全局参数适用于所有子命令：`--path <DIR>`（指定库目录，默认 `.`）、`--as <ACTOR>`（指定当前操作成员，默认库所有者）、`--json`（stdout 输出结构化 JSON，stderr 输出结构化错误）。

| 分类 | 子命令 | 说明 |
| --- | --- | --- |
| **建库与体验** | `init`, `status`, `demo`, `member add` | 初始化个人库或社区库、查看状态统计、运行 5 分钟离线演示、签发成员访问凭证 |
| **智能体接入** | `connect`, `hook`, `daemon`, `inspect`, `mcp` | 接入 Claude Code / Codex / Cursor / Windsurf、管理 `0700` UDS 守护进程、启动 `127.0.0.1` 查看器与 stdio MCP |
| **导入与记录** | `add`, `ingest`, `remember`, `dead-end`, `verify` | 导入文档代码网页、流式写入公开 JSONL、记录显式事实与失败尝试、校验 `tree-sitter` 代码符号漂移 |
| **检索与追问** | `search`, `why`, `derive`, `synthesize` | 按查看者权限混合检索（支持 `--deep`、`--at`、`--also-library`）、查看半环证明树、生成限定受众综合 |
| **治理与生命周期** | `review`, `allow`, `reject`, `forget`, `purge`, `identity bind` | 确认歧义范围、签发单次命令放行、拒收错误抽取、撤回事实或写入 HMAC 墓碑彻底擦除原文 |
| **投影与向量** | `export`, `apply`, `log`, `providers`, `embed` | Markdown 无损导出与乐观锁写回、查看按权限隔离的审计日志、测试与迁移本地或云端向量模型 |
| **睡眠期自演化** | `dream`, `episode record`, `skill`, `feedback` | 从会话提炼事实与技能、审批外部社区技能、记录按任务去重的 `helped` / `hurt` 反馈 |
| **社区与连接器** | `community`, `serve`, `publish`, `sync`, `connector` | 加入社区库、启动 HTTP/MCP/A2A 社区服务、双向幂等同步、运行 GitHub/飞书/会话增量连接器 |

---

## 本地开发与离线验收

全套单元测试、集成测试、Hooks、守护进程、Web Inspector、连接器与端到端旅程测试均可在 100% 离线、无任何外部密钥的环境下运行：

```sh
sh -n scripts/install.sh
python3 scripts/verify-oss-readiness.py
cargo fmt --all -- --check
cargo clippy -p kipo --all-targets --no-default-features --locked -- -D warnings
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
KP_OFFLINE=1 KP_DISABLE_MODEL_DOWNLOAD=1 cargo test --workspace --all-features --locked
```

Crate 依赖红线、代码风格与扩展连接器或向量 Recipe 的步骤见 [CONTRIBUTING.md](CONTRIBUTING.md)。

---

## 安全、治理与开源许可

* **安全策略与私密漏洞上报：** [SECURITY.md](SECURITY.md)
* **诊断与支持指南：** [SUPPORT.md](SUPPORT.md)
* **贡献者指南：** [CONTRIBUTING.md](CONTRIBUTING.md)
* **社区行为准则：** [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)
* **版本变更记录：** [CHANGELOG.md](CHANGELOG.md)
* **开源许可证：** 本项目基于 [Apache License, Version 2.0](LICENSE) 开源。所借鉴开源项目的算法、提示词结构及依赖库版权声明完整记录于 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
