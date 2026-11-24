#!/usr/bin/env python3
"""Public-CLI governance controls. All content is synthetic and explicitly labeled.

These checks measure deterministic boundaries, not extraction accuracy, autonomous
learning, production safety, or benchmark quality. No model, external server, real
account, agent configuration, or existing library is used.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import traceback


class Controls:
    def __init__(self, binary: Path, output: Path, work: Path):
        self.binary, self.output, self.work = binary, output, work
        self.assertions: list[dict] = []
        self.commands: list[dict] = []
        self.env = os.environ.copy()
        self.env.update({
            "KP_MODEL_POLICY": "raw_only", "KP_OFFLINE": "1",
            "KP_NO_AUTO_DREAM": "1", "KP_NO_AUTO_DAEMON": "1",
            "KP_DISABLE_MODEL_DOWNLOAD": "1", "KP_DISABLE_AUTO_MODEL_DOWNLOAD": "1",
        })

    def run(self, lib: Path, *args: str, actor="owner", stdin=None, denied=False):
        argv = [str(self.binary), "--path", str(lib), "--as", actor, "--json", *map(str, args)]
        start = time.monotonic()
        result = subprocess.run(argv, input=stdin, text=True, capture_output=True,
                                env=self.env, timeout=45)
        try:
            value = json.loads(result.stdout) if result.stdout.strip() else None
        except json.JSONDecodeError:
            value = None
        safe_value = dict(value) if isinstance(value, dict) else value
        if isinstance(safe_value, dict) and "token" in safe_value:
            safe_value["token"] = "[ISOLATED_TEST_TOKEN_REDACTED]"
        entry = {
            "id": len(self.commands) + 1, "argv": argv, "stdin": stdin,
            "returncode": result.returncode, "stdout": safe_value,
            "stderr": result.stderr, "elapsed_ms": round((time.monotonic() - start) * 1000, 2),
        }
        self.commands.append(entry)
        self.flush()
        if denied:
            return entry
        if result.returncode != 0:
            raise RuntimeError(f"CLI failed: {args}: {result.stderr}")
        if value is None:
            raise AssertionError(f"No JSON output: {args}: {result.stdout}")
        return value

    def check(self, name, condition, detail=None):
        self.assertions.append({"name": name, "status": "passed" if condition else "failed",
                                "detail": detail, "command_id": len(self.commands)})
        self.flush()

    def gap(self, name, reason):
        self.assertions.append({"name": name, "status": "not_exercised", "detail": reason})

    def flush(self):
        (self.output / "commands.jsonl").write_text(
            "".join(json.dumps(x, ensure_ascii=False) + "\n" for x in self.commands))
        (self.output / "assertions.json").write_text(json.dumps({
            "classification": "synthetic deterministic governance controls; no model extraction",
            "binary": str(self.binary), "environment": {k: self.env[k] for k in (
                "KP_MODEL_POLICY", "KP_OFFLINE", "KP_NO_AUTO_DREAM", "KP_NO_AUTO_DAEMON",
                "KP_DISABLE_MODEL_DOWNLOAD")},
            "checks": self.assertions,
            "counts": {status: sum(x["status"] == status for x in self.assertions)
                       for status in ("passed", "failed", "not_exercised")},
        }, ensure_ascii=False, indent=2) + "\n")

    def library(self, name):
        lib = self.work / name
        self.run(lib, "init", "--owner", "owner", "--name", f"CONTROL ONLY: {name}",
                 "--community", "--no-embedding")
        self.run(lib, "member", "add", "reader", "--role", "member")
        return lib

    def ingest(self, lib, *lines, actor="owner", denied=False):
        return self.run(lib, "ingest", "-", actor=actor, denied=denied,
                        stdin="".join(json.dumps(x) + "\n" for x in lines))

    @staticmethod
    def found(result, needle):
        return any(needle in hit["passage"] for hit in result["results"])

    def remember(self, lib, subject, value, *, predicate="kp.core.decision", kind="decision",
                 audience="owner,reader", valid_from=0, actor="owner"):
        return self.run(lib, "remember", "--subject", subject, "--predicate", predicate,
                        "--value", json.dumps(value), "--kind", kind,
                        "--audience", audience, "--valid-from", str(valid_from), actor=actor)["facts"][0]

    def lifecycle(self):
        lib = self.library("source_lifecycle")
        at = int(time.time() * 1000)
        roster = {"container_id": "control.room", "label": "SYNTHETIC CONTROL ROOM",
                  "members": ["owner", "reader"], "all_library_members": False, "synced_at": at}
        original = {"source": {"kind": "control", "uri": "control://source/1",
                                "external_id": "source-1", "container_id": "control.room"},
                    "speaker": "owner", "audience": "container_members", "occurred_at": at,
                    "content": "CONTROLORIGINAL814: synthetic source, version one.", "op": "upsert",
                    "model_policy": {"mode": "raw_only"}, "thread_key": "control-thread"}
        first = self.ingest(lib, roster, original)
        self.check("raw_only_source_is_indexed_without_extraction_job",
                   first["records_upserted"] == 1 and first["jobs_enqueued"] == 0, first)
        hit = self.run(lib, "search", "CONTROLORIGINAL814", actor="reader")
        self.check("source_roster_member_can_retrieve_original", self.found(hit, "CONTROLORIGINAL814"))
        duplicate = self.ingest(lib, original)
        self.check("repeated_source_is_idempotent", duplicate["records_duplicate"] == 1, duplicate)
        changed = {**original, "content": "CONTROLILLICIT814: changed without supersede"}
        denied = self.ingest(lib, changed, denied=True)
        self.check("changed_bytes_strict_upsert_is_rejected", denied["returncode"] != 0, denied["stderr"])
        withdrawal = {**original, "content": "", "op": "withdraw", "occurred_at": at + 1}
        denied = self.ingest(lib, withdrawal, actor="reader", denied=True)
        self.check("member_cannot_withdraw_another_ingesters_source", "permission denied" in denied["stderr"], denied["stderr"])
        self.check("denied_withdrawal_preserves_original", self.found(
            self.run(lib, "search", "CONTROLORIGINAL814"), "CONTROLORIGINAL814"))
        replacement = {**original, "source": {**original["source"], "external_id": "source-2"},
                       "content": "CONTROLREPLACEMENT814: synthetic source, version two.",
                       "op": "supersede", "previous_external_id": "source-1", "occurred_at": at + 2}
        report = self.ingest(lib, replacement)
        self.check("explicit_source_supersession_is_applied", report["records_superseded"] == 1, report)
        self.check("superseded_source_is_not_in_current_retrieval", not self.found(
            self.run(lib, "search", "CONTROLORIGINAL814"), "CONTROLORIGINAL814"))
        self.check("replacement_source_is_retrievable", self.found(
            self.run(lib, "search", "CONTROLREPLACEMENT814", actor="reader"), "CONTROLREPLACEMENT814"))
        narrowed = {**roster, "members": ["owner"], "synced_at": at + 3}
        denied = self.ingest(lib, narrowed, actor="reader", denied=True)
        self.check("member_cannot_rewrite_source_roster", "permission denied" in denied["stderr"])
        self.ingest(lib, narrowed)
        self.check("roster_departure_immediately_hides_existing_source", not self.found(
            self.run(lib, "search", "CONTROLREPLACEMENT814", actor="reader"), "CONTROLREPLACEMENT814"))
        stale = self.ingest(lib, roster, denied=True)
        self.check("stale_roster_cannot_restore_revoked_access", stale["returncode"] != 0, stale["stderr"])
        source_withdraw = {**replacement, "op": "withdraw", "content": "", "occurred_at": at + 4}
        source_withdraw.pop("previous_external_id")
        report = self.ingest(lib, source_withdraw)
        self.check("owner_withdraws_replacement", report["records_withdrawn"] == 1)
        self.check("withdrawn_source_is_absent", not self.found(
            self.run(lib, "search", "CONTROLREPLACEMENT814"), "CONTROLREPLACEMENT814"))
        dream = self.run(lib, "dream")
        self.check("raw_only_replay_fabricates_no_semantic_facts", dream["facts_distilled"] == 0, dream)

    def history(self):
        lib = self.library("valid_time")
        old = self.remember(lib, "CONTROLHISTORY814", "rainy", predicate="kp.core.weather",
                            kind="fact_slot", valid_from=10)
        new = self.remember(lib, "CONTROLHISTORY814", "sunny", predicate="kp.core.weather",
                            kind="fact_slot", valid_from=20)
        earlier = self.run(lib, "search", "CONTROLHISTORY814", "--at", "15")
        later = self.run(lib, "search", "CONTROLHISTORY814", "--at", "25")
        self.check("valid_time_search_returns_old_slot_only",
                   self.found(earlier, "rainy") and not self.found(earlier, "sunny"))
        self.check("valid_time_search_returns_new_slot_only",
                   self.found(later, "sunny") and not self.found(later, "rainy"))
        proof = self.run(lib, "why", new, "--at", "25")
        self.check("current_fact_has_strong_reference_provenance",
                   proof["fact_id"] == new and "content_sha256" in proof["reference"], proof)
        denied = self.run(lib, "forget", new, actor="reader", denied=True)
        self.check("member_cannot_forget_another_authors_fact", denied["returncode"] != 0,
                   {"returncode": denied["returncode"], "response": denied["stdout"], "stderr": denied["stderr"]})
        self.run(lib, "forget", new)
        self.check("withdrawn_fact_does_not_reappear_via_historical_time", not self.found(
            self.run(lib, "search", "CONTROLHISTORY814", "--at", "25"), "sunny"))
        self.gap("transaction_time_and_historical_why", "Only valid-time search was exercised; --at does not promise arbitrary transaction-time reconstruction or revoked-access replay.")

    @staticmethod
    def edit_entry(path, fact_id, replacement):
        before = path.read_text()
        lines, armed, found = [], False, False
        for line in before.splitlines():
            if line.strip() == f"<!-- kp-entry: {fact_id} -->":
                armed = True
            elif armed and line.startswith("- **[kp-ref:"):
                prefix, sep, _ = line.partition("`) ")
                if not sep:
                    raise AssertionError("Unexpected exported entry structure")
                line = prefix + sep + replacement
                armed, found = False, True
            lines.append(line)
        if not found:
            raise AssertionError(f"Entry not found: {fact_id}")
        after = "\n".join(lines) + "\n"
        path.write_text(after)
        return "".join(difflib.unified_diff(before.splitlines(True), after.splitlines(True),
                                            fromfile="before/index.md", tofile="after/index.md"))

    def writeback(self):
        lib = self.library("markdown_writeback")
        shared = self.remember(lib, "CONTROLWRITEBACK814", {"summary": "CONTROLWRITEBACK814 version one"})
        self.remember(lib, "CONTROLPRIVATE814", {"summary": "CONTROLPRIVATE814 owner-only"}, audience="owner")
        reader_dir = self.work / "reader_projection"
        self.run(lib, "export", reader_dir, actor="reader")
        self.check("export_obeys_viewer_acl", "CONTROLPRIVATE814" not in (reader_dir / "index.md").read_text())
        owner_dir = self.work / "owner_projection"
        self.run(lib, "export", owner_dir)
        diff = self.edit_entry(owner_dir / "index.md", shared, "CONTROLWRITEBACK814 version two")
        (self.output / "writeback-preview.diff").write_text(diff)
        result = self.run(lib, "apply", owner_dir)
        self.check("reviewed_markdown_edit_updates_one_owned_fact",
                   len(result["updated_facts"]) == 1 and not result["baseline_conflict"], result)
        new_id = result["updated_facts"][0]["new_fact_id"]
        repeat = self.run(lib, "apply", owner_dir)
        self.check("duplicate_apply_is_noop", repeat["unchanged"] and not repeat["updated_facts"], repeat)
        self.check("writeback_result_is_searchable", self.found(
            self.run(lib, "search", "CONTROLWRITEBACK814"), "version two"))
        denied = self.run(lib, "apply", owner_dir, actor="reader", denied=True)
        self.check("projection_cannot_be_applied_as_a_different_viewer", denied["returncode"] != 0, denied["stderr"])
        stale_dir = self.work / "stale_projection"
        self.run(lib, "export", stale_dir)
        self.remember(lib, "CONTROLCONCURRENT814", {"summary": "Concurrent explicit control input"})
        diff = self.edit_entry(stale_dir / "index.md", new_id, "CONTROLWRITEBACK814 stale edit version three")
        (self.output / "stale-writeback-preview.diff").write_text(diff)
        stale = self.run(lib, "apply", stale_dir)
        self.check("stale_baseline_queues_review_without_direct_update",
                   stale["baseline_conflict"] and len(stale["enqueued_proposals"]) == 1
                   and not stale["updated_facts"], stale)
        self.check("stale_edit_does_not_silently_change_active_fact", not self.found(
            self.run(lib, "search", "CONTROLWRITEBACK814"), "stale edit version three"))
        review_id = stale["enqueued_proposals"][0]
        denied = self.run(lib, "review", "resolve", review_id, "--choice", "confirm", actor="reader", denied=True)
        self.check("another_actor_cannot_confirm_owners_writeback_review", denied["returncode"] != 0)
        approved = self.run(lib, "review", "resolve", review_id, "--choice", "confirm")
        self.check("own_review_confirmation_applies_staged_edit", approved["status"] == "confirmed", approved)
        self.check("confirmed_edit_is_now_searchable", self.found(
            self.run(lib, "search", "CONTROLWRITEBACK814"), "stale edit version three"))
        denied = self.run(lib, "review", "resolve", review_id, "--choice", "confirm", denied=True)
        self.check("resolved_review_cannot_be_confirmed_twice", denied["returncode"] != 0)
        tampered_dir = self.work / "tampered_projection"
        exported = self.run(lib, "export", tampered_dir)
        index = tampered_dir / "index.md"
        before = index.read_text()
        after = before.replace(f"library_id: {exported['library_id']}",
                               "library_id: 00000000-0000-4000-8000-000000000000", 1)
        index.write_text(after)
        (self.output / "tampered-projection-preview.diff").write_text("".join(
            difflib.unified_diff(before.splitlines(True), after.splitlines(True),
                                 fromfile="original/index.md", tofile="tampered/index.md")))
        denied = self.run(lib, "apply", tampered_dir, denied=True)
        self.check("tampered_projection_library_identity_is_rejected",
                   denied["returncode"] != 0 and "library_id" in denied["stderr"], denied["stderr"])
        self.check("tampered_projection_leaves_current_fact_unchanged", self.found(
            self.run(lib, "search", "CONTROLWRITEBACK814"), "stale edit version three"))
        self.run(lib, "log", "--limit", "30")

    def guards(self):
        lib = self.library("typed_guards")
        source = self.work / "controlled-policy.md"
        source.write_text("# CONTROLGUARD814\n\nconstraint: Synthetic control policy [forbid: `npm install`] [allow: `pnpm install`]\n")
        result = self.run(lib, "add", source, "--extract", "--readable-by", "owner,reader")
        guard_id = result[0]["extracted"]["facts"][0]

        def pretool(command):
            return self.run(lib, "hook", "pre-tool-use", "--client", "codex",
                            stdin=json.dumps({"tool_name": "exec_command", "tool_input": {"cmd": command}}))

        def asks(response):
            return response.get("hookSpecificOutput", {}).get("permissionDecision") == "deny"

        self.check("typed_command_guard_denies_forbidden_codex_command", asks(pretool("npm install control-package")))
        self.check("explicit_allowed_command_is_not_blocked", pretool("pnpm install control-package") == {})
        denied = self.run(lib, "allow", guard_id, actor="reader", denied=True)
        self.check("ordinary_member_cannot_grant_guard_bypass", "permission denied" in denied["stderr"])
        grant = self.run(lib, "allow", guard_id)
        self.check("guard_approval_has_bounded_single_use_lease",
                   grant["single_use"] and grant["expires_at"] - grant["created_at"] == 600000, grant)
        self.check("first_guarded_action_consumes_approval", pretool("npm install control-package") == {})
        self.check("second_guarded_action_requires_new_approval", asks(pretool("npm install control-package")))
        self.run(lib, "forget", guard_id)
        self.check("withdrawn_guard_stops_affecting_commands", pretool("npm install control-package") == {})
        # This explicit owner declaration predates the grant. No historical source is fabricated.
        guard_id = self.remember(lib, "CONTROLLEASE814", {
            "summary": "CONTROL explicit owner declaration for lease expiry",
            "forbidden_commands": ["npm install"], "allowed_commands": ["pnpm install"],
            "required_flags": [], "hard_guard": True,
        }, kind="constraint", predicate="kp.core.constraint", valid_from=0)
        # Public --at models an already-expired grant without a sleep or database edit.
        expired_at = int(time.time() * 1000) - 700000
        self.run(lib, "allow", guard_id, "--at", str(expired_at))
        self.check("expired_guard_lease_does_not_authorize_action", asks(pretool("npm install control-package")))
        self.run(lib, "forget", guard_id)
        prose = self.remember(lib, "CONTROLPROSE814", {"summary": "Never run npm install in this control"}, kind="constraint", predicate="kp.core.constraint")
        self.check("prose_without_typed_guard_is_not_scraped_at_runtime", pretool("npm install control-package") == {})
        self.run(lib, "forget", prose)
        self.gap("external_untrusted_guard_authority", "No external source was made authoritative in these controls. External-claim elevation needs its separate trust-boundary scenario; a manual owner fact cannot prove it.")

    def skills(self):
        lib = self.library("manual_episode_state_machine")

        def record(session):
            return self.run(lib, "episode", "record", "--kind", "human-correction",
                            "--before", "CONTROL: synthetic unbounded retry example",
                            "--after", "No, use a bounded retry and verify the result. CONTROL synthetic correction.",
                            "--step-ref", "controlled-step-1", "--session", session)

        self.check("explicit_manual_episode_is_recorded", record("controlled-session-1")["episodes"] == 1)
        first = self.run(lib, "dream")
        self.check("one_control_episode_does_not_promote_skill", first["bullets_created"] == 1 and not first["promoted"], first)
        self.check("same_manual_episode_is_deduplicated", record("controlled-session-1")["episodes"] == 0)
        self.check("repeated_dream_has_no_unprocessed_episode", self.run(lib, "dream")["processed"] == 0)
        record("controlled-session-2")
        second = self.run(lib, "dream")
        self.check("two_explicit_control_sessions_promote_skill", len(second["promoted"]) == 1, second)
        skills = self.run(lib, "skill", "list")
        slug = skills[0]["name"]
        self.check("promoted_skill_has_evidence_references", bool(skills[0]["references"]), skills[0])
        denied = self.run(lib, "skill", "approve", slug, actor="reader", denied=True)
        self.check("skill_execution_approval_requires_owner", "permission denied" in denied["stderr"])
        self.run(lib, "skill", "approve", slug)
        first_hurt = self.run(lib, "feedback", slug, "--outcome", "hurt", "--task", "controlled-feedback-1")
        self.check("first_hurt_after_two_helped_is_retained_not_rollback", first_hurt["counts"]["hurt"] == 1 and not first_hurt["rolled_back"], first_hurt)
        repeated = self.run(lib, "feedback", slug, "--outcome", "hurt", "--task", "controlled-feedback-1")
        self.check("feedback_is_deduplicated_per_task", repeated["recorded"] is False, repeated)
        second_hurt = self.run(lib, "feedback", slug, "--outcome", "hurt", "--task", "controlled-feedback-2")
        self.check("second_independent_hurt_rolls_back_skill", second_hurt["rolled_back"], second_hurt)
        self.check("rolled_back_skill_is_no_longer_active", self.run(lib, "skill", "list") == [])
        self.gap("autonomous_skill_learning", "Episodes above are explicitly authored synthetic controls. They prove state transitions only, not model extraction, real successes, or autonomous improvement.")

    def all(self):
        for case in (self.lifecycle, self.history, self.writeback, self.guards, self.skills):
            try:
                case()
            except Exception as exc:
                self.assertions.append({"name": f"{case.__name__}_execution", "status": "failed",
                                        "detail": str(exc), "traceback": traceback.format_exc(),
                                        "command_id": len(self.commands)})
                self.flush()
        self.gap("ambiguous_governed_scope_confirmation", "No fabricated model scope response was injected. Real extraction must produce an actual review candidate; writeback review is tested separately.")
        self.gap("community_publication_and_imported_skill_quarantine", "This runner does not start network services or publish source material. Separate local-community integration evidence is required.")
        self.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kp", type=Path, default=Path(__file__).resolve().parents[2] / "target/release/kp")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary, output = args.kp.resolve(), args.output.resolve()
    if not binary.is_file():
        parser.error(f"Build the current CLI first: missing {binary}")
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="kipo-governance-") as work:
        controls = Controls(binary, output, Path(work))
        controls.all()
    failed = [x for x in controls.assertions if x["status"] == "failed"]
    print(json.dumps({"output": str(output), "passed": sum(x["status"] == "passed" for x in controls.assertions),
                      "failed": len(failed), "not_exercised": sum(x["status"] == "not_exercised" for x in controls.assertions)}, indent=2))
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
