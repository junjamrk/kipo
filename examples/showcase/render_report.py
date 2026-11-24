#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Create a portable, offline evidence dashboard from actual showcase receipts."""
import argparse
import json
from pathlib import Path

HERE = Path(__file__).resolve().parent


def read(path):
    return json.loads(path.read_text()) if path.exists() else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    root = args.evidence.resolve()
    manifest = read(HERE / "scenario_manifest.json")
    benchmarks = []
    for mode in ["none", "local"]:
        reports = sorted((root / "benchmark/runs").glob(f"*-{mode}/report.json"))
        if reports:
            report = read(reports[-1])
            benchmarks.append({key: report[key] for key in ["mode", "completed_cases", "errors", "scored_answerable_cases", "aggregate_metrics", "claim"]})
            benchmarks[-1]["evidence"] = str(reports[-1].relative_to(root))
    scenarios = []
    for name in ["scenario-original", "scenario-original-live", "scenario-original-local", "scenario-raw", "scenario-live"]:
        path = root / name / "result.json"
        report = read(path)
        if report:
            scenarios.append({key: report.get(key) for key in ["mode", "transformation", "complete", "recall_all", "recall_any", "answerable_question_count", "source_count", "model_failure_commands", "generated_fact_ids", "error"]})
            scenarios[-1].update(name=name, evidence=str(path.relative_to(root)), questions=[
                {key: q.get(key) for key in ["id", "phase", "query", "required_source_ids", "retrieved_source_ids", "recall_all", "unanswerable", "unresolvable_hits"]}
                for q in report.get("questions", [])])
            if scenarios[-1]["transformation"] == "human-paraphrase":
                scenarios[-1]["transformation"] = "agent-curated-paraphrase (旧标签已纠正)"
    governance = read(root / "governance/assertions.json")
    interfaces = read(root / "interfaces/assertions.json")
    if interfaces is None:
        interfaces = read(root / "interfaces/result.json")
    if interfaces is None:
        paths = sorted(root.glob("interfaces-run-*/result.json"))
        if paths:
            interfaces = read(paths[-1])
            interfaces["receipt"] = str(paths[-1].relative_to(root))
    data = dict(manifest=manifest, benchmarks=benchmarks, scenarios=scenarios,
                governance=governance, interfaces=interfaces,
                answers=read(root / "answers-original/result.json"))
    output = args.out or root / "showcase.html"
    output.parent.mkdir(parents=True, exist_ok=True)
    payload = json.dumps(data, ensure_ascii=False).replace("</", "<\\/")
    output.write_text(TEMPLATE.replace("__DATA__", payload))
    print(output.resolve())


TEMPLATE = r'''<!doctype html>
<html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Kipo · 真实证据工作台</title>
<style>
:root{--bg:#f5f4ee;--paper:#fffefa;--ink:#182b27;--muted:#60736a;--line:#d9dfd5;--green:#195b46;--red:#ae3e35;--gold:#986915}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.65 system-ui,-apple-system,sans-serif}header{background:#173b30;color:#fff;padding:44px max(6vw,24px) 30px}header small{color:#b9d8b8;letter-spacing:2px}h1{font-size:38px;line-height:1.25;max-width:940px;margin:14px 0}h2{font-size:24px}header p{color:#d1dfd5;max-width:900px}main{max-width:1240px;margin:auto;padding:26px 24px 64px}nav{display:flex;gap:10px;flex-wrap:wrap;margin-bottom:26px}button,select{font:inherit;padding:8px 16px;border:1px solid var(--line);border-radius:7px;background:var(--paper);color:var(--ink);cursor:pointer}button.active{background:var(--green);color:white;border-color:var(--green)}section{display:none}section.active{display:block}.grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:16px}.card{padding:20px;background:var(--paper);border:1px solid var(--line);border-radius:10px}.num{font-size:32px;font-weight:700;color:var(--green);line-height:1.3}.sub{font-size:13px;color:var(--muted)}.callout{padding:16px 20px;border-left:4px solid var(--gold);background:#f4eddc;margin:20px 0}.bad{color:var(--red)}.good{color:var(--green)}.badge{font-size:12px;border-radius:20px;padding:3px 10px;background:#e5ebe2;display:inline-block}table{width:100%;border-collapse:collapse;margin-top:16px;background:var(--paper)}th,td{padding:13px;text-align:left;border-bottom:1px solid var(--line);vertical-align:top}th{font-size:13px;color:var(--muted)}a{color:var(--green);text-underline-offset:3px}details{border-bottom:1px solid var(--line);padding:14px 0}summary{cursor:pointer;font-weight:600}code{font-size:12px;overflow-wrap:anywhere}.timeline{border-left:2px solid #90ad95;margin-left:12px;padding-left:24px}.phase{position:relative;margin:25px 0}.phase:before{content:'';position:absolute;width:12px;height:12px;background:#31735c;border-radius:50%;left:-31px;top:10px}.phase h3{margin:0 0 6px}.event{padding:12px 0}.event p{margin:8px 0;color:#40594d}.flow{display:flex;gap:8px;align-items:center;flex-wrap:wrap;padding:20px 0}.flow span{border:1px solid var(--line);border-radius:8px;padding:10px;background:white}.flow b{color:var(--muted)}footer{color:var(--muted);margin-top:35px;border-top:1px solid var(--line);padding-top:20px}pre{overflow:auto;background:#182b27;color:#e5efde;padding:16px;border-radius:8px}#checks .card{margin:10px 0}.controls{display:flex;gap:10px;align-items:center;flex-wrap:wrap}.empty{padding:30px;background:var(--paper);border:1px dashed var(--line)}@media(max-width:760px){.grid{grid-template-columns:1fr}h1{font-size:28px}table{font-size:13px}th,td{padding:8px}header{padding:30px 24px}main{padding:20px 16px}}
</style>
<header><small>KIPO / EVIDENCE WORKBENCH</small><h1>当团队的旧经验失效，<br>Agent 能否知道依据变了？</h1><p>一场真实的 Next.js 缓存故障讨论，加一套独立的公开检索基准。每个数字来自运行记录，事实、模型抽取与受控实验分别呈现。</p></header>
<main><nav><button data-tab="case" class="active">真实事件</button><button data-tab="answers">Agent 回答</button><button data-tab="benchmark">Benchmark</button><button data-tab="controls">治理与接口</button><button data-tab="scope">演示边界</button></nav>
<section id="case" class="active"><div class="grid"><div class="card"><div class="num" id="sources"></div><div>公开原始材料</div><div class="sub">同一 issue 的评论 + 关联 PR</div></div><div class="card"><div class="num">9</div><div>证据可用阶段</div><div class="sub">按当前正文的最后编辑时间排序</div></div><div class="card"><div class="num">12</div><div>预先冻结的问题</div><div class="sub">11 题有依据，1 题应拒答且未评分</div></div></div>
<div class="flow"><span>社区评论 / PR</span><b>→</b><span>公开 ingest · L0 原件</span><b>→</b><span>真实模型抽取 / 来源锚点</span><b>→</b><span>search / why</span><b>→</b><span>Agent 消费证据</span></div>
<div class="callout">这是 2023 年事件的证据回放，不是当前 Next.js 使用建议。评论的旧版本不可得，因此不把编辑后的正文冒充最初发布时间的知识。普通社区建议也不等于维护者批准。</div>
<h2>实跑结果</h2><div id="scenario-summary"></div><h2>查看问题与来源</h2><div class="controls"><select id="run-select" aria-label="选择运行"></select><span class="sub">@5 是五个原生 hit；标注集并非唯一充分依据，覆盖不代表回答正确。</span></div><div id="questions"></div>
<h2>真实变化的证据链</h2><div class="timeline" id="timeline"></div></section>
<section id="answers"><h2>Agent 实际怎样使用检索证据</h2><p>真实 Codex 逐题消费原文词法检索的 top 5。每题独立上下文，只接收问题、片段与来源坐标；不接收 gold，也不读取未来阶段的证据。</p><div class="callout">模型消费者是示例层，不是 Kipo 内置 QA API。调用和格式成功不代表回答正确；作者字段缺失、目标方案歧义、召回遗漏仍可能影响答案。</div><div id="answer-list"></div><p><a href="answers-original/result.json">模型调用回执</a> · <a href="live-quality-review.md">抽取质量审阅</a> · <a href="answers-review.md">回答质量审阅</a></p></section>
<section id="benchmark"><h2>LongMemEval-S：先测能否找全依据</h2><p>固定官方数据 revision 与 SHA256，六类问题各取一题，再加一题拒答诊断。每题导入完整历史，问题、gold 与答案标记不进入索引。</p><div id="benchmark-table"></div><div class="callout">这是 7 题 smoke，不是 500 题全量成绩，也不是官方 user-only 检索协议或端到端问答准确率。输入包含 user + assistant；拒答题不进入检索分母。两种模式的候选池不同，不能将差异归因于向量本身。</div><div class="grid"><div class="card"><div class="num">352</div><div>原始 session 记录</div></div><div class="card"><div class="num">3,689</div><div>消息轮次</div></div><div class="card"><div class="num">2,634</div><div>Kipo 原生 chunks</div></div></div><p>local 档使用真实 <code>local:bge-small-zh-v1.5</code>、512 维 ONNX 向量，并通过公共 <code>embed --stale</code> 核验。none 档只用词法检索。所有耗时包括 CLI 启动与数据库打开，单次测量不作性能结论。</p><p>基准依据：<a href="https://github.com/xiaowu0162/LongMemEval">LongMemEval 官方仓库</a> · <a href="https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned">官方数据</a></p></section>
<section id="controls"><h2>让边界真的失败一次</h2><p>以下使用明确标记的合成输入，验证权限与生命周期。它们不是 Next.js 社区真实发生的审批，也不用于证明模型智能。</p><div id="counts" class="grid"></div><div class="controls" style="margin-top:20px"><button id="failed-only">只看失败与未覆盖</button><button id="all-checks">查看全部</button></div><div id="checks"></div><h2>接口记录</h2><div id="interfaces"></div></section>
<section id="scope"><h2>如何读这份 showcase</h2><div class="card"><p><b>真实部分：</b>固定公开原始正文，保留作者、URL、正文哈希与编辑时间；使用真实 CLI 入口、真实已登录模型。人工预写的结论没有注入真实案例库。</p><p><b>摘要对照：</b>仓库中的英文摘要由 Agent 整理，用作可离线阅读的叙事对照；不称为原始聊天或人工标注的权威数据。</p><p><b>指标边界：</b>来源召回、模型成功抽取、断言通过和回答正确是四件事。没有独立答案评审的地方明确显示未评分。</p><p><b>覆盖方式：</b>检索/来源/时间变化放进真实案例；权限、审批、回写、反馈等放进受控实验。Office/PDF/OCR、远程真实连接器、自然发生的审批与自动学习不能由此推定已验证。</p><p><b>当前缺口：</b>本地 CLI 普通成员能 forget 共享 owner fact 的红灯保留。HTTP 路径单独测试；不要扩写为未经验证的远程漏洞。</p></div><h2>可复跑入口</h2><pre>python3 examples/showcase/benchmark.py --run --limit 7 --embedding none
python3 examples/showcase/scenario.py --out /tmp/kipo-case --fetch-originals
python3 examples/showcase/governance.py --output /tmp/kipo-controls</pre><p>完整命令、真实模型模式和语义边界见仓库 <code>examples/showcase/README.md</code>。</p></section>
<footer>读指标，也读失败。报告仅汇总已有回执；没有 result 的运行不会显示为成功。公开材料留在本地缓存，仓库只分发出处、摘要与执行器。</footer></main>
<script id="evidence" type="application/json">__DATA__</script><script>
const D=JSON.parse(document.querySelector('#evidence').textContent),E=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c])),pct=x=>x==null?'未评分':(x*100).toFixed(1)+'%';
document.querySelectorAll('nav button').forEach(b=>b.onclick=()=>{document.querySelectorAll('nav button,section').forEach(e=>e.classList.remove('active'));b.classList.add('active');document.getElementById(b.dataset.tab).classList.add('active')});
document.querySelector('#sources').textContent=D.manifest.events.length;
document.querySelector('#timeline').innerHTML=D.manifest.phases.map(p=>`<div class="phase"><h3>${p.phase}. ${E(p.title)}</h3><div class="sub">正文最晚可用时点 ${E(p.as_of)}</div>${D.manifest.events.filter(e=>e.phase===p.phase).map(e=>`<details class="event"><summary>${E(e.author)} · ${E(e.id)}</summary><p>${E(e.text)}</p><a href="${E(e.url)}" target="_blank" rel="noopener">查看原始来源 ↗</a><div class="sub">发布 ${E(e.created_at)} · 编辑 ${E(e.updated_at)}</div><code>SHA256 ${E(e.original_sha256)}</code></details>`).join('')}</div>`).join('');
document.querySelector('#scenario-summary').innerHTML='<table><tr><th>输入 / 模式</th><th>覆盖全部标注来源 @5</th><th>找到至少一条 @5</th><th>运行回执</th></tr>'+D.scenarios.map(s=>`<tr><td>${E(s.name)}<br><span class="sub">${E(s.transformation)} · ${s.generated_fact_ids?.length||0} 个生成 fact ID</span></td><td>${pct(s.recall_all)}</td><td>${pct(s.recall_any)}</td><td><a href="${E(s.evidence)}">${s.complete?'流程完成':'未完整完成'}</a>${s.model_failure_commands?.length?'<br><span class="bad">模型失败 '+s.model_failure_commands.length+' 次</span>':''}</td></tr>`).join('')+'</table>';
const sel=document.querySelector('#run-select');sel.innerHTML=D.scenarios.map((s,i)=>`<option value="${i}">${E(s.name)}</option>`).join('');function questions(){const s=D.scenarios[Number(sel.value)];document.querySelector('#questions').innerHTML=s?s.questions.map(q=>`<details><summary><span class="${q.unanswerable?'':q.recall_all?'good':'bad'}">${q.unanswerable?'未评分':q.recall_all?'来源齐全':'来源未齐'}</span> · ${E(q.query)}</summary><p>阶段 ${q.phase} · 需要：${E(q.required_source_ids?.join(', '))}</p><p>召回：${E(q.retrieved_source_ids?.join(', '))}</p><p class="sub">回答正确性与拒答行为未评分。${q.unresolvable_hits?.length?'部分 hit 的来源坐标无法解析。':''}</p></details>`).join(''):'<div class="empty">尚无完成的运行回执。</div>'}sel.onchange=questions;questions();
document.querySelector('#answer-list').innerHTML=D.answers?(D.answers.answers||[]).map(r=>`<details><summary>${E(r.query)} <span class="badge">${r.answer?.abstain?'拒答':'回答'}</span></summary><p>${E(r.answer?.answer||r.errors?.join('; ')||'没有回答')}</p><p>${(r.answer?.citations||[]).map(c=>`<a href="${E(c.source)}" target="_blank" rel="noopener">${E(c.relevance)}</a>`).join('<br>')}</p><div class="sub">phase ${r.phase} · 调用状态 ${E(r.status)} · 质量另行审阅</div></details>`).join(''):'<div class="empty">尚无完整模型回执；不展示预写答案。</div>';
document.querySelector('#benchmark-table').innerHTML='<table><tr><th>检索模式</th><th>Any @10</th><th>All @10</th><th>MRR @10</th><th>证据</th></tr>'+D.benchmarks.map(b=>`<tr><td>${E(b.mode)}</td><td>${pct(b.aggregate_metrics['recall_any@10'])}</td><td>${pct(b.aggregate_metrics['recall_all@10'])}</td><td>${b.aggregate_metrics['reciprocal_rank@10'].toFixed(3)}</td><td><a href="${E(b.evidence)}">${b.completed_cases}题 / ${b.errors}执行错误</a></td></tr>`).join('')+'</table>';
const G=D.governance;if(G){document.querySelector('#counts').innerHTML=['passed','failed','not_exercised'].map((k,i)=>`<div class="card"><div class="num ${k==='failed'?'bad':''}">${G.counts[k]}</div><div>${['通过','失败','未覆盖'][i]}</div></div>`).join('');function checks(all){document.querySelector('#checks').innerHTML=G.checks.filter(c=>all||c.status!=='passed').map(c=>`<div class="card"><span class="badge ${c.status==='failed'?'bad':''}">${E(c.status)}</span> <b>${E(c.name)}</b><p>${E(typeof c.detail==='object'?JSON.stringify(c.detail):c.detail)}</p><div class="sub">command ${E(c.command_id)} · <a href="governance/commands.jsonl">原始命令日志</a></div></div>`).join('')}document.querySelector('#failed-only').onclick=()=>checks(false);document.querySelector('#all-checks').onclick=()=>checks(true);checks(false)}
document.querySelector('#interfaces').innerHTML=D.interfaces?`<div class="card"><a href="${E(D.interfaces.receipt||'interfaces/assertions.json')}">查看独立接口断言回执</a><p>stdio MCP 与 HTTP MCP 工具面分别验证。远端发布仅指本机回环地址的隔离社区。${D.interfaces.checks?.length||0} 项检查。</p></div>`:'<div class="empty">尚无接口结果；不计为通过。</div>';
</script></html>'''

if __name__ == "__main__":
    main()
