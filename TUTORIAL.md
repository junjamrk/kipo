# Kipo (`kp`)：从个人记忆到多 Avatar 协作

**Kipo (`kp`)**（仓库：<https://github.com/junjamrk/kipo>）把原始材料、可检索事实、半环推导依据和闲时演化出的技能统一存放在单文件 SQLite 本地库中。个人库与社区库使用完全同构的同一套引擎。默认检索和日常 Hooks 绝不阻塞会话；可先执行 `kp demo` 在 5 分钟内离线体验全流程（含双宿主配置接入、前提演进与 `StalePremise` 作废、`<kp-context>` 提示召回、双端 `PreToolUse` 守护、Markdown 无损导出写回及 `carol` 零泄漏隔离，并给出打开 `127.0.0.1` 只读查看网页的下一步命令）。

下面的命令适用于 Bash / Zsh，按顺序在同一终端执行。需要 Git；示例用 Python 3 读取 JSON，用 curl 演示社区接口。所有示例数据写入新建的临时目录。

## 准备 release 二进制

在仓库根目录执行：

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
kp --version

LAB=$(mktemp -d)
ALICE="$LAB/alice_lib"
BOB="$LAB/bob_lib"
Q="$LAB/community_q"
PROJECT="$LAB/project"
mkdir -p "$PROJECT/src" "$PROJECT/notes"
printf '本次示例目录：%s\n' "$LAB"
```

`--path` 指向知识库，`--as` 指定库内成员（默认使用库所有者）。`--json` 的成功结果写到 stdout，失败返回非零状态并将 JSON 错误写到 stderr。

## ① 初始化并连接客户端

```sh
kp init --path "$ALICE" --owner alice --name 'Alice 的个人库'
kp init --path "$BOB" --owner bob --name 'Bob 的个人库'
kp connect claude-code --project "$PROJECT" --path "$ALICE"
kp connect codex --project "$PROJECT" --path "$ALICE"
kp status --path "$ALICE" --json
```

连接命令分别更新项目的 `.claude/settings.json` 和 `.codex/hooks.json`，保留已有用户 hooks。`session-start` 默认零注入（仅后台预热 `kp daemon`）；`user-prompt-submit` 在相关时注入不超过 `250 tokens` / `3` 条带原话出处（`↳`）的记忆；`pre-tool-use` 按宿主协议执行危险命令守护（Claude Code 返回 `ask`、Codex 返回 `deny` 并提示单次授权 `kp allow <kp-ref>`）；`stop` / `session-end` 过滤带 `[kp-ref:…]` 的回声行并脱敏后异步保存。失败到恢复、人类纠正等真实对比才会产生待处理 episode。

## ② 导入材料并记录代码契约

```sh
cat > "$PROJECT/src/billing.rs" <<'RS'
pub struct Receipt;
pub struct Error;
pub fn settle_invoice(cents: u64) -> Result<Receipt, Error> {
    let _ = cents;
    Ok(Receipt)
}
RS
cat > "$PROJECT/notes/billing.md" <<'MD'
# 结算规则
fact: kp.code.api_contract = 结算前检查当前版本的请求字段。
MD
cat > "$PROJECT/notes/ops.html" <<'HTML'
<!doctype html>
<html><body><main><h1>团建检查单</h1><p>杭州团建前检查天气预警。</p></main></body></html>
HTML
kp add "$PROJECT" --extract --path "$ALICE" --json

F_API=$(kp remember --subject settle_invoice \
  --predicate kp.code.api_contract --value '结算前检查当前版本的请求字段' \
  --logical-key F_api --repo "$PROJECT" \
  --file src/billing.rs --symbol settle_invoice \
  --path "$ALICE" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["facts"][0])')
kp dead-end --subject settle_invoice --hypothesis '失败后盲目重试' \
  --reason '可能重复结算' --repo "$PROJECT" \
  --file src/billing.rs --symbol settle_invoice --path "$ALICE" --json
```

`add --extract` 同时保存 L0 原文与可识别的 L1 事实。支持 Markdown、源代码、PDF、Office 文档、HTML 和静态网页 URL；可直接将这些文件放入目录后导入。搜索结果保留行号、符号、表格单元格或页码锚点。扫描件或乱码字库 PDF 页面返回 `needs_ocr: true` 并自动隔离乱码，不会静默写入脏文本。

`remember` 适合显式事实；`dead-end` 记录已验证失败的尝试。未指定 audience 时只对当前 actor 可见。`--audience alice,bob` 指定成员，`--audience '*'` 允许库内所有成员；原始材料对应的参数是 `add --readable-by`。

## ③ 深度检索与可选 judge

```sh
kp remember --subject 杭州 --predicate kp.core.weather \
  --value '周五杭州有暴雨，降雨预警' --path "$ALICE"
kp remember --subject 雨天交通 --predicate kp.core.preference \
  --value '下雨天不开车，改乘高铁' --path "$ALICE"
kp search '周五还能自驾去杭州露营吗' --deep --path "$ALICE" --json
```

默认使用本地词法、向量、实体关联和最多两轮线索扩展。下列是可选命令，需要对应的模型服务已经启动：

```sh
# 本机 Decisions 服务（例如 KnowLine）
kp search '周五还能自驾去杭州露营吗' --deep \
  --judge-tier local-decision --judge-url http://127.0.0.1:8000/decisions \
  --judge-model KnowLine-4B --path "$ALICE" --json

# 先在当前 shell 中配置 OPENROUTER_API_KEY，再显式允许云端调用
kp search '周五还能自驾去杭州露营吗' --deep \
  --judge-tier cloud-jev --judge-model typesafe/jev-1.14 \
  --allow-cloud-jev --judge-key-env OPENROUTER_API_KEY --path "$ALICE" --json
```

Cloud Jev 默认使用 OpenRouter 的 `/api/alpha/decisions` SystemOne `noul` 协议。没有 `--allow-cloud-jev` 会在建立连接前失败。发送内容限制在当前查看者可见的候选片段内，并进行密钥脱敏。

本机有多个库时，给查看者授予目标库成员身份后可以联邦检索：

```sh
kp member add alice --path "$BOB" --json
kp remember --subject 杭州 --predicate kp.core.note --value '高铁订票需要身份证' \
  --audience alice,bob --path "$BOB"
kp search '周五还能自驾去杭州露营吗' --deep \
  --also-library "$BOB" --path "$ALICE" --json
```

各库先独立过滤权限再检索；启用 judge 时直接合并其 `[0,1]` 分数。默认 off 分数是启发式分数，不是模型校准概率。远程库通过下面的 publish/sync 流程协作。

## ④ 启动社区并发布事实

```sh
kp init --community --owner admin --name '出行协作社区' --path "$Q"
ALICE_TOKEN=$(kp member add alice --path "$Q" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
BOB_TOKEN=$(kp member add bob --path "$Q" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
CAROL_TOKEN=$(kp member add carol --path "$Q" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
COMMUNITY_URL=http://127.0.0.1:8080
kp serve --community --listen 127.0.0.1:8080 --path "$Q" &
KP_SERVER_PID=$!
trap 'kill "$KP_SERVER_PID" 2>/dev/null || true; wait "$KP_SERVER_PID" 2>/dev/null || true' EXIT
```

看到 `kp listening on 127.0.0.1:8080` 后继续。Token 只在签发时返回一次，社区存储其哈希。这里在一台机器上模拟三方；跨机器部署时在社区主机执行建库和 serve，每位用户在自己的机器初始化个人库并保存自己的社区 token。可用 SSH 转发 `ssh -L 8080:127.0.0.1:8080 user@community-host`，让个人机器上的 `COMMUNITY_URL` 仍指向本地端口。

```sh
kp remember --subject 杭州团建 --predicate kp.core.schedule \
  --value '周五杭州户外团建，气象台发布大暴雨预警' --kind event \
  --tags hangzhou_rain,outdoor --logical-key friday-offsite --path "$ALICE"
kp publish friday-offsite --to "$COMMUNITY_URL" --token "$ALICE_TOKEN" \
  --audience alice,bob,carol --path "$ALICE" --json

kp remember --subject 杭州出行 --predicate kp.core.preference \
  --value '下雨天绝不自驾开车，必须改乘高铁' --kind constraint \
  --tags hangzhou_rain --logical-key rain-constraint --path "$BOB"
kp publish rain-constraint --to "$COMMUNITY_URL" --token "$BOB_TOKEN" \
  --audience alice,bob --path "$BOB" --json
```

发布只发送结构化 claim 和溯源信息，原始会话文件留在个人库。`queued: false` 表示已送达；`queued: true` 表示写入本地 outbox，后续 sync 会重试。

## ⑤ 自动碰撞与限定受众的综合

两条事实共享 `hangzhou_rain` 标签，社区自动产生 `R2_EVENT_CONSTRAINT_COLLISION` 警告。综合只使用所有目标成员都能读取的事实，并把全部输入纳入审计依据：

```sh
SYNTHESIS_ID=$(kp synthesize --query 'hangzhou_rain 出行建议' \
  --audience alice,bob --as alice --path "$Q" --json | \
  python3 -c 'import json,sys; print(json.load(sys.stdin)["facts"][0])')
kp why "$SYNTHESIS_ID" --as alice --path "$Q" --json
```

这条命令在持有社区目录的主机运行。远程 Alice 可调用同一能力：

```sh
curl -fsS "$COMMUNITY_URL/v1/synthesize" \
  -H "Authorization: Bearer $ALICE_TOKEN" -H 'Content-Type: application/json' \
  -d '{"query":"hangzhou_rain 出行建议","target_audience":["alice","bob"]}'
```

默认综合器输出带依据的结构化摘要，并明确标记尚未验证的因果关系。

## ⑥ 同步、追问依据与 Carol 隔离

```sh
kp sync --from "$COMMUNITY_URL" --token "$ALICE_TOKEN" --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
kp search hangzhou_rain --deep --path "$BOB" --json
COLLISION_ID=$(kp search hangzhou_rain --path "$ALICE" --limit 100 --json | \
  python3 -c 'import json,sys; r=next(r for r in json.load(sys.stdin)["results"] if r["kind"]=="collision_warning"); print(r["id"].split(":fact-",1)[1])')
kp why "$COLLISION_ID" --path "$ALICE" --json
curl -fsS "$COMMUNITY_URL/v1/search?q=hangzhou_rain&deep=true" \
  -H "Authorization: Bearer $CAROL_TOKEN"
```

Alice 和 Bob 能看到警告和综合。Carol 只能看到公开 Event；询问隐藏结论的 `/v1/why/<id>` 与询问不存在的 ID 都得到 404。访问权限在读取时沿每条证明检查，不复制固定 audience 到推导结论。

Agent 接口也使用同一权限判断：

```sh
# 本地 stdio MCP，客户端以逐行 JSON-RPC 通信
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kp_search","arguments":{"query":"hangzhou_rain"}}}' | kp mcp --path "$ALICE"

# 社区 Streamable HTTP MCP（q 是社区搜索参数）
curl -fsS "$COMMUNITY_URL/mcp" -H "Authorization: Bearer $CAROL_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"kp_search","arguments":{"q":"hangzhou_rain"}}}'
curl -fsS "$COMMUNITY_URL/.well-known/agent.json"
curl -fsS "$COMMUNITY_URL/a2a" -H "Authorization: Bearer $CAROL_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"task":"search","input":{"q":"hangzhou_rain"}}'
```

## ⑦ 检测代码漂移与撤回前提

```sh
python3 - "$PROJECT/src/billing.rs" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
p.write_text(p.read_text().replace('cents: u64)', 'cents: u64, currency: &str)'))
PY
kp verify --repo "$PROJECT" --path "$ALICE" --json
kp forget rain-constraint --path "$BOB" --json
kp sync --from "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
kp sync --from "$COMMUNITY_URL" --token "$ALICE_TOKEN" --path "$ALICE" --json
kp why "$COLLISION_ID" --path "$ALICE" --json
```

签名变化产生 `sig_changed` 和 `R4_ANCHOR_STALENESS` 警告。撤回约束后，碰撞结论变为 `invalidated`，依赖它的综合变为 `quarantined`，正常检索不再返回它们。原有依据仍可用于本地审计。

## ⑧ 睡眠期学习、技能批准与回滚

白天 hooks 只捕获对比；`dream` 才处理 episode。下面用两个独立任务显式记录经过验证的恢复，均以 `F_api` 为依据：

```sh
AFTER=$(python3 - "$F_API" <<'PY'
import json, sys
print(json.dumps({"success": True, "target": "settle_invoice", "attribution": {
    "skill_slug": "check-api-contract", "trigger": "constructing a versioned API request",
    "wrong_move": "Assume request fields are compatible across versions",
    "verified_fix": "Compare request fields with the selected API contract before sending.",
    "dimension": "DOM", "cited_premise_ids": [sys.argv[1]]}}))
PY
)
for TASK in invoice-task-1 invoice-task-2; do
  kp episode record --kind failure-to-recovery --before 'Invoice request failed' \
    --after "$AFTER" --premise "$F_API" --session "$TASK" --step-ref tool:2 \
    --path "$ALICE" --json
done
kp dream --path "$ALICE" --json
kp skill list --path "$ALICE" --json
cat "$ALICE/skills/check-api-contract/SKILL.md"
```

预期创建 1 条 PlaybookBullet，累计 2 次独立 helped 验证并晋升技能；重复同一任务不会提高独立验证数。技能文件包含 frontmatter、StrongRef 与原样触发句：

> When constructing a versioned API request, load the `check-api-contract` skill.

```sh
SKILL_ID=$(kp skill list --path "$ALICE" --json | \
  python3 -c 'import json,sys; print(next(s for s in json.load(sys.stdin) if s["name"]=="check-api-contract")["fact_id"])')
kp publish "$SKILL_ID" --to "$COMMUNITY_URL" --token "$ALICE_TOKEN" \
  --audience alice,bob --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
kp skill list --path "$BOB" --json
kp search check-api-contract --path "$BOB" --json
kp skill approve check-api-contract --path "$BOB" --json
```

Bob 收到的技能默认 `exec_approved: false`，带只读提示。批准只适用于本地这个版本，不能由发布者代替 Bob 批准。技能目录是数据库的投影，手改文件不会修改事实。

反馈按独立任务计数。导入的技能没有 Bob 的本地验证历史；下面先记录两次实际成功，再演示两次伤害反馈触发回滚：

```sh
for TASK in local-success-1 local-success-2; do
  kp feedback check-api-contract --outcome helped --task "$TASK" --path "$BOB" --json
done
for TASK in local-failure-1 local-failure-2; do
  kp feedback check-api-contract --outcome hurt --task "$TASK" --path "$BOB" --json
done
kp skill list --path "$BOB" --json
kp forget "$F_API" --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$ALICE_TOKEN" --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
```

达到两次 hurt，或已有 hurt 且 `hurt >= helped`，会撤销技能并删除 `SKILL.md`。撤回基础事实也会级联撤销依赖技能。

## ⑨ forget 与 purge：撤回还是擦除

| 操作 | 结果 |
| --- | --- |
| `kp forget KEY_OR_FACT_ID` | 撤回事实，保留审计数据，触发依赖失效；已发布事实的撤回通过 sync 传播。 |
| `kp purge LOGICAL_KEY_OR_SOURCE_ID` | 先写不含原文的 tombstone，再清理事实、索引、向量、依赖技能与 `raw_blobs` 原文；sync 传播擦除。 |

```sh
kp remember --subject 演示清理 --predicate kp.core.note --value '一次性演示内容' \
  --logical-key disposable-note --path "$ALICE"
kp publish disposable-note --to "$COMMUNITY_URL" --token "$ALICE_TOKEN" \
  --audience alice,bob --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
kp purge disposable-note --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$ALICE_TOKEN" --path "$ALICE" --json
kp sync --from "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
```

个人库中擦除从原文提取的事实，也会擦除其所在原文及同源提取事实。原始输入文件（例如项目中的笔记）在知识库之外，不由 kp 删除；备份或离线机器只有应用 tombstone 后才能阻止旧数据复活。

## ⑩ M1 双人自然语言非打扰全自动闭环（Claude Code + Codex）

本节独立演示 M1 全自动自然语言闭环（假设 `kp` 已加入 `PATH`）：每人只做一次三命令设置（`init`、`community join`、`connect`），之后第一次 Hook 自动在后台拉起 `kp daemon`，成批抽取、按语义受众投递发件箱、社区双向同步与开放概念对齐推导全部在后台自动完成。Alice 的个人日程始终留在 Alice 本机不出库；唯一需要人手动确认的是当规矩作用范围存在歧义时执行一次 `kp review`。

### 1. 一次性设置（init → community join → connect）

```sh
LAB=$(mktemp -d)
ALICE="$LAB/alice_lib"
BOB="$LAB/bob_lib"
Q="$LAB/community_hub"
ALICE_PROJ="$LAB/alice_proj"
BOB_PROJ="$LAB/bob_proj"
mkdir -p "$ALICE_PROJ" "$BOB_PROJ"

# 启动共享社区并签发成员凭证
kp init --community --owner admin --name '研发协作社区' --path "$Q"
ALICE_TOKEN=$(kp member add alice --path "$Q" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
BOB_TOKEN=$(kp member add bob --path "$Q" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
CAROL_TOKEN=$(kp member add carol --path "$Q" --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')

COMMUNITY_URL=http://127.0.0.1:18080
kp serve --community --listen 127.0.0.1:18080 --path "$Q" &
M1_SERVER_PID=$!

# Alice 一次性设置（token 以 0600 存入库目录 communities.json，并自动拉取社区成员名录）
kp init --path "$ALICE" --owner alice --name 'Alice 个人库'
kp community join "$COMMUNITY_URL" --token "$ALICE_TOKEN" --path "$ALICE" --json
kp connect claude-code --project "$ALICE_PROJ" --path "$ALICE" --json

# Bob 一次性设置
kp init --path "$BOB" --owner bob --name 'Bob 个人库'
kp community join "$COMMUNITY_URL" --token "$BOB_TOKEN" --path "$BOB" --json
kp connect codex --project "$BOB_PROJ" --path "$BOB" --as bob --json
```

### 2. 在各自会话中自然说话（后台自动抽取、同步与推导）

第一次 Hook 触发时若本机 `kp daemon` 尚未运行，Hook 立即在后台无阻塞拉起守护进程（`0ms` 等待）并返回。多成员可见的事实（如成员关系与已确认的团队规矩）自动进入发件箱推向社区；仅对本人可见的事实（如 Alice 自己的日程事件）永远不出本机：

```sh
cat > "$LAB/alice-session.jsonl" <<'JSONL'
{"type":"user","timestamp":"2026-11-22T10:00:00+08:00","message":{"role":"user","content":"当前已进入数据库发布维护窗口，正在升级核心服务。"}}
{"type":"assistant","timestamp":"2026-11-22T10:00:05+08:00","message":{"role":"assistant","content":"收到，已记录维护安排。"}}
JSONL

cat > "$LAB/bob-session.jsonl" <<'JSONL'
{"type":"response_item","timestamp":"2026-11-22T10:01:00+08:00","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Alice 是我们发布值班组的成员；遇到数据库发布维护窗口都禁止执行 sqlx migrate。"}]}}
{"type":"response_item","timestamp":"2026-11-22T10:01:05+08:00","payload":{"type":"agent_message","message":"好的，已记下。"}}
JSONL

# 第一次 Hook 自动拉起各自本机的 kp daemon；SessionEnd 立即在后台完成成批抽取与社区同步
printf '{"session_id":"alice-m1-01","transcript_path":"%s"}\n' "$LAB/alice-session.jsonl" \
  | kp hook session-end --client claude-code --path "$ALICE" --as alice

printf '{"session_id":"bob-m1-01","transcript_path":"%s"}\n' "$LAB/bob-session.jsonl" \
  | kp hook session-end --client codex --path "$BOB" --as bob

# 若 Bob 的规矩作用范围存在歧义，默认仅在 Bob 本机生效并挂入 kp review；Bob 确认一次即自动推送社区：
for REV_ID in $(kp review list --path "$BOB" --as bob --json | python3 -c 'import json,sys; print(*[r["id"] for r in json.load(sys.stdin) if r["kind"]=="ambiguous_scope"])'); do
  kp review resolve "$REV_ID" --choice confirm --path "$BOB" --as bob --json
done
```

### 3. 看到提示词前注入与双端 `PreToolUse` 守护

Alice 的守护进程在后台自动拉回 Bob 的成员关系与规矩，对齐开放概念并在 Alice 本机推导出 `CollisionWarning`（Bob 端看不到 Alice 的个人事件）。当 Alice 下一次输入相关提示词或触发工具调用时，自动返回带双方人类原话出处（`↳`）的提醒与守护：

```sh
# UserPromptSubmit：在 <= 250 tokens、<= 3 条硬预算内注入结论及双方人类原话依据（无关提示词或无关成员 Carol 返回 {}）
printf '%s\n' '{"prompt":"数据库发布维护窗口期间准备执行 sqlx migrate 更新表结构"}' \
  | kp hook user-prompt-submit --path "$ALICE" --as alice

# Claude Code PreToolUse：只读返回 permissionDecision = "ask" 与 ↳ 原话依据链
printf '%s\n' '{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}' \
  | kp hook pre-tool-use --client claude-code --path "$ALICE" --as alice

# Codex PreToolUse：返回 permissionDecision = "deny"，提示用户执行 kp allow <kp-ref> 单次放行
DENY_OUT=$(printf '%s\n' '{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}' \
  | kp hook pre-tool-use --client codex --path "$ALICE" --as alice)
printf '%s\n' "$DENY_OUT"

printf '%s' "$DENY_OUT" \
  | python3 -c 'import json,re,sys; r=json.load(sys.stdin)["hookSpecificOutput"]["permissionDecisionReason"]; print(*re.search(r"kp allow ([^（。\n]+)", r).group(1).split())' \
  | xargs kp allow --path "$ALICE" --as alice --json
printf '%s\n' '{"tool_name":"Bash","tool_input":{"command":"sqlx migrate run"}}' \
  | kp hook pre-tool-use --client codex --path "$ALICE" --as alice

# 演练结束后停止守护进程与社区服务并清理临时目录：
kp daemon stop --path "$ALICE" --json
kp daemon stop --path "$BOB" --json
kill "$M1_SERVER_PID" 2>/dev/null || true
wait "$M1_SERVER_PID" 2>/dev/null || true
rm -rf "$LAB"
```

若事后发现某条抽取出的规矩有误，作者或库管理员可执行 `kp reject <fact_id> --reason '原因' --path "$BOB" --as bob --json`：该命令在撤回事实及自动同步撤回单的同时写入双钥匙粘性拒收记录，后续重抽同一会话原话也不会死灰复燃。

## ⑪ 公开写入契约、Markdown 无损写回、本机查看网页与真实会话 `kp dream`

除上述全流程外，日常使用中最常用的四组命令如下：

```sh
# 1. 一键运行 5 分钟离线全功能体验（指定 --dir 可保留生成的示例库供自由探索）
kp demo --dir "$LAB/demo_lib"

# 2. 公开 JSONL 写入契约（详见 docs/INGEST-SCHEMA.md）与向量服务自检
kp providers list
cat <<'JSONL' | kp ingest - --path "$ALICE" --as alice --json
{"source":{"kind":"note","uri":"note://ops/1","external_id":"ops-1"},"speaker":"alice","audience":{"explicit":["alice"]},"occurred_at":"2026-11-23T09:00:00Z","content":"灰度发布前必须先核对只读副本延迟指标。","op":"upsert"}
JSONL

# 3. Markdown 无损导出、乐观锁写回与变更日志
kp export "$LAB/alice_md" --path "$ALICE" --as alice --json
kp apply "$LAB/alice_md" --path "$ALICE" --as alice --json
kp log --limit 10 --path "$ALICE" --json

# 4. 本机 127.0.0.1 四栏只读查看网页与真实会话自动沉淀技能
kp inspect --path "$ALICE" --as alice --json
kp dream --sessions "$LAB" --path "$ALICE" --json
```

## 运行完整验收

在仓库根目录执行：

```sh
cargo test -p kipo --test m5_full_journey -- --nocapture
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo fmt --all -- --check
```

验收自动使用临时目录、临时本机回环端口和离线测试夹具，无需任何外部云端密钥。覆盖多成员协作旅程、`Carol` 的逐字节响应隔离、双端 `PreToolUse` 守护、公开 `kp ingest` 契约、Markdown 导出写回、真实会话 `kp dream` 技能沉淀与回滚，以及 SQLite、FTS5、向量与 `raw_blobs` 的零残留擦除。
