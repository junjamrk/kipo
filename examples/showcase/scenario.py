#!/usr/bin/env python3
"""Replay an attributed public incident through Kipo's actual public ingress.

Gold answers never enter the library or model extraction. Retrieval evidence is
scored; generated-answer correctness is deliberately left unscored.
"""
from __future__ import annotations

import argparse
from datetime import datetime
import hashlib
import json
from pathlib import Path
import sys
import urllib.request

from common import Recorder, digest, source_ids, write_json

HERE = Path(__file__).resolve().parent


def epoch(text):
    return int(datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp() * 1000)


def original_bodies(args, events):
    """Load user-local research caches or fetch pinned public bodies, failing on drift."""
    if args.content == "paraphrase":
        return {event["id"]: event["text"] for event in events}
    cached = {}
    if args.original_comments:
        document = json.loads(args.original_comments.read_text())
        for item in document.get("comments", []) if isinstance(document, dict) else document:
            cached[f"comment-{item['id']}"] = item["body"]
    if args.original_pr:
        cached["pr-48383"] = json.loads(args.original_pr.read_text())["body"]
    result = {}
    for event in events:
        body = cached.get(event["id"])
        if body is None and args.fetch_originals:
            resource = ("pulls/48383" if event["id"] == "pr-48383"
                        else "issues/comments/" + event["id"].removeprefix("comment-"))
            request = urllib.request.Request("https://api.github.com/repos/vercel/next.js/" + resource,
                                             headers={"User-Agent": "kipo-showcase/1", "Accept": "application/vnd.github+json"})
            with urllib.request.urlopen(request, timeout=60) as response:
                body = json.load(response)["body"]
        if body is None:
            raise ValueError(f"Missing original {event['id']}; supply original caches or --fetch-originals")
        actual = hashlib.sha256(body.encode("utf-8")).hexdigest()
        if actual != event["original_sha256"]:
            raise ValueError(f"Original body hash changed for {event['id']}; do not silently replace the frozen corpus")
        result[event["id"]] = body
    return result


def run(args):
    manifest_path, gold_path = HERE / "scenario_manifest.json", HERE / "scenario_gold.json"
    manifest, gold = json.loads(manifest_path.read_text()), json.loads(gold_path.read_text())
    events = manifest["events"]
    event_index = {event["id"]: event for event in events}
    assert len(event_index) == len(events), "Duplicate evidence IDs"
    for event in events:
        assert epoch(event["observed_at"]) >= epoch(event["updated_at"])
    for question in gold["questions"]:
        assert all(event_index[x]["phase"] <= question["after_phase"]
                   for x in question["required_source_ids"]), "Future evidence in gold"
    bodies = original_bodies(args, events)
    recorder = Recorder(args.kp, args.out, "host_cli" if args.mode == "live" else "raw_only")
    if args.host_program:
        recorder.env["KP_HOST_CLI_PROGRAM"] = str(args.host_program.resolve())
        recorder.env["KP_HOST_CLI_KIND"] = args.host_kind
    if args.embedding == "none":
        init_args = ["--no-embedding"]
    else:
        init_args = ["--embedding-model", "local:bge-small-zh-v1.5"]
    recorder.call("init", "--name", manifest["title"], "--owner", "operator", *init_args)
    manifest_hash, gold_hash = digest(manifest_path), digest(gold_path)
    (recorder.output / "manifest.json").write_bytes(manifest_path.read_bytes())
    (recorder.output / "gold.json").write_bytes(gold_path.read_bytes())
    source_to_event, question_results, phase_results = {}, [], []
    model_calls, model_failures, generated_facts = [], [], []
    error = None
    try:
        for phase in manifest["phases"]:
            phase_id = phase["phase"]
            print(f"Phase {phase_id}: {phase['title']}", flush=True)
            payloads = []
            for event in [e for e in events if e["phase"] == phase_id]:
                record = {
                    "source": {"kind": "github", "uri": event["url"], "external_id": event["id"]},
                    "speaker": "github:" + event["author"],
                    "audience": {"explicit": ["operator"]},
                    "occurred_at": event["observed_at"], "content": bodies[event["id"]],
                    "op": "upsert", "thread_key": manifest["case_id"],
                }
                payload = json.dumps(record, ensure_ascii=False) + "\n"
                payloads.append(payload)
                response = recorder.call("ingest", "-", stdin=payload)["parsed"]
                for source in response["source_ids"]:
                    source_to_event[source] = event["id"]
                recorder.check(f"ingest:{event['id']}", response["records_upserted"] == 1)
            write_json(recorder.output / "source-map.json", source_to_event)
            replay = recorder.call("ingest", "-", stdin="".join(payloads))["parsed"]
            recorder.check(f"phase-{phase_id}:duplicate-replay",
                           replay["records_duplicate"] == len(payloads) and replay["records_upserted"] == 0)
            if args.mode == "live":
                extraction = recorder.call("extract", "--max-jobs", "100", "--force", required=False,
                                           timeout=args.model_timeout)
                model_calls.append(extraction["sequence"])
                if extraction["returncode"]:
                    model_failures.append(extraction["sequence"])
                elif extraction["parsed"]:
                    generated_facts.extend(extraction["parsed"].get("created_facts", []))
            for question in [q for q in gold["questions"] if q["after_phase"] == phase_id]:
                response = recorder.call("search", question["query"], "--limit", str(args.top_k))
                hits = (response["parsed"] or {}).get("results", [])
                retrieved, missing_coordinates = [], []
                for hit in hits:
                    found = [source_to_event[s] for s in source_ids(hit) if s in source_to_event]
                    # Why explains logical premises but its JSON does not expose source IDs.
                    # Keep unresolved hits visible instead of guessing provenance from text.
                    if not found:
                        missing_coordinates.append(hit.get("id"))
                    for source in found:
                        if source not in retrieved:
                            retrieved.append(source)
                expected = set(question["required_source_ids"])
                unanswerable = question.get("unanswerable", False)
                question_results.append(dict(
                    id=question["id"], phase=phase_id, query=question["query"],
                    required_source_ids=sorted(expected), retrieved_source_ids=retrieved,
                    recall_all=None if unanswerable else expected.issubset(retrieved),
                    recall_any=None if unanswerable else bool(expected.intersection(retrieved)),
                    source_coverage=None if unanswerable or not expected else len(expected.intersection(retrieved))/len(expected),
                    answer_correctness="not_scored", abstention="not_scored",
                    reason="Search returns evidence, not a generated answer; gold and forbidden claims require separate answer evaluation.",
                    unanswerable=unanswerable, search_command=response["sequence"],
                    unresolvable_hits=missing_coordinates, returned_hit_count=len(hits),
                    hits=hits))
            status = recorder.call("status")["parsed"]
            phase_results.append(dict(**phase, status=status))
        if args.embedding == "local":
            embedding = recorder.call("embed", "--stale")["parsed"]
            write_json(recorder.output / "embedding-verification.json", embedding)
            recorder.check("real-local-embedding", embedding.get("model_id") == "local:bge-small-zh-v1.5"
                           and embedding.get("dims") == 512)
        if args.mode == "live":
            recorder.call("derive", required=False)
            for fact_id in generated_facts[:6]:
                recorder.call("why", fact_id, required=False)
            recorder.call("review", "list", required=False)
        recorder.call("log", "--limit", "30")
    except Exception as exception:
        error = str(exception)
    answerable = [q for q in question_results if not q["unanswerable"]]
    result = recorder.result(
        case_id=manifest["case_id"], mode=args.mode, embedding=args.embedding,
        manifest_sha256=manifest_hash, gold_sha256=gold_hash,
        transformation="original-body" if args.content == "original" else "agent-curated-paraphrase",
        temporal_policy=manifest["temporal_policy"],
        questions=question_results, phases=phase_results, source_count=len(source_to_event),
        top_k_hits=args.top_k, recall_all=(sum(q["recall_all"] for q in answerable)/len(answerable) if answerable else None),
        recall_any=(sum(q["recall_any"] for q in answerable)/len(answerable) if answerable else None),
        answerable_question_count=len(answerable), model_call_commands=model_calls,
        model_failure_commands=model_failures, generated_fact_ids=generated_facts,
        error=error, scope="Curated incident replay; source retrieval, not an E2E QA benchmark.",
        scoring_limit="Coverage of the curator-specified evidence set in top-k native hits. Alternative sufficient evidence may exist. Hits lacking direct source coordinates remain unresolved; this is not answer correctness.",
        complete=error is None and len(question_results) == len(gold["questions"]) and not model_failures)
    print(json.dumps({k: result[k] for k in ["complete", "source_count", "recall_all", "recall_any", "error"]}))
    return 0 if result["complete"] and all(x["passed"] for x in recorder.checks) else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kp", type=Path, default=HERE.parent.parent / "target/release/kp")
    parser.add_argument("--out", type=Path, required=True, help="A new output directory; existing results are never overwritten")
    parser.add_argument("--mode", choices=["raw", "live"], default="raw")
    parser.add_argument("--content", choices=["original", "paraphrase"], default="original")
    parser.add_argument("--original-comments", type=Path, help="Local API snapshot, not redistributed with code")
    parser.add_argument("--original-pr", type=Path)
    parser.add_argument("--fetch-originals", action="store_true", help="Fetch selected public GitHub bodies and verify frozen hashes")
    parser.add_argument("--embedding", choices=["none", "local"], default="none")
    parser.add_argument("--host-program", type=Path)
    parser.add_argument("--host-kind", choices=["codex", "claude"], default="codex")
    parser.add_argument("--top-k", type=int, default=5)
    parser.add_argument("--model-timeout", type=int, default=900)
    raise SystemExit(run(parser.parse_args()))
