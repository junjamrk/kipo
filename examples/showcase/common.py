"""Public-CLI recording utilities. No direct database writes or synthetic model output."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def write_json(path: Path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def digest(path: Path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Recorder:
    def __init__(self, binary: Path, output: Path, policy="raw_only"):
        self.binary = binary.resolve()
        self.output = output.resolve()
        self.output.mkdir(parents=True, exist_ok=False)
        self.library = self.output / "library"
        self.env = dict(os.environ, KP_MODEL_POLICY=policy,
                        KP_NO_AUTO_DREAM="1", KP_DISABLE_MODEL_DOWNLOAD="1")
        if policy == "raw_only":
            self.env["KP_OFFLINE"] = "1"
        else:
            self.env.pop("KP_OFFLINE", None)
        self.records = []
        self.checks = []
        self.sequence = 0

    def call(self, *args, stdin=None, required=True, timeout=240):
        self.sequence += 1
        command = [str(self.binary), "--path", str(self.library), "--json", *map(str, args)]
        started = time.perf_counter()
        try:
            proc = subprocess.run(command, input=stdin, text=True, capture_output=True,
                                  env=self.env, timeout=timeout)
            rc, stdout, stderr = proc.returncode, proc.stdout, proc.stderr
        except subprocess.TimeoutExpired as error:
            rc = 124
            stdout = error.stdout or b""
            stderr = error.stderr or b""
            stdout = stdout.decode(errors="replace") if isinstance(stdout, bytes) else stdout
            stderr = stderr.decode(errors="replace") if isinstance(stderr, bytes) else stderr
            stderr += "\nShowcase runner timeout; not a successful operation."
        elapsed = (time.perf_counter() - started) * 1000
        try:
            parsed = json.loads(stdout) if stdout.strip() else None
        except json.JSONDecodeError:
            parsed = None
        record = dict(sequence=self.sequence, argv=command, returncode=rc,
                      elapsed_ms=elapsed, stdout=stdout, stderr=stderr, parsed=parsed)
        if stdin is not None:
            record["stdin"] = stdin
        write_json(self.output / "commands" / f"{self.sequence:04d}.json", record)
        self.records.append(record)
        if required and rc != 0:
            raise RuntimeError(f"Command {self.sequence} failed ({rc}); inspect commands/{self.sequence:04d}.json")
        return record

    def check(self, name, passed, **evidence):
        self.checks.append(dict(name=name, passed=bool(passed), **evidence))

    def result(self, **fields):
        result = dict(**fields, binary=str(self.binary), binary_sha256=digest(self.binary),
                      checks=self.checks, command_count=len(self.records),
                      command_failures=sum(x["returncode"] != 0 for x in self.records))
        write_json(self.output / "result.json", result)
        return result


def source_ids(hit):
    """Collect only explicit provenance coordinates, never infer relevance from text."""
    found = set()
    def visit(value):
        if isinstance(value, dict):
            for key, item in value.items():
                if key == "source_id" and isinstance(item, str):
                    found.add(item)
                elif key == "source_ids" and isinstance(item, list):
                    found.update(x for x in item if isinstance(x, str))
                else:
                    visit(item)
        elif isinstance(value, list):
            for item in value:
                visit(item)
    visit(hit)
    return found
