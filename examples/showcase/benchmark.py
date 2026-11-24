#!/usr/bin/env python3
"""Run a pinned LongMemEval-S retrieval subset through the public kp CLI.

Prepared data has three owners: history/ contains only neutral-named source
transcripts, questions.json contains queries, and private-gold.json contains
evaluation labels. Only history/ is passed to kp add. This is session retrieval,
not LongMemEval answer accuracy or a reproduction of its user-only retriever.
"""

import argparse
import collections
import datetime
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import sys
import time
import urllib.request
import uuid


DATASET_REVISION = "98d7416c24c778c2fee6e6f3006e7a073259d48f"
DATASET_SHA256 = "d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442"
DATASET_BYTES = 277383467
DATASET_URL = (
    "https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/"
    + DATASET_REVISION + "/longmemeval_s_cleaned.json"
)
SEED = "kipo-showcase-2026-11-25-v1"
LOCAL_MODEL = "local:bge-small-zh-v1.5"
KS = (1, 5, 10)
DEFAULT_OUTPUT = Path("outputs/kipo-showcase-2026-11-25/benchmark")


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def stratum(case):
    return "abstention" if "_abs" in case["question_id"] else case["question_type"]


def selection_hash(case):
    return hashlib.sha256((case["question_id"] + "\0" + SEED).encode()).hexdigest()


def select_cases(data, limit):
    buckets = collections.defaultdict(list)
    for case in data:
        buckets[stratum(case)].append(case)
    for rows in buckets.values():
        rows.sort(key=lambda case: (selection_hash(case), case["question_id"]))
    ordered = []
    for index in range(max(map(len, buckets.values()))):
        for name in sorted(buckets):
            if index < len(buckets[name]):
                ordered.append(buckets[name][index])
    return ordered if limit == 0 else ordered[:limit]


def obtain_dataset(path):
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        partial = path.with_suffix(".download-" + uuid.uuid4().hex)
        print(f"Downloading {DATASET_BYTES:,} bytes from the pinned official release", flush=True)
        with urllib.request.urlopen(DATASET_URL, timeout=120) as source, partial.open("wb") as out:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                out.write(block)
        if partial.stat().st_size != DATASET_BYTES or sha256(partial) != DATASET_SHA256:
            raise ValueError(f"Dataset verification failed; retained {partial}")
        partial.replace(path)
    if path.stat().st_size != DATASET_BYTES or sha256(path) != DATASET_SHA256:
        raise ValueError("Dataset bytes or SHA256 differ from the frozen release")
    data = json.loads(path.read_text())
    if len(data) != 500 or len({case["question_id"] for case in data}) != 500:
        raise ValueError("Expected 500 unique benchmark questions")
    return data


def prepare(dataset, output, limit):
    cases = select_cases(obtain_dataset(dataset), limit)
    frozen = json.loads(Path(__file__).with_name("benchmark-manifest.json").read_text())
    if [case["question_id"] for case in cases[:7]] != frozen["smoke_question_ids"][:len(cases[:7])]:
        raise ValueError("Selection differs from the frozen pre-run manifest")
    destination = output / ("prepared-all" if limit == 0 else f"prepared-{limit}")
    questions, gold, manifest = [], {}, []
    for number, case in enumerate(cases, 1):
        case_id = f"case-{number:04d}"
        ids, dates, sessions = (case[key] for key in
                                ("haystack_session_ids", "haystack_dates", "haystack_sessions"))
        if not (len(ids) == len(dates) == len(sessions)):
            raise ValueError(f"Invalid aligned history for {case['question_id']}")
        history = destination / case_id / "history"
        history.mkdir(parents=True, exist_ok=True)
        source_map, source_files, canonical_sessions, evaluation_sessions = {}, [], {}, {}
        total_bytes = total_words = total_turns = 0
        for index, (original_id, date, turns) in enumerate(zip(ids, dates, sessions), 1):
            neutral_id = f"session-{index:04d}"
            text = "Session date: " + date + "\n\n" + "\n\n".join(
                turn["role"] + ":\n" + turn["content"] for turn in turns
            ) + "\n"
            path = history / (neutral_id + ".txt")
            path.write_text(text)
            source_map[neutral_id] = original_id
            canonical_sessions.setdefault(original_id, neutral_id)
            evaluation_sessions[neutral_id] = canonical_sessions[original_id]
            source_files.append({"name": path.name, "sha256": sha256(path)})
            total_bytes += len(text.encode())
            total_words += len(text.split())
            total_turns += len(turns)
        expected_files = {item["name"] for item in source_files}
        if {path.name for path in history.iterdir()} != expected_files:
            raise ValueError(f"Unexpected files in {history}; use a clean --output directory")
        questions.append({"case_id": case_id, "question": case["question"],
                          "question_date": case["question_date"]})
        gold[case_id] = {
            "question_id": case["question_id"], "stratum": stratum(case),
            "original_session_ids": source_map,
            "evaluation_sessions": evaluation_sessions,
            "gold_sessions": [canonical_sessions[original] for original in set(case["answer_session_ids"])
                              if original in canonical_sessions],
            "abstention": "_abs" in case["question_id"],
        }
        if not gold[case_id]["abstention"] and not gold[case_id]["gold_sessions"]:
            raise ValueError("Answerable question has no evidence sessions")
        manifest.append({"case_id": case_id, "question_id": case["question_id"],
                         "stratum": stratum(case), "selection_hash": selection_hash(case),
                         "sessions": len(ids), "turns": total_turns,
                         "unique_session_ids": len(set(ids)),
                         "source_bytes": total_bytes, "whitespace_tokens": total_words,
                         "files": source_files})
    write_json(destination / "questions.json", questions)
    write_json(destination / "private-gold.json", gold)
    (destination / "private-gold.json").chmod(0o600)
    write_json(destination / "manifest.json", {
        "schema_version": 1, "dataset_revision": DATASET_REVISION,
        "dataset_sha256": DATASET_SHA256, "seed": SEED,
        "selection": "round-robin sorted strata; within stratum SHA256(question_id + NUL + seed)",
        "token_count_method": "Python str.split; whitespace units, NOT model tokenizer tokens",
        "full_history": True, "messages": "user and assistant", "cases": manifest,
    })
    return destination


def command(binary, library, args, env, logs, name, timeout):
    argv = [str(binary), "--path", str(library), "--json", *args]
    started = time.perf_counter()
    completed = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=timeout)
    elapsed = (time.perf_counter() - started) * 1000
    write_json(logs / (name + ".json"), {
        "argv": argv, "elapsed_ms": elapsed, "returncode": completed.returncode,
        "stdout": completed.stdout, "stderr": completed.stderr,
    })
    if completed.returncode:
        raise RuntimeError(f"{name} failed ({completed.returncode}); see {logs / (name + '.json')}")
    return json.loads(completed.stdout), elapsed


def session_metrics(ranking, gold):
    if not gold:
        raise ValueError("Retrieval metrics require nonempty evidence labels")
    result = {}
    for k in KS:
        recalled = set(ranking[:k])
        result[f"recall_any@{k}"] = int(bool(recalled & gold))
        result[f"recall_all@{k}"] = int(gold <= recalled)
    result["reciprocal_rank@10"] = next(
        (1 / rank for rank, session in enumerate(ranking[:10], 1) if session in gold), 0
    )
    return result


def run_benchmark(binary, prepared, output, embedding, chunk_limit, repetitions, timeout):
    questions = json.loads((prepared / "questions.json").read_text())
    gold = json.loads((prepared / "private-gold.json").read_text())
    manifest = json.loads((prepared / "manifest.json").read_text())
    run_id = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:6]
    run_dir = output / "runs" / (run_id + "-" + embedding)
    run_dir.mkdir(parents=True)
    env = dict(os.environ, KP_MODEL_POLICY="raw_only", KP_NO_AUTO_DREAM="1",
               KP_DISABLE_MODEL_DOWNLOAD="1")
    summaries = []
    for question, case in zip(questions, manifest["cases"]):
        case_id = question["case_id"]
        library, logs = run_dir / case_id / "library", run_dir / case_id / "commands"
        history = prepared / case_id / "history"
        result = {key: value for key, value in case.items() if key != "files"}
        try:
            for file in case["files"]:
                if sha256(history / file["name"]) != file["sha256"]:
                    raise ValueError("Prepared input changed since manifest was written")
            init_args = ["init", "--owner", "benchmark", "--name", "LongMemEval case"]
            init_args += ["--no-embedding"] if embedding == "none" else ["--embedding-model", LOCAL_MODEL]
            init, init_ms = command(binary, library, init_args, env, logs, "init", timeout)
            if embedding == "none" and (init["embedding_model"] != "none" or init["embedding_dims"] != 0):
                raise ValueError("Lexical-only library unexpectedly enabled embeddings")
            added, add_ms = command(binary, library, ["add", str(history)], env, logs, "add", timeout)
            if not isinstance(added, list) or len(added) != case["sessions"]:
                raise ValueError("Not every history session was imported successfully")
            source_to_session = {item["source"]["source_id"]: Path(item["uri"]).stem for item in added}
            if len(source_to_session) != case["sessions"] or any(item["chunks"] == 0 for item in added):
                raise ValueError("History import produced duplicate sources or empty chunks")
            result.update(init_ms=init_ms, ingest_ms=add_ms,
                          chunks=sum(item["chunks"] for item in added),
                          embedding_model=init["embedding_model"], embedding_dims=init["embedding_dims"])
            if embedding == "local":
                embedded, embed_ms = command(binary, library, ["embed", "--stale"], env, logs, "embed", timeout)
                if embedded["model_id"] != LOCAL_MODEL or embedded["dims"] <= 0:
                    raise ValueError("Requested real local embeddings were not used")
                if embedded["chunks_reembedded"] + embedded["chunks_up_to_date"] != result["chunks"]:
                    raise ValueError("Some chunks lack real local embeddings")
                result.update(embedding_verification=embedded, embed_verification_ms=embed_ms)
            outputs, timings = [], []
            for trial in range(repetitions):
                retrieved, elapsed = command(binary, library,
                    ["search", question["question"], "--limit", str(chunk_limit)],
                    env, logs, f"search-{trial + 1}", timeout)
                outputs.append(retrieved)
                timings.append(elapsed)
            ranking = []
            for hit in outputs[0]["results"]:
                anchor = hit.get("anchor") or {}
                source_id = anchor.get("source_id")
                if source_id not in source_to_session:
                    raise ValueError(f"Search hit lacks a mapped public source anchor: {hit['id']}")
                neutral_id = gold[case_id]["evaluation_sessions"][source_to_session[source_id]]
                if neutral_id not in ranking:
                    ranking.append(neutral_id)
            stable = all([hit["id"] for hit in value["results"]] ==
                         [hit["id"] for hit in outputs[0]["results"]] for value in outputs)
            result.update(status="ok", retrieved_chunks=len(outputs[0]["results"]),
                          ranked_sessions=ranking, cli_search_ms=timings, repeated_ranking_stable=stable,
                          sqlite_bytes=sum(path.stat().st_size for path in library.glob("kp.sqlite*")))
            if gold[case_id]["abstention"]:
                result.update(metrics=None, excluded_from_retrieval_metrics="abstention has no answer target")
            else:
                result["metrics"] = session_metrics(ranking, set(gold[case_id]["gold_sessions"]))
        except (RuntimeError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
            result.update(status="error", error=str(error), metrics=None)
        summaries.append(result)
        write_json(run_dir / "cases.json", summaries)
        print(f"{case_id} {result['stratum']}: {result['status']}", flush=True)
    scored = [case["metrics"] for case in summaries if case.get("metrics") is not None]
    latencies = [ms for case in summaries for ms in case.get("cli_search_ms", [])]
    errors = sum(case["status"] != "ok" for case in summaries)
    complete = errors == 0 and len(summaries) == len(questions)
    report = {
        "protocol": "custom all-message session retrieval from native kp chunks; first occurrence deduplication",
        "claim": "retrieval smoke test only; NOT end-to-end QA, official user-only retrieval, or a leaderboard score",
        "dataset_revision": DATASET_REVISION, "dataset_sha256": DATASET_SHA256,
        "selection_manifest": str(prepared / "manifest.json"), "selection_manifest_sha256": sha256(prepared / "manifest.json"),
        "binary": str(binary), "binary_sha256": sha256(binary), "platform": platform.platform(),
        "embedding": embedding, "mode": "lexical-only" if embedding == "none" else "local-onnx-plus-lexical",
        "requested_cases": len(questions), "completed_cases": len(summaries), "errors": errors,
        "scored_answerable_cases": len(scored), "excluded_abstention_cases": sum(gold[q["case_id"]]["abstention"] for q in questions),
        "complete": complete,
        "aggregate_metrics": {key: statistics.mean(row[key] for row in scored) for key in scored[0]} if scored and complete else None,
        "chunk_candidate_limit": chunk_limit, "session_k": list(KS), "query_repetitions": repetitions,
        "timing_note": "Wall clock includes new CLI process, DB open, tokenizer/model startup and search; descriptive observations, not a performance comparison",
        "cli_search_ms": {"count": len(latencies), "median": statistics.median(latencies),
                          "min": min(latencies), "max": max(latencies),
                          "p95_nearest_rank": sorted(latencies)[math.ceil(len(latencies) * .95) - 1]} if latencies else None,
        "cases": summaries,
    }
    write_json(run_dir / "report.json", report)
    write_json(output / ("latest-" + embedding + ".json"), {"report": str(run_dir / "report.json")})
    print(json.dumps({"report": str(run_dir / "report.json"), "complete": complete,
                      "aggregate_metrics": report["aggregate_metrics"]}, indent=2))
    return 0 if complete else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepare", action="store_true", help="Download, verify and prepare source-only histories")
    parser.add_argument("--run", action="store_true", help="Prepare and run isolated libraries through kp")
    parser.add_argument("--limit", type=int, default=7, help="Number of hash-selected cases; 7 = one per stratum; 0 = all 500")
    parser.add_argument("--dataset", type=Path, default=DEFAULT_OUTPUT / "cache/longmemeval_s_cleaned.json")
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    parser.add_argument("--kp", type=Path, default=Path(__file__).resolve().parents[2] / "target/release/kp")
    parser.add_argument("--embedding", choices=["none", "local"], default="none")
    parser.add_argument("--chunk-limit", type=int, default=1000, help="Returned native chunk budget before session deduplication")
    parser.add_argument("--repetitions", type=int, default=1, help="Search calls per case; scores use the first call")
    parser.add_argument("--timeout", type=int, default=600, help="Per CLI command timeout in seconds")
    args = parser.parse_args()
    if not args.prepare and not args.run:
        parser.error("choose --prepare, --run, or both")
    if args.limit < 0 or args.limit > 500 or args.chunk_limit < 10 or args.repetitions < 1:
        parser.error("limit must be 0..500, chunk-limit >= 10, repetitions >= 1")
    prepared = prepare(args.dataset.resolve(), args.output.resolve(), args.limit)
    print(f"Prepared {prepared}", flush=True)
    if args.run:
        return run_benchmark(args.kp.resolve(), prepared, args.output.resolve(), args.embedding,
                             args.chunk_limit, args.repetitions, args.timeout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
