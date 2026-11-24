# Kipo Evidence Workbench

一个可复跑的复杂 showcase，包含两部分：**公开数据 benchmark**，以及**真实软件故障讨论的证据回放**。所有数据进入公共 `kp` CLI；运行器不直接写数据库，也不伪造模型响应。

它回答一个具体问题：团队保存了过去的修复经验，后来版本变了、作者纠正了建议、维护者改变了处理方式，Agent 能否找到足够的依据，避免把旧经验或讨论中的设想当成今天的事实？

## 真实场景：Next.js 缓存故障协作

材料来自 [Next.js issue #42991](https://github.com/vercel/next.js/issues/42991) 的 18 条公开评论和关联的 [PR #48383](https://github.com/vercel/next.js/pull/48383)。这是同一事件的真实讨论链，不是把几个社区故事拼成一家虚构公司的事故。

讨论包含足够真实的复杂性：

1. 有人建议给 Link 加 query string；同一作者后来承认同路由段仍会缓存，还可能引发 hydration 问题。
2. `prefetch=false` 的效果随版本发生变化；有人实际降级验证，也有人报告新的反例。
3. `pageTTL` 是讨论者设想的 API，不能当成已发布配置。
4. 维护者为具体 reproduction 指出应用层问题，但不能由此推断所有报告都属于同一个问题。
5. 评论被标成 outdated，只表示维护者已经读过，不等于内容过期。
6. issue 关闭是为了把讨论迁移到更完整的说明，不等于所有应用的故障已修复。

主模式导入 **GitHub 原始正文**，逐条校验冻结的 SHA256。仓库只分发 URL、哈希、时间和 Agent 整理的摘要；原始讨论保存在使用者本地缓存，不随代码再分发。`--content paraphrase` 是摘要敏感性对照，不是原文能力测试。

### 时间的含义

19 条材料分成 9 个证据可用阶段，冻结了 12 个问题。大量早期评论后来在 2023-08-10 编辑过，所以用 `max(created_at, updated_at)` 排序当前正文。**这是保守的当前快照回放，不是对丢失的历史正文做重建。** 每一阶段只摄取当时已纳入的材料，再执行该阶段的问题；没有用全量未来资料回答早期问题。

真实作者保留为外部 `github:<author>`，本地 `operator` 只是导入者。没有把社区作者擅自设成库 owner/maintainer，也没有虚构批准关系。该案例不应产生强制执行的团队命令规范。

## 快速运行

从仓库根目录运行。执行器使用 Python 标准库；`answer.py` 的模型消费者另见其 `--help`。需要已经构建的 `kp`：

```sh
cargo build --release -p kipo --features connectors --locked
```

先跑原文检索回放。它会读取公开 GitHub API，正文哈希不匹配时会停止，要求重新审查材料，而不是悄悄换成新版本。

```sh
python3 examples/showcase/scenario.py \
  --kp target/release/kp \
  --out /tmp/kipo-showcase-case \
  --content original --fetch-originals --mode raw
```

需要离线运行时，给出之前保存的 GitHub API 快照：

```sh
python3 examples/showcase/scenario.py \
  --out /tmp/kipo-showcase-case-cached \
  --original-comments /path/to/comments-cache.json \
  --original-pr /path/to/pr-cache.json
```

comments 文件可为 API 对象数组或 `{"comments": [...]}`；PR 文件为包含 `body` 的 API 对象。完整缓存不在仓库中。也可以用 `--content paraphrase` 运行已附的 Agent 摘要，但应在展示时明确输入变化。

### 真实抽取

使用已经登录、可调用的 Codex/Claude CLI。以下调用会把公开原文发送给该模型，运行器不会创建假模型来替代它：

```sh
python3 examples/showcase/scenario.py \
  --out /tmp/kipo-showcase-live \
  --content original --fetch-originals --mode live \
  --host-kind codex --host-program /absolute/path/to/codex
```

`live` 每阶段执行真实 `kp extract`，保留生成 fact ID、错误、来源 anchor、审阅列表和 `why` 输出。生成事实数量不等于事实正确率。若认证、网络或模型失败，结果会保留失败命令，退出码非零。

`--embedding none` 明确使用词法检索；`--embedding local` 明确指定 `local:bge-small-zh-v1.5`，最后用 `embed --stale` 核验真实模型和 512 维向量。这里禁用了自动下载，需要事先安装本地模型。不能用 `hash:v1` 冒充语义向量。

### 消费检索结果的真实 Agent

`answer.py` 从已经记录的每阶段搜索结果生成回答。每题是独立模型上下文，输入只包含问题及真实召回片段，不包含 gold、期望来源、答案、评分或未来阶段的证据。

这是一层示例消费者，用来展示 Agent 怎样利用 Kipo 的结果；它不是 Kipo 内置的问答 API。回答、引用、拒答和模型调用日志单独保存，回答正确性需要独立审阅。

```sh
python3 examples/showcase/answer.py \
  --receipt /tmp/kipo-showcase-case/result.json \
  --output /tmp/kipo-showcase-answers \
  --codex /absolute/path/to/codex --jobs 2
```

## Benchmark：LongMemEval-S

选择 [LongMemEval 官方项目](https://github.com/xiaowu0162/LongMemEval) 和 [官方 cleaned 数据](https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned)。冻结 revision、277,383,467 字节的文件和 SHA256；首次准备需要下载约 277 MB。数据与许可证信息见 [benchmark-manifest.json](benchmark-manifest.json)。

默认六类问题各一题，再取一题 abstention，使用固定 seed 的哈希排序，选择发生在看结果之前。每题导入完整历史；原始 session ID 可能带有答案标记，故进入索引的文件名统一替换成中性编号。`answer`、`has_answer`、gold session IDs、问题与答案均不会进入历史目录。

```sh
python3 examples/showcase/benchmark.py --run --limit 7 \
  --kp target/release/kp --embedding none --output /tmp/kipo-benchmark

python3 examples/showcase/benchmark.py --run --limit 7 \
  --kp target/release/kp --embedding local --output /tmp/kipo-benchmark

# 同一协议扩展到全部 500 题；这不等于已实跑全量。
python3 examples/showcase/benchmark.py --run --limit 0 \
  --kp target/release/kp --embedding local --output /tmp/kipo-benchmark-full
```

指标定义：

| 指标 | 实际含义 |
|---|---|
| recall_any@k | 前 k 个去重 session 是否至少命中一个标注证据 session |
| recall_all@k | 前 k 个去重 session 是否包含全部标注证据 session；逐题 0/1 |
| MRR@10 | 首个标注证据 session 的倒数排名，超过 10 为 0 |
| CLI latency | 包含进程启动、数据库打开、模型初始化和搜索的总墙钟时间 |

拒答题没有检索 gold，保留其搜索结果但不进入上述分母。`--repetitions` 可重复搜索；默认单次耗时只作观察，不能发表性能优势结论。

这是 **all-message、原生 chunk 到 session 去重的自定义检索协议**。官方检索脚本的 user-only 协议、官方完整问答评估、这里的 smoke 不能混为一谈。LongMemEval 本身包含模拟对话，因此 benchmark 与真实社区案例是两部分独立证据。LoCoMo 同样不是可冒称真实用户历史的替代物。

2026-11-25 对 `0e6a1de` 的实测：两档都完整导入 352 个 session 记录、3,689 个消息轮次和 2,634 个 chunks。

| 模式 | Any@10 | All@10 | MRR@10 |
|---|---:|---:|---:|
| 词法检索 | 6/6 | 4/6 | 0.8056 |
| 本地真实 BGE + 词法 | 6/6 | 4/6 | 0.6667 |

两个未找全的类别为 multi-session 和 temporal-reasoning。小样本及不同候选池不足以判断哪种检索总体更优；结果没有证明开启向量就会提升表现。

## 治理与协议控制实验

真实事件自然覆盖来源、版本变化、权威边界与检索。其余能力用明确标记的受控输入验证，避免给真实社区虚构权限撤回、审批或技能学习记录。

```sh
python3 examples/showcase/governance.py \
  --kp target/release/kp --output /tmp/kipo-governance

python3 examples/showcase/interfaces.py \
  --kp target/release/kp --output /tmp/kipo-interfaces
```

每次使用新输出目录，不覆盖既有结果。接口测试仅监听 `127.0.0.1`，涉及两个独立本地 Community Library；测试结束停止服务。没有发布到真实社区，没有安装或修改现有 Agent 配置。

本次实跑治理 **53 通过、0 失败、5 未覆盖**，接口 **27/27 通过**。本地 `kp --as reader forget <owner_fact>`、源级 `ingest withdraw` 与 HTTP 越权撤回均拒绝非作者且非管理员的撤回请求。

另一个经实测确认的状态机规则：技能获得两次独立任务的 `helped` 后，首次 `hurt` 不会回滚；当 `hurt >= 2` 或 `hurt >= 1` 且 `hurt >= helped` 时触发回滚（已与仓库中英文 README 描述保持一致）。

## Feat 覆盖与边界

| 能力 | 展示入口 | 本次证据与边界 |
|---|---|---|
| 单文件个人/社区库 | 所有运行器 | 隔离 SQLite 库；跨库交换实跑 |
| 公共 ingest、外部身份、thread | scenario | 原始正文、真实作者、保守时点、thread_key |
| 文件 add、结构化切块 | benchmark | 完整会话文本经原生导入；没有手工造 chunk |
| raw_only / 模型路由 | scenario 两档、governance | raw-only 不产生语义事实；live 为真实 CLI 抽取 |
| ONNX + FTS 检索 | benchmark 两档 | 真实向量验收；非 hash 向量 |
| 来源锚点与逐字 span | live scenario | 原文模式核对原正文；摘要模式的引文只指摘要 |
| 幂等、supersede、withdraw | scenario / governance | 重复无新增、变更字节拒绝、显式替换与撤回 |
| 权限收窄、stale roster | governance | 当前 roster 立即生效，旧 roster 不能恢复权限 |
| fact 撤回权限 | governance / interfaces | 本地 CLI、源级 withdraw 与 HTTP 路径均拒绝越权撤回 |
| 有效时间、历史 slot | governance | valid-time 搜索；不声称任意 transaction-time 重建 |
| 推理与 why | live / governance | 保留实际输出；当前部分 base fact 的 why 无 L0 引文，来源核验依靠 search anchor |
| 导出、编辑、apply、review | governance | viewer/库身份绑定、幂等、stale 冲突、作者确认 |
| typed command guard | governance | 显式控制输入；散文不会被运行时解析成命令规范 |
| Codex 一次性 allow | governance | 权限、10 分钟 lease、单次消费与过期拒绝 |
| Claude guard 语义 | 代码核验 | Claude 返回 ask；不把它误当 Codex 的 lease 消费路径 |
| episode、dream、feedback、skill | governance | 显式合成证据验证状态机；不是自动学习质量证明 |
| MCP stdio | interfaces | 3 工具、note/search/why、ACL、隐藏/未知一致行为 |
| HTTP REST / MCP / A2A | interfaces | bearer、Origin、5 工具目录、鉴权搜索 |
| publish / outbox / sync | interfaces | 发布、镜像、撤回传播到两个本地库 |
| 自然多人批准、双锚 ratification | 未自然覆盖 | 本案例无适合的授权批准链；不能把普通赞同升级为审批 |
| imported skill quarantine | 未实跑 | base fact 跨库交换不能替代此验收 |
| 真实模糊 scope 审阅 | 未自然覆盖 | 未人工注入模型候选来制造成功 |
| 自动从真实工作中学习 skill | 未证明 | 手工 episode 只验证机制 |
| Office/PDF/HTML、代码锚点 verify | 当前代码能力表 | 未给本案例制造文档副本凑格式；PDF needs_ocr 不代表会 OCR |
| 真实 GitHub/飞书连接器与 watch | 未实跑 | JSONL 回放不等于线上连接器端到端验收 |
| Inspector、daemon、真实宿主安装 | 未实跑 | 不修改使用者现有 hooks/MCP 设置 |
| 音视频输入 | 不适用 | 当前版本已移除对应原型 |

“所有 feat”在这里意味着都有明确的展示位置或证据缺口。没有自然场景与实跑证据的能力不会标成已通过。

## 阅读报告与演示顺序

`render_report.py` 汇总已有回执，生成无需前端依赖的单文件交互 HTML：

```sh
python3 examples/showcase/render_report.py \
  --evidence /path/to/kipo-showcase-2026-11-25
```

建议演示先看 query-string 自我纠正和 pageTTL 非官方 API，再看关闭 issue 的语义边界；接着展示重复摄取、成员离开、过期审批和回写冲突；最后打开 benchmark 失败题与治理红灯。

真实案例的 `@5` 是五个原生 hit，benchmark 的 `@10` 是十个去重 session，不能合并成一个排名。真实案例的 gold 是策划者指定证据集合，并非每题唯一、最小的充分依据。`why` 的逻辑证明输出不总含 source ID；无直接来源坐标的 hit 会列为 unresolved，而不会靠字符串猜测来源。

目录中的 `result.json`、`report.json`、`commands/*.json`、`commands.jsonl` 和 `assertions.json` 是事实依据。原始日志可能包含公开来源全文或临时测试 token 的脱敏表示；对外展示优先使用摘要报告，不上传整个工作目录。
