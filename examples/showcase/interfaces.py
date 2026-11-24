#!/usr/bin/env python3
"""Run isolated MCP, loopback HTTP and community exchange control experiments.

These fixtures are invented protocol probes, separate from the real community case.
Only public kp commands and HTTP/MCP interfaces are used. No database writes,
external server access, model calls or edits to installed agent configuration.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import select
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request

from common import Recorder, write_json


def wire_text(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True)


class StdioMcp:
    def __init__(self, recorder, library, actor):
        self.recorder = recorder
        self.seq = 0
        self.log = []
        self.stderr = (recorder.output / "mcp-stderr.log").open("w")
        self.argv = [str(recorder.binary), "--path", str(library), "--as", actor, "mcp"]
        self.process = subprocess.Popen(self.argv, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.stderr,
                                        text=True, env=recorder.env)

    def request(self, method, params=None, notification=False):
        self.seq += 1
        request = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if not notification:
            request["id"] = self.seq
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
        response = None
        if not notification:
            ready, _, _ = select.select([self.process.stdout], [], [], 15)
            if not ready:
                raise TimeoutError(f"MCP response timed out: {method}")
            line = self.process.stdout.readline()
            if not line:
                raise RuntimeError(f"MCP process closed stdout: {method}")
            response = json.loads(line)
            if response.get("id") != self.seq:
                raise RuntimeError("MCP response id mismatch")
        self.log.append({"request": request, "response": response})
        write_json(self.recorder.output / "mcp-transcript.json", self.log)
        return response

    def tool(self, name, arguments):
        return self.request("tools/call", {"name": name, "arguments": arguments})["result"]

    def close(self):
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        self.stderr.close()
        if self.process.stdout:
            self.process.stdout.close()
        write_json(self.recorder.output / "mcp-process.json",
                   {"argv": self.argv, "returncode": self.process.returncode,
                    "stopped": self.process.poll() is not None})


class LoopbackServer:
    def __init__(self, recorder, library, name):
        self.recorder = recorder
        self.name = name
        self.log_path = recorder.output / f"{name}-server.log"
        self.log = self.log_path.open("w")
        self.argv = [str(recorder.binary), "--path", str(library), "serve", "--community",
                     "--listen", "127.0.0.1:0"]
        self.process = subprocess.Popen(self.argv, stdout=self.log, stderr=self.log,
                                        env=recorder.env)
        self.url = None

    def ready(self):
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            match = re.search(r"kp listening on (127\.0\.0\.1:\d+)",
                              self.log_path.read_text())
            if match:
                self.url = "http://" + match.group(1)
                return self.url
            if self.process.poll() is not None:
                raise RuntimeError(f"{self.name} server exited; see {self.log_path.name}")
            time.sleep(.05)
        raise TimeoutError(f"{self.name} server failed to announce its loopback address")

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        self.log.close()
        write_json(self.recorder.output / f"{self.name}-process.json",
                   {"argv": self.argv, "url": self.url,
                    "returncode": self.process.returncode,
                    "stopped": self.process.poll() is not None})


class HttpProbe:
    def __init__(self, recorder):
        self.recorder = recorder
        self.log = []
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def call(self, url, path, token=None, body=None, origin=None):
        parsed_url = urllib.parse.urlsplit(url)
        if parsed_url.scheme != "http" or parsed_url.hostname != "127.0.0.1":
            raise ValueError("This runner permits only explicit IPv4 loopback HTTP")
        headers = {"Content-Type": "application/json"}
        if token is not None:
            headers["Authorization"] = "Bearer " + token
        if origin:
            headers["Origin"] = origin
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(url + path, data=data, headers=headers)
        try:
            with self.opener.open(request, timeout=10) as response:
                status, raw = response.status, response.read().decode()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read().decode()
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            value = raw
        record = {"url": url + path, "method": request.get_method(),
                  "has_bearer_token": token is not None, "origin": origin,
                  "request": body, "status": status, "response": value}
        self.log.append(record)
        write_json(self.recorder.output / "http-transcript.json", self.log)
        return status, value


def run(binary, output):
    r = Recorder(binary, output)
    servers = []
    mcp = None
    error = None
    remote = r.library
    origin_library = r.output / "origin-library"
    recipient = r.output / "recipient-library"
    private_marker = "interface_private_6407"
    shared_marker = "interface_shared_6407"
    exchange_marker = "interface_exchange_6407"
    http = HttpProbe(r)

    def call(library, *args):
        r.library = library
        return r.call(*args, timeout=30)["parsed"]

    def remember(library, subject, value, audience=None):
        args = ["remember", "--subject", subject, "--predicate", "kp.core.preference",
                "--value", value]
        if audience:
            args += ["--audience", audience]
        return call(library, *args)["facts"][0]

    try:
        call(remote, "init", "--community", "--owner", "owner", "--no-embedding")
        reader_token = call(remote, "member", "add", "reader")["token"]
        publisher_token = call(remote, "member", "add", "publisher")["token"]
        private_id = remember(remote, "private interface fixture", private_marker)
        shared_id = remember(remote, "shared interface fixture", shared_marker, "*")

        mcp = StdioMcp(r, remote, "reader")
        initialized = mcp.request("initialize", {"protocolVersion": "2025-03-26",
                                                 "capabilities": {},
                                                 "clientInfo": {"name": "kipo-showcase", "version": "1"}})
        r.check("mcp_initialize", initialized.get("result", {}).get("serverInfo", {}).get("name") == "kp")
        mcp.request("notifications/initialized", notification=True)
        names = [tool["name"] for tool in mcp.request("tools/list")["result"]["tools"]]
        r.check("mcp_three_tool_catalog", names == ["kp_search", "kp_note", "kp_why"], names=names)
        note = mcp.tool("kp_note", {"text": "Protocol fixture: interface_note_6407 is an explicit local note."})
        note_id = note.get("structuredContent", {}).get("fact_id")
        r.check("mcp_explicit_note_saved", note.get("isError") is False and bool(note_id))
        found = mcp.tool("kp_search", {"query": "interface_note_6407", "limit": 100})
        r.check("mcp_note_retrievable", found.get("isError") is False and
                "interface_note_6407" in wire_text(found))
        why = mcp.tool("kp_why", {"fact_id": note_id})
        r.check("mcp_note_provenance", why.get("structuredContent", {}).get("proof", {}).get("fact_id") == note_id)
        found_shared = mcp.tool("kp_search", {"query": shared_marker, "limit": 100})
        found_private = mcp.tool("kp_search", {"query": private_marker, "limit": 100})
        r.check("mcp_viewer_acl", shared_marker in wire_text(found_shared) and
                private_marker not in wire_text(found_private))
        hidden = mcp.tool("kp_why", {"fact_id": private_id})
        unknown = mcp.tool("kp_why", {"fact_id": "fact-nonexistent-protocol-fixture"})
        r.check("mcp_hidden_unknown_same_empty_proof", hidden == unknown and
                hidden.get("structuredContent", {}).get("proof") is None)
        admin = mcp.tool("kp_forget", {"fact_id": shared_id})
        r.check("mcp_admin_tool_unavailable", admin.get("isError") is True)
        mcp.close()
        r.check("mcp_process_clean_exit", mcp.process.returncode == 0)
        mcp = None

        community = LoopbackServer(r, remote, "community")
        servers.append(community)
        url = community.ready()
        r.check("http_requires_bearer", http.call(url, "/v1/status")[0] == 401)
        r.check("http_invalid_bearer_denied", http.call(url, "/v1/status", "invalid-fixture-token")[0] == 401)
        status, value = http.call(url, "/v1/status", reader_token)
        r.check("http_authenticated_status", status == 200 and value.get("actor") == "reader")
        status, _ = http.call(url, "/v1/status", reader_token, origin="https://unrelated.invalid")
        r.check("http_cross_origin_denied", status == 403)
        _, visible = http.call(url, "/v1/search?q=" + shared_marker, reader_token)
        _, invisible = http.call(url, "/v1/search?q=" + private_marker, reader_token)
        r.check("http_search_acl", shared_marker in wire_text(visible) and private_marker not in wire_text(invisible))
        hidden = http.call(url, "/v1/why/" + private_id, reader_token)
        unknown = http.call(url, "/v1/why/fact-nonexistent-protocol-fixture", reader_token)
        r.check("http_hidden_unknown_same_response", hidden == unknown and hidden[0] == 404)
        status, _ = http.call(url, "/v1/withdraw", reader_token, {"fact_id": shared_id})
        r.check("http_member_cannot_withdraw_another_author", status == 403)
        status, catalog = http.call(url, "/mcp", reader_token,
                                    {"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
        http_names = [x["name"] for x in catalog.get("result", {}).get("tools", [])]
        r.check("http_mcp_distinct_community_catalog", status == 200 and
                set(http_names) == {"kp_search", "kp_publish", "kp_withdraw", "kp_why", "kp_synthesize"}, names=http_names)
        status, searched = http.call(url, "/mcp", reader_token,
                                     {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                      "params": {"name": "kp_search", "arguments": {"q": shared_marker}}})
        r.check("http_mcp_search", status == 200 and shared_marker in wire_text(searched))
        status, card = http.call(url, "/.well-known/agent.json")
        r.check("a2a_public_discovery", status == 200 and card.get("authentication", {}).get("schemes") == ["Bearer"])
        status, a2a = http.call(url, "/a2a", reader_token, {"task": "search", "input": {"q": shared_marker}})
        r.check("a2a_authenticated_search", status == 200 and a2a.get("status") == "completed" and shared_marker in wire_text(a2a))

        call(origin_library, "init", "--community", "--owner", "publisher", "--no-embedding")
        origin = LoopbackServer(r, origin_library, "origin")
        servers.append(origin)
        origin.ready()
        fact_id = remember(origin_library, "exchange protocol fixture", exchange_marker, "*")
        published = call(origin_library, "publish", fact_id, "--to", url, "--token", publisher_token)
        r.check("publish_to_loopback_community", published.get("queued") is False)
        _, published_search = http.call(url, "/v1/search?q=" + exchange_marker, reader_token)
        r.check("published_fact_visible_to_remote_reader", exchange_marker in wire_text(published_search))
        call(recipient, "init", "--owner", "reader", "--no-embedding")
        synced = call(recipient, "sync", "--from", url, "--token", reader_token)
        r.check("sync_into_independent_library", synced.get("mirrored", 0) > 0)
        mirrored = call(recipient, "search", exchange_marker)
        r.check("synced_fact_retrievable", exchange_marker in wire_text(mirrored))
        call(origin_library, "forget", fact_id)
        flushed = call(origin_library, "sync", "--from", url, "--token", publisher_token)
        r.check("withdrawal_outbox_delivered", flushed.get("delivered", 0) > 0 and flushed.get("pending") == 0)
        call(recipient, "sync", "--from", url, "--token", reader_token)
        after = call(recipient, "search", exchange_marker)
        _, remote_after = http.call(url, "/v1/search?q=" + exchange_marker, reader_token)
        r.check("withdrawal_removed_remote_and_synced_visibility", exchange_marker not in wire_text(after) and
                exchange_marker not in wire_text(remote_after))
    except Exception as caught:
        error = f"{type(caught).__name__}: {caught}"
        r.check("runner_completed", False, error=error)
    finally:
        if mcp is not None:
            mcp.close()
        for server in reversed(servers):
            server.close()
        r.check("all_child_servers_stopped", all(server.process.poll() is not None for server in servers),
                server_count=len(servers))
    passed = error is None and all(check["passed"] for check in r.checks)
    result = r.result(suite="interfaces", passed=passed, error=error,
                      fixture_type="synthetic protocol control experiment; not the real Next.js case",
                      scope="public CLI, stdio MCP, loopback HTTP MCP/REST/A2A, cross-library publish/sync/withdraw",
                      model_calls=0, http_request_count=len(http.log),
                      limitations=["No live external community or installed agent configuration was accessed.",
                                   "Base-fact exchange does not demonstrate imported skill quarantine or approval.",
                                   "No claim about TLS, internet deployment, throughput or multi-host networks."])
    print(json.dumps({"passed": passed, "checks": len(r.checks), "result": str(r.output / "result.json"),
                      "error": error}, indent=2))
    return 0 if result["passed"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kp", type=Path, default=Path(__file__).resolve().parents[2] / "target/release/kp")
    parser.add_argument("--output", type=Path, required=True, help="A new output directory; existing runs are preserved")
    args = parser.parse_args()
    return run(args.kp, args.output)


if __name__ == "__main__":
    raise SystemExit(main())
