#!/usr/bin/env python3
"""Fake OpenCode 2.0.8 server for opencode-cosmic UI flow tests.

Python 3 stdlib only. Response shapes, status codes, error bodies, event
sequences and SSE framing are copied from the real captures in
``tests/fixtures/v2-2.0.8`` (see its README.md). ``tests/fake_v2/selftest.py``
checks this server against those fixtures.

Scenarios are picked by a marker in the prompt text, like the real harness's
mock provider: ``[[scenario:text|reasoning|tools|permission|child-permission|
subagent|form|error|retry|slow|long]]`` (``subagent-permission`` is an alias of
``child-permission``). Without a marker the reply is a short text.

``--background-jobs`` seeds running background work for the sidebar's
Background section: a running child session of the main session
(``SES_BG_CHILD``), a shell it started in the workspace, and a shell in the
other directory started by a root session without a tab (``SES_BG_OWNER``).

Test-only endpoints (no auth, never logged as API traffic):
- ``POST /__control`` with ``{"action": ...}``; see ``Server.control``.
- ``GET /__control`` returns a state summary (seed IDs, SSE clients, pending
  permissions/forms, running sessions).
- ``GET /__log`` returns the request log as JSON lines.

The request log (``--log-file``) holds one JSON object per line:
``{"seq", "t", "kind": "http"|"sse.open"|"sse.close"|"event"|"control"|"restart", ...}``.
HTTP records carry ``method, path, route, params, query, status, auth, body``
where ``body`` is a summary: attachment bytes are never logged.
"""

import argparse
import base64
import binascii
import hashlib
import json
import os
import queue
import random
import re
import secrets
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

VERSION = "2.0.8"
USERNAME = "opencode"
MAX_ATTACHMENT_BYTES = 20 * 1024 * 1024
ID_CHARS = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
MASK48 = (1 << 48) - 1

# Fixed seed IDs so flow scripts can write state.json before the server starts.
SES_MAIN = "ses_f90000000001ffeIntegration"
SES_OTHER = "ses_f90000000002ffeSecondSessn"
SES_CHILD = "ses_f90000000003ffeChildOfMain"
SES_BG_CHILD = "ses_f90000000004ffeBgChildTask"
SES_BG_OWNER = "ses_f90000000005ffeBgShellOwnr"
SH_BG_DEV = "sh_f90000000001ffeBgDevServer"
SH_BG_TEST = "sh_f90000000002ffeBgCargoTest"
BOOT_PERMISSION = "per_000000000001BootPermissn1"
BOOT_FORM = "frm_000000000001BootFormHarns1"
GLOBAL = "global"

# Envelope rules observed in the 2.0.8 captures.
NO_LOCATION = {
    "server.connected",
    "session.execution.started",
    "session.execution.succeeded",
    "session.execution.failed",
    "session.execution.interrupted",
    "session.usage.updated",
    "session.inbox.cancelled",
    "session.inbox.delivery.changed",
}
NOT_DURABLE = {
    "server.connected",
    "session.text.delta",
    "session.reasoning.delta",
    "session.usage.updated",
    "session.tool.progress",
    "permission.asked",
    "permission.replied",
    "shell.created",
    "shell.exited",
    "form.created",
    "form.cancelled",
    "form.replied",
    "model.updated",
    "provider.updated",
    "session.deleted",
}

MODEL_CATALOG = [
    {
        "id": "mock-model",
        "modelID": "mock-model",
        "providerID": "mock",
        "name": "Mock Model",
        "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"apiKey": "dummy-not-a-secret", "baseURL": "http://127.0.0.1:9/v1"},
        "capabilities": {"tools": True, "input": ["text", "image"], "output": ["text"]},
        "variants": [
            {"id": "low", "settings": {"reasoningEffort": "low"}},
            {"id": "medium", "settings": {"reasoningEffort": "medium"}},
            {"id": "high", "settings": {"reasoningEffort": "high"}},
        ],
        "time": {"released": 0},
        "cost": [{"input": 1, "output": 2, "cache": {"read": 0, "write": 0}}],
        "status": "active",
        "enabled": True,
        "limit": {"context": 128000, "output": 4096},
    },
    {
        "id": "mock-model-alt",
        "modelID": "mock-model-alt",
        "providerID": "mock",
        "name": "Mock Model Alt",
        "package": "@opencode/ai/providers/openai-compatible",
        "settings": {"apiKey": "dummy-not-a-secret", "baseURL": "http://127.0.0.1:9/v1"},
        "capabilities": {"tools": True, "input": ["text"], "output": ["text"]},
        "variants": [
            {"id": "low", "body": {"reasoning_effort": "low"}},
            {"id": "high", "body": {"reasoning_effort": "high"}},
        ],
        "time": {"released": 0},
        "cost": [],
        "status": "active",
        "enabled": True,
        "limit": {"context": 32000, "output": 2048},
    },
]

HARNESS_FORM_FIELDS = [
    {"key": "name", "title": "Name", "required": True, "type": "string", "placeholder": "Ada"},
    {"key": "count", "title": "Count", "type": "integer", "minimum": 1, "maximum": 5, "default": 2},
    {"key": "ok", "title": "Proceed?", "type": "boolean", "default": False},
    {
        "key": "tags",
        "title": "Tags",
        "type": "multiselect",
        "options": [{"value": "a", "label": "Alpha"}, {"value": "b", "label": "Beta"}],
    },
    {"key": "docs", "type": "external", "url": "https://example.invalid/docs", "title": "Docs"},
]

README_TEXT = (
    "Read file README.md, lines 1-3\n1: # Harness workspace\n2: \n"
    "3: A tiny seeded project used by the opencode-cosmic v2 test harness."
)
SUBAGENT_BACKGROUND_TEXT = (
    "The subagent is working in the background (sessionID: {child}). You will be notified "
    "automatically when it finishes.\nDO NOT sleep, poll for progress, ask the subagent for "
    "status, or duplicate this subagent's work; avoid working with the same files or topics it "
    "is using.\nWork on non-overlapping tasks, or briefly tell the user what you launched and "
    "end your response."
)
PIXEL_PNG = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)

SCENARIO_RE = re.compile(r"\[\[scenario:([a-z-]+)\]\]")


def now_ms():
    return int(time.time() * 1000)


def compact(value):
    return json.dumps(value, separators=(",", ":"))


def b64url(value):
    return base64.urlsafe_b64encode(compact(value).encode()).rstrip(b"=").decode()


def unb64url(text):
    return json.loads(base64.urlsafe_b64decode(text + "=" * (-len(text) % 4)))


def project_id(directory):
    return hashlib.sha1(directory.encode()).hexdigest()


def cost_of(tokens_in, tokens_out):
    return (tokens_in * 1 + tokens_out * 2) / 1_000_000


def tokens(tokens_in=0, tokens_out=0):
    return {"input": tokens_in, "output": tokens_out, "reasoning": 0, "cache": {"read": 0, "write": 0}}


class Ids:
    """Server ID format: prefix + 12 hex (low 48 bits of ms<<12 + counter) + 14 base62."""

    def __init__(self):
        self.lock = threading.Lock()
        self.last = 0
        self.random = random.Random()

    def _tail(self):
        return "".join(self.random.choice(ID_CHARS) for _ in range(14))

    def _value(self, at=None):
        with self.lock:
            value = (at if at is not None else now_ms()) << 12
            if at is None:
                value = max(value, self.last + 1)
                self.last = value
            else:
                value += self.random.randrange(1, 4000)
            return value

    def ascending(self, prefix, at=None):
        return f"{prefix}_{self._value(at) & MASK48:012x}{self._tail()}"

    def descending(self, prefix, at=None):
        return f"{prefix}_{(~self._value(at)) & MASK48:012x}{self._tail()}"


class ApiError(Exception):
    def __init__(self, status, body):
        super().__init__(body.get("message") if isinstance(body, dict) else str(body))
        self.status = status
        self.body = body


def invalid_request(message, kind="Body", **extra):
    return ApiError(400, {"_tag": "InvalidRequestError", "message": message, "kind": kind, **extra})


def session_not_found(session_id):
    return ApiError(
        404,
        {"_tag": "SessionNotFoundError", "sessionID": session_id, "message": f"Session not found: {session_id}"},
    )


class Interrupted(Exception):
    pass


class Session:
    def __init__(self, session_id, directory, created, title=None, parent_id=None, agent=None):
        self.id = session_id
        self.parent_id = parent_id
        self.directory = directory
        self.project_id = project_id(directory)
        self.title = title
        self.model = None
        self.agent = agent
        self.cost = 0
        self.tokens = tokens()
        self.outcome = None
        self.created = created
        self.updated = created
        self.idle = None
        self.slug = random.choice(["gentle", "misty", "tidy", "kind", "eager", "quick"]) + "-" + random.choice(
            ["rocket", "garden", "eagle", "panda", "canyon", "island"]
        )
        self.messages = []
        self.inbox = []
        self.seq = -1
        self.running = False
        self.runner = None
        self.instructions_sent = False

    def info(self):
        value = {"id": self.id}
        if self.parent_id:
            value["parentID"] = self.parent_id
        value["projectID"] = self.project_id
        if self.model:
            value["model"] = dict(self.model)
        if self.agent:
            value["agent"] = self.agent
        value["cost"] = self.cost
        value["tokens"] = json.loads(json.dumps(self.tokens))
        if self.outcome:
            value["outcome"] = self.outcome
        value["time"] = {"created": self.created, "updated": self.updated}
        if self.idle:
            value["time"]["idle"] = self.idle
        if self.title is not None:
            value["title"] = self.title
        value["location"] = {"directory": self.directory}
        return value

    def next_seq(self):
        self.seq += 1
        return self.seq

    def add_usage(self, tokens_in, tokens_out):
        self.tokens["input"] += tokens_in
        self.tokens["output"] += tokens_out
        self.cost = round(self.cost + cost_of(tokens_in, tokens_out), 12)


class SseClient:
    def __init__(self, number, sock):
        self.number = number
        self.sock = sock
        self.queue = queue.Queue()
        self.drop_after = None
        self.sent = 0


class Server:
    def __init__(self, args):
        self.args = args
        self.password = args.password
        self.lock = threading.RLock()
        self.log_lock = threading.Lock()
        self.ids = Ids()
        self.seq = 0
        self.log_path = args.log_file
        self.pid = random.randrange(100, 30000)
        self.address = None
        self.httpd = None
        self.connections = set()
        self.sse_clients = []
        self.sse_count = 0
        self.pending_drop_after = args.drop_sse_after
        self.delays = {}
        self.fail_routes = {}
        for item in args.delay or []:
            route, _, ms = item.partition("=")
            self.delays[route] = int(ms)
        if args.delay_bootstrap_ms:
            for route in ("server.info", "project.list", "session.list", "session.active"):
                self.delays.setdefault(route, args.delay_bootstrap_ms)
        self.races = {}
        for item in args.race or []:
            route, _, kind = item.partition(":")
            self.races[route] = {"kind": kind or "rename", "once": True}
        self.reply_404 = args.reply_404
        self.models_empty_remaining = 1 if args.models_empty_once else 0
        self.catalog_empty = False
        self.restarting = threading.Event()
        self.restart_done = threading.Event()
        self.stopping = False
        self.sessions = {}
        self.projects = {}
        self.permissions = {}
        self.permission_waiters = {}
        self.forms = {}
        self.inbox_index = {}
        # Shell commands by ID: {"info": ShellInfo, "directory": location}.
        self.shells = {}
        self._seed()

    # ------------------------------------------------------------------ log

    def record(self, kind, **fields):
        with self.log_lock:
            self.seq += 1
            entry = {"seq": self.seq, "t": now_ms(), "kind": kind, **fields}
            if self.log_path:
                with open(self.log_path, "a", encoding="utf-8") as stream:
                    stream.write(json.dumps(entry) + "\n")
            if self.args.log_stdout:
                print(json.dumps(entry), flush=True)
            return entry

    # ----------------------------------------------------------------- seed

    def _add_project(self, directory, created, vcs=None):
        if directory in self.projects:
            return
        project = {"id": project_id(directory), "canonical": directory}
        if vcs:
            project["vcs"] = vcs
        project["time"] = {"created": created, "updated": created}
        project["sandboxes"] = []
        self.projects[directory] = project

    def _seed(self):
        args = self.args
        start = now_ms() - 6 * 3600 * 1000
        self._add_project(args.cwd, start - 1000)
        self._add_project(args.workspace, start - 500, vcs="git")
        self._add_project(args.other_directory, start - 400, vcs="git")

        main = Session(SES_MAIN, args.workspace, start, title="Integration session")
        other = Session(SES_OTHER, args.other_directory, start + 1000, title="Second session")
        child = Session(
            SES_CHILD, args.workspace, start + 2000, title="Mock child task", parent_id=SES_MAIN, agent="general"
        )
        for session in (main, other, child):
            session.instructions_sent = True
            session.outcome = "succeeded"
            self.sessions[session.id] = session

        clock = [start + 3000]

        def tick(step=700):
            clock[0] += step
            return clock[0]

        def seed_turn(session, text, kind="text", files=None):
            user_id = self.ids.ascending("msg", tick())
            session.messages.append(user_entry(user_id, clock[0], text, files))
            created = tick(20)
            assistant = assistant_entry(self.ids.ascending("msg", created), created, "build", default_model_ref())
            if kind == "reasoning":
                assistant["content"].append(
                    {
                        "type": "reasoning",
                        "text": "Let me think about this carefully.",
                        "state": {"reasoningField": "reasoning_content"},
                        "time": {"created": created + 5, "completed": created + 8},
                    }
                )
                assistant["content"].append({"type": "text", "text": "After thinking, the answer is 42."})
                finish_assistant(assistant, created + 10, "stop", 42, 5)
            elif kind == "tools":
                assistant["content"].append({"type": "text", "text": "Running two tools at once."})
                assistant["content"].append(
                    completed_tool("call_mock_read", "read", {"path": "README.md"}, README_TEXT, {"truncated": False}, created)
                )
                assistant["content"].append(
                    completed_tool(
                        "call_mock_glob",
                        "glob",
                        {"pattern": "**/*.txt"},
                        f"{args.workspace}/notes.txt",
                        {"count": 1, "truncated": False},
                        created,
                    )
                )
                finish_assistant(assistant, created + 25, "tool-calls", 42, 12)
                session.messages.append(assistant)
                created = tick(20)
                assistant = assistant_entry(self.ids.ascending("msg", created), created, "build", default_model_ref())
                assistant["content"].append({"type": "text", "text": "Both tool calls finished."})
                finish_assistant(assistant, created + 10, "stop", 42, 2)
            elif kind == "error":
                assistant["time"]["completed"] = created + 7
                assistant["finish"] = "error"
                assistant["error"] = {"type": "provider.internal", "message": "Mock provider exploded", "status": 500}
            else:
                assistant["content"].append({"type": "text", "text": text_reply(text)})
                finish_assistant(assistant, created + 10, "stop", 42, 3)
            session.messages.append(assistant)
            if kind == "subagent":
                synthetic_id = self.ids.ascending("msg", tick(20))
                session.messages.append(
                    synthetic_entry(
                        synthetic_id,
                        clock[0],
                        f'<subagent sessionID="{SES_CHILD}" state="completed" description="Mock child task">\n'
                        "Hello from the child subagent.\n</subagent>",
                        "Mock child task",
                        {"source": "subagent", "childID": SES_CHILD, "agent": "General", "state": "completed"},
                    )
                )
            idle_at = tick(10)
            session.messages.append(
                idle_entry(self.ids.ascending("msg", idle_at), idle_at, "failed" if kind == "error" else "succeeded")
            )
            session.add_usage(42, 3)
            session.updated = clock[0]
            session.idle = clock[0]

        seed_turn(main, "Say hello. [[scenario:text]]")
        seed_turn(main, "Think first. [[scenario:reasoning]]", "reasoning")
        seed_turn(main, "Read two things at once. [[scenario:tools]]", "tools")
        seed_turn(
            main,
            "Describe the attached image. [[scenario:text]]",
            files=[{"data": PIXEL_PNG, "mime": "image/png", "source": {"type": "inline"}, "name": "pixel.png"}],
        )
        seed_turn(main, "Delegate this. [[scenario:subagent]]", "subagent")
        seed_turn(main, "Fail please. [[scenario:error]]", "error")
        for turn in range(1, args.history_turns + 1):
            seed_turn(main, f"History turn {turn}. [[scenario:text]]")
        seed_turn(child, "You are a subagent spawned by another session.\n[[scenario:child]] Reply with a short greeting.")
        seed_turn(other, "Second session prompt. [[scenario:text]]")
        main.updated = clock[0] + 5000
        other.updated = clock[0] + 4000
        for session in (main, other, child):
            session.seq = len(session.messages) * 4

        for index in range(args.extra_sessions):
            at = start - 60_000 * (index + 1)
            filler = Session(
                self.ids.descending("ses", at), args.workspace, at, title=f"Filler session {index + 1:03d}"
            )
            filler.outcome = "succeeded"
            filler.instructions_sent = True
            self.sessions[filler.id] = filler

        if args.boot_permission:
            self.permissions[BOOT_PERMISSION] = {
                "id": BOOT_PERMISSION,
                "sessionID": SES_MAIN,
                "action": "read",
                "resources": [f"{args.workspace}/../outside.txt"],
                "source": {"type": "tool", "messageID": self.ids.ascending("msg"), "id": "call_boot_read"},
            }
        if args.boot_form:
            self.forms[BOOT_FORM] = self._new_form(BOOT_FORM, SES_MAIN, args.workspace, "Boot form")
        if args.background_jobs:
            now = now_ms()
            bg_child = Session(
                SES_BG_CHILD, args.workspace, now - 4 * 60_000, title="Audit v1 call sites", parent_id=SES_MAIN, agent="general"
            )
            bg_child.running = True
            bg_child.instructions_sent = True
            owner = Session(SES_BG_OWNER, args.other_directory, start - 30_000, title="Reflection projection retries")
            owner.outcome = "succeeded"
            owner.instructions_sent = True
            for session in (bg_child, owner):
                self.sessions[session.id] = session
            self.create_shell(
                args.workspace, "pnpm dev --port 5173", SES_BG_CHILD, started=now - 22 * 60_000, shell_id=SH_BG_DEV, emit=False
            )
            self.create_shell(
                args.other_directory, "cargo test --all-targets", SES_BG_OWNER, started=now - 60_000, shell_id=SH_BG_TEST, emit=False
            )

    def _new_form(self, form_id, session_id, directory, title):
        return {
            "record": {
                "id": form_id,
                "sessionID": session_id,
                "title": title,
                "metadata": {"source": "harness"},
                "fields": json.loads(json.dumps(HARNESS_FORM_FIELDS)),
            },
            "directory": directory,
            "status": "pending",
        }

    # --------------------------------------------------------------- events

    def emit(self, event_type, data, session=None, directory=None, metadata=None, version=1):
        event = {"id": self.ids.ascending("evt"), "created": now_ms()}
        if metadata is not None:
            event["metadata"] = metadata
        event["type"] = event_type
        if event_type not in NO_LOCATION:
            where = directory or (session.directory if session else None)
            if where:
                event["location"] = {"directory": where}
        event["data"] = data
        if event_type not in NOT_DURABLE and session is not None:
            event["durable"] = {"aggregateID": session.id, "seq": session.next_seq(), "version": version}
        self.broadcast(event)
        return event

    def broadcast(self, event):
        with self.lock:
            clients = list(self.sse_clients)
        extra = {}
        data = event.get("data")
        if isinstance(data, dict) and "inboxID" in data:
            extra["inboxID"] = data["inboxID"]
        self.record("event", type=event.get("type"), id=event.get("id"), sessionID=_event_session(event), **extra)
        for client in clients:
            client.queue.put(event)

    def drop_sse(self, after=None):
        with self.lock:
            clients = list(self.sse_clients)
            if after is not None and not clients:
                self.pending_drop_after = after
        for client in clients:
            if after is None:
                client.queue.put(None)
            else:
                client.drop_after = after
        return len(clients)

    # ------------------------------------------------------------- lookups

    def session(self, session_id):
        if not isinstance(session_id, str) or not session_id.startswith("ses"):
            raise invalid_request('Expected a string starting with "ses"\n  at ["sessionID"]', kind="Params")
        with self.lock:
            session = self.sessions.get(session_id)
        if session is None:
            raise session_not_found(session_id)
        return session

    def location_directory(self, query, headers):
        directory = first(query, "location[directory]")
        if directory:
            return directory
        header = headers.get("x-opencode-directory")
        if header:
            return header
        return self.args.cwd

    # --------------------------------------------------------------- routes

    def info(self):
        return {"version": VERSION, "pid": self.pid, "urls": [self.address], "paths": {"tmp": "/state/tmp/opencode"}}

    def list_projects(self):
        with self.lock:
            return sorted(self.projects.values(), key=lambda project: project["time"]["created"], reverse=True)

    def list_sessions(self, query):
        limit = parse_limit(query, 50)
        cursor = first(query, "cursor")
        if cursor:
            try:
                decoded = unb64url(cursor)
                anchor = decoded["anchor"]
                anchor_key = (int(anchor["time"]), str(anchor["id"]))
                direction = anchor["direction"]
            except (ValueError, KeyError, TypeError, binascii.Error):
                raise ApiError(400, {"_tag": "InvalidCursorError", "message": "Invalid cursor"})
            filters = {key: decoded[key] for key in ("directory", "parentID") if key in decoded}
            order = decoded.get("order", "desc")
        else:
            anchor_key = None
            direction = "next"
            filters = {}
            if first(query, "directory") is not None:
                filters["directory"] = first(query, "directory")
            if first(query, "parentID") is not None:
                filters["parentID"] = first(query, "parentID")
            order = first(query, "order") or "desc"
        with self.lock:
            items = [session for session in self.sessions.values() if session_matches(session, filters)]
            items.sort(key=lambda session: (session.updated, session.id), reverse=order != "asc")
            keys = [(session.updated, session.id) for session in items]
            if anchor_key is None:
                page = items[:limit]
            else:

                def after(key):
                    return key < anchor_key if order != "asc" else key > anchor_key

                if direction == "previous":
                    before = [session for session, key in zip(items, keys) if not after(key) and key != anchor_key]
                    page = before[-limit:]
                else:
                    page = [session for session, key in zip(items, keys) if after(key)][:limit]
            data = [session.info() for session in page]
        encoded = dict(filters)
        if order == "asc":
            encoded["order"] = "asc"
        if data:
            head, tail = data[0], data[-1]
            cursor_value = {
                "previous": b64url(
                    {**encoded, "anchor": {"id": head["id"], "time": head["time"]["updated"], "direction": "previous"}}
                ),
                "next": b64url(
                    {**encoded, "anchor": {"id": tail["id"], "time": tail["time"]["updated"], "direction": "next"}}
                ),
            }
        else:
            cursor_value = {"previous": None, "next": None}
        return {"data": data, "cursor": cursor_value}

    def list_messages(self, session, query):
        cursor = first(query, "cursor")
        order = first(query, "order")
        if cursor and order:
            raise ApiError(400, {"_tag": "InvalidCursorError", "message": "Cursor cannot be combined with order"})
        limit = parse_limit(query, 50)
        anchor = None
        direction = "next"
        if cursor:
            try:
                decoded = unb64url(cursor)
                anchor = str(decoded["id"])
                order = decoded.get("order", "desc")
                direction = decoded.get("direction", "next")
            except (ValueError, KeyError, TypeError, binascii.Error):
                raise ApiError(400, {"_tag": "InvalidCursorError", "message": "Invalid cursor"})
        order = order or "desc"
        if order not in ("asc", "desc"):
            raise invalid_request('Expected "asc" | "desc"\n  at ["order"]', kind="Query")
        with self.lock:
            entries = list(session.messages)
        entries.sort(key=lambda entry: entry["id"], reverse=order == "desc")
        if anchor is None:
            page = entries[:limit]
        else:

            def after(entry_id):
                return entry_id < anchor if order == "desc" else entry_id > anchor

            if direction == "previous":
                page = [entry for entry in entries if not after(entry["id"]) and entry["id"] != anchor][-limit:]
            else:
                page = [entry for entry in entries if after(entry["id"])][:limit]
        page = json.loads(json.dumps(page))
        if page:
            cursor_value = {
                "previous": b64url({"id": page[0]["id"], "order": order, "direction": "previous"}),
                "next": b64url({"id": page[-1]["id"], "order": order, "direction": "next"}),
            }
        else:
            cursor_value = {"previous": None, "next": None}
        return {"data": page, "cursor": cursor_value}

    def catalog_is_empty(self, directory):
        """Observed at 2.0.8 startup: the catalog is empty until providers load, then model.updated."""
        with self.lock:
            if self.models_empty_remaining > 0 and not self.catalog_empty:
                self.models_empty_remaining -= 1
                self.catalog_empty = True

                def ready():
                    with self.lock:
                        self.catalog_empty = False
                    self.emit("model.updated", {}, directory=directory)

                threading.Timer(self.args.model_ready_ms / 1000, ready).start()
            return self.catalog_empty

    def list_models(self, directory):
        if self.catalog_is_empty(directory):
            return {"location": {"directory": directory}, "data": []}
        return {"location": {"directory": directory}, "data": json.loads(json.dumps(MODEL_CATALOG))}

    def default_model(self, directory):
        if self.catalog_is_empty(directory):
            return {"location": {"directory": directory}}
        return {"location": {"directory": directory}, "data": json.loads(json.dumps(MODEL_CATALOG[0]))}

    def create_session(self, body):
        location = body.get("location") if isinstance(body, dict) else None
        directory = location.get("directory") if isinstance(location, dict) else None
        directory = directory or self.args.cwd
        title = body.get("title")
        if title is not None and not isinstance(title, str):
            raise invalid_request('Expected string\n  at ["title"]')
        created = now_ms()
        with self.lock:
            session = Session(self.ids.descending("ses"), directory, created, title=title or None)
            self.sessions[session.id] = session
            self._add_project(directory, created, vcs="git")
        data = {
            "sessionID": session.id,
            "slug": session.slug,
            "version": VERSION,
            "projectID": session.project_id,
            "location": {"directory": directory},
            "subpath": "",
        }
        if session.title:
            data["title"] = session.title
        self.emit("session.created", data, session=session)
        return session

    def rename_session(self, session, body):
        title = body.get("title") if isinstance(body, dict) else None
        if not isinstance(title, str):
            raise invalid_request('Expected string\n  at ["title"]')
        if not title.strip():
            # The real server regenerates the title for "" (never send blank); the fake rejects it loudly.
            raise invalid_request("Blank titles are rejected by the fake server (the real server regenerates them)")
        with self.lock:
            session.title = title
            session.updated = now_ms()
        self.emit("session.renamed", {"sessionID": session.id, "title": title}, session=session)

    def select_model(self, session, body):
        model = body.get("model") if isinstance(body, dict) else None
        if not isinstance(model, dict) or not model.get("id") or not model.get("providerID"):
            raise invalid_request('Expected { providerID, id, variant? }\n  at ["model"]')
        known = next(
            (item for item in MODEL_CATALOG if item["id"] == model["id"] and item["providerID"] == model["providerID"]),
            None,
        )
        if known is None:
            raise invalid_request(f"Unknown model: {model['providerID']}/{model['id']}")
        ref = {"id": model["id"], "providerID": model["providerID"]}
        if model.get("variant"):
            ref["variant"] = model["variant"]
        with self.lock:
            session.model = ref
            session.updated = now_ms()
        self.emit("session.model.selected", {"sessionID": session.id, "model": dict(ref)}, session=session)

    def prompt(self, session, body):
        if not isinstance(body, dict):
            raise invalid_request("Expected an object")
        prompt_id = body.get("id")
        if prompt_id is not None and (not isinstance(prompt_id, str) or not prompt_id.startswith("msg_")):
            raise invalid_request('Expected a string starting with "msg"\n  at ["id"]')
        text = body.get("text", "")
        if not isinstance(text, str):
            raise invalid_request('Expected string\n  at ["text"]')
        files = decode_files(body.get("files"))
        delivery = body.get("delivery") or "steer"
        if delivery not in ("steer", "queue"):
            raise invalid_request('Expected "steer" | "queue"\n  at ["delivery"]')
        resume = body.get("resume", True)
        if not isinstance(resume, bool):
            raise invalid_request('Expected boolean\n  at ["resume"]')
        payload = {"text": text}
        if files:
            payload["files"] = files
        with self.lock:
            if prompt_id and prompt_id in self.inbox_index:
                existing_session, existing = self.inbox_index[prompt_id]
                if existing_session == session.id and existing["payload"] == payload:
                    return existing, False
                raise ApiError(409, {"_tag": "ConflictError", "message": f"Prompt id already used: {prompt_id}"})
            prompt_id = prompt_id or self.ids.ascending("msg")
            record = {
                "id": prompt_id,
                "sessionID": session.id,
                "time": {"created": now_ms()},
                "type": "user",
                "payload": payload,
                "delivery": delivery,
            }
            self.inbox_index[prompt_id] = (session.id, record)
            session.inbox.append(record)
            session.updated = now_ms()
        self.emit(
            "session.inbox.enqueued",
            {"inboxID": prompt_id, "sessionID": session.id, "item": {"type": "user", "payload": payload, "delivery": delivery}},
            session=session,
        )
        if resume:
            self.ensure_runner(session)
        return record, True

    def ensure_runner(self, session, scope="input"):
        """Wakes the session like the server's run coordinator: a live runner widens its scope
        ("input" subsumes "steer"); one that is stopping is awaited, then a fresh one starts."""
        stopping = None
        with self.lock:
            runner = session.runner
            if runner is not None and runner.is_alive():
                if not runner.exiting:
                    if scope == "input":
                        runner.scope = "input"
                    return
                stopping = runner
        if stopping is not None:
            stopping.join(timeout=10)
        with self.lock:
            runner = session.runner
            if runner is not None and runner.is_alive() and not runner.exiting:
                if scope == "input":
                    runner.scope = "input"
                return
            session.runner = Runner(self, session, scope)
            session.runner.start()

    def interrupt(self, session, resume=False):
        """Without resume every waiting item stays parked. With resume the session wakes for its
        parked steers only (queued items stay parked), even when nothing was running."""
        with self.lock:
            runner = session.runner
            active = session.running and runner is not None and runner.is_alive()
        if active:
            runner.interrupt()
        if resume:
            with self.lock:
                steer = any(item.get("delivery", "steer") == "steer" for item in session.inbox)
            if steer:
                self.ensure_runner(session, "steer")
        return {"interrupted": bool(active)}

    def update_inbox(self, session, inbox_id, body):
        """Conditional switch to the other mode: 409 when the item is gone or already has it."""
        delivery = body.get("delivery") if isinstance(body, dict) else None
        if delivery not in ("steer", "queue"):
            raise invalid_request('Expected "steer" | "queue"\n  at ["delivery"]')
        with self.lock:
            record = next((item for item in session.inbox if item["id"] == inbox_id), None)
            if record is None or record.get("delivery", "steer") == delivery:
                raise ApiError(
                    409,
                    {
                        "_tag": "ConflictError",
                        "message": f"Pending input cannot change to {delivery}: {inbox_id}",
                        "resource": inbox_id,
                    },
                )
            record["delivery"] = delivery
        self.emit(
            "session.inbox.delivery.changed",
            {"sessionID": session.id, "inboxID": inbox_id, "delivery": delivery},
            session=session,
        )
        if delivery == "steer":
            self.ensure_runner(session)

    def cancel_inbox(self, session, inbox_id):
        """204 even when the item is no longer waiting (the server swallows that conflict)."""
        with self.lock:
            record = next((item for item in session.inbox if item["id"] == inbox_id), None)
            if record is None:
                return
            session.inbox.remove(record)
        self.emit("session.inbox.cancelled", {"sessionID": session.id, "inboxID": inbox_id}, session=session)

    def permissions_for(self, directory=None, session_id=None):
        with self.lock:
            result = []
            for request in self.permissions.values():
                owner = self.sessions.get(request["sessionID"])
                if session_id is not None and request["sessionID"] != session_id:
                    continue
                if directory is not None and (owner is None or owner.directory != directory):
                    continue
                result.append(json.loads(json.dumps(request)))
            return result

    def ask_permission(self, session, action, resources, save=None, source=None, request_id=None):
        request = {"id": request_id or self.ids.ascending("per"), "sessionID": session.id, "action": action, "resources": resources}
        if save:
            request["save"] = save
        if source:
            request["source"] = source
        waiter = {"event": threading.Event(), "decision": None}
        with self.lock:
            self.permissions[request["id"]] = request
            self.permission_waiters[request["id"]] = waiter
        self.emit("permission.asked", json.loads(json.dumps(request)), session=session)
        return request["id"], waiter

    def reply_permission(self, session, request_id, body):
        decision = body.get("decision") if isinstance(body, dict) else None
        if decision not in ("once", "always", "reject"):
            raise invalid_request('Expected "once" | "always" | "reject"\n  at ["decision"]')
        not_found = ApiError(
            404,
            {
                "_tag": "PermissionNotFoundError",
                "requestID": request_id,
                "message": f"Permission request not found: {request_id}",
            },
        )
        if self.reply_404:
            raise not_found
        with self.lock:
            request = self.permissions.get(request_id)
            if request is None or request["sessionID"] != session.id:
                raise not_found
            del self.permissions[request_id]
            waiter = self.permission_waiters.pop(request_id, None)
        self.emit(
            "permission.replied", {"sessionID": session.id, "requestID": request_id, "reply": decision}, session=session
        )
        if waiter:
            waiter["decision"] = decision
            waiter["event"].set()

    def forms_for(self, directory=None, session_id=None):
        with self.lock:
            return [
                json.loads(json.dumps(form["record"]))
                for form in self.forms.values()
                if form["status"] == "pending"
                and (session_id is None or form["record"]["sessionID"] == session_id)
                and (directory is None or form["directory"] == directory)
            ]

    def form_owner(self, session_id, query, headers):
        if session_id == GLOBAL:
            return None, self.location_directory(query, headers)
        session = self.session(session_id)
        return session, session.directory

    def create_form(self, session_id, directory, title="Harness form", body=None):
        form_id = self.ids.ascending("frm")
        form = self._new_form(form_id, session_id, directory, title)
        if isinstance(body, dict):
            record = form["record"]
            record["title"] = body.get("title", record["title"])
            if "metadata" in body:
                record["metadata"] = body["metadata"]
            if isinstance(body.get("fields"), list):
                record["fields"] = body["fields"]
        with self.lock:
            self.forms[form_id] = form
            owner = self.sessions.get(session_id)
        self.emit("form.created", {"form": json.loads(json.dumps(form["record"]))}, session=owner, directory=directory)
        return form

    def settle_form(self, session_id, directory, form_id, status, answer=None):
        with self.lock:
            form = self.forms.get(form_id)
            if (
                self.reply_404
                or form is None
                or form["record"]["sessionID"] != session_id
                or (session_id == GLOBAL and form["directory"] != directory)
            ):
                raise ApiError(404, {"_tag": "FormNotFoundError", "id": form_id, "message": f"Form not found: {form_id}"})
            if form["status"] != "pending":
                raise ApiError(
                    409, {"_tag": "FormAlreadySettledError", "id": form_id, "message": f"Form already settled: {form_id}"}
                )
            form["status"] = status
            owner = self.sessions.get(session_id)
        event_type = "form.cancelled" if status == "cancelled" else "form.replied"
        data = {"id": form_id, "sessionID": session_id}
        if answer is not None:
            data["answer"] = answer
        self.emit(event_type, data, session=owner, directory=form["directory"])

    def form_detail(self, session_id, form_id):
        with self.lock:
            form = self.forms.get(form_id)
            if form is None or form["record"]["sessionID"] != session_id:
                raise ApiError(404, {"_tag": "FormNotFoundError", "id": form_id, "message": f"Form not found: {form_id}"})
            detail = json.loads(json.dumps(form["record"]))
            detail["state"] = {"status": form["status"]}
            return detail

    def active(self):
        with self.lock:
            return {session.id: {"type": "running"} for session in self.sessions.values() if session.running}

    # --------------------------------------------------------------- shells

    def list_shells(self, directory):
        """Running commands of one location only, like ``Shell.list``."""
        with self.lock:
            return [
                json.loads(json.dumps(shell["info"]))
                for shell in self.shells.values()
                if shell["directory"] == directory and shell["info"]["status"] == "running"
            ]

    def create_shell(self, directory, command, session_id=None, started=None, shell_id=None, emit=True):
        shell_id = shell_id or self.ids.ascending("sh")
        info = {
            "id": shell_id,
            "status": "running",
            "command": command,
            "cwd": directory,
            "shell": "/usr/bin/bash",
            "file": f"/state/data/opencode/shell/{project_id(directory)}/{shell_id}.out",
            "metadata": {"sessionID": session_id} if session_id else {},
            "time": {"started": started or now_ms()},
        }
        with self.lock:
            self.shells[shell_id] = {"info": info, "directory": directory}
        if emit:
            self.emit("shell.created", {"info": info}, directory=directory)
        return info

    def shell(self, shell_id):
        with self.lock:
            shell = self.shells.get(shell_id)
        if shell is None:
            raise ApiError(404, {"_tag": "ShellNotFoundError", "id": shell_id, "message": f"Shell command not found: {shell_id}"})
        return shell

    def exit_shell(self, shell_id, exit_code=0, status="exited"):
        shell = self.shell(shell_id)
        with self.lock:
            if shell["info"]["status"] != "running":
                return shell["info"]
            shell["info"]["status"] = status
            if exit_code is not None:
                shell["info"]["exit"] = exit_code
            shell["info"]["time"]["completed"] = now_ms()
        data = {"id": shell_id, "status": status}
        if exit_code is not None:
            data["exit"] = exit_code
        self.emit("shell.exited", data, directory=shell["directory"])
        return shell["info"]

    def delete_shell(self, shell_id):
        shell = self.shell(shell_id)
        with self.lock:
            self.shells.pop(shell_id, None)
        if shell["info"]["status"] == "running":
            shell["info"]["status"] = "killed"
        self.emit("shell.deleted", {"id": shell_id}, directory=shell["directory"])

    def set_running(self, session, running, outcome="succeeded"):
        """Marks a session running or idle and emits the execution event, without a scripted turn."""
        with self.lock:
            changed = session.running != running
            session.running = running
        if changed:
            self.emit(
                "session.execution.started" if running else f"session.execution.{outcome}",
                {"sessionID": session.id},
                session=session,
            )

    # -------------------------------------------------------------- control

    def summary(self):
        with self.lock:
            return {
                "address": self.address,
                "pid": self.pid,
                "seed": {
                    "main": SES_MAIN,
                    "other": SES_OTHER,
                    "child": SES_CHILD,
                    "bootPermission": BOOT_PERMISSION,
                    "bgChild": SES_BG_CHILD,
                    "bgOwner": SES_BG_OWNER,
                },
                "workspace": self.args.workspace,
                "otherDirectory": self.args.other_directory,
                "cwd": self.args.cwd,
                "sseClients": [client.number for client in self.sse_clients],
                "sseCount": self.sse_count,
                "permissions": list(self.permissions),
                "forms": [form_id for form_id, form in self.forms.items() if form["status"] == "pending"],
                "running": [session.id for session in self.sessions.values() if session.running],
                "shells": sorted(shell_id for shell_id, shell in self.shells.items() if shell["info"]["status"] == "running"),
                "sessions": len(self.sessions),
                "delays": dict(self.delays),
                "failRoutes": dict(self.fail_routes),
                "races": dict(self.races),
                "reply404": self.reply_404,
                "modelsEmptyRemaining": self.models_empty_remaining,
            }

    def control(self, body):
        action = body.get("action")
        result = {"ok": True, "action": action}
        if action == "drop_sse":
            result["dropped"] = self.drop_sse(body.get("after"))
        elif action == "delay":
            routes = body.get("routes") or [body.get("route")]
            for route in routes:
                if body.get("ms"):
                    self.delays[route] = int(body["ms"])
                else:
                    self.delays.pop(route, None)
        elif action == "fail_route":
            route = body["route"]
            count = int(body.get("count", 1))
            if count > 0:
                self.fail_routes[route] = count
            else:
                self.fail_routes.pop(route, None)
        elif action == "race":
            self.races[body.get("route", "session.list")] = {
                "kind": body.get("kind", "rename"),
                "once": body.get("once", True),
                "sessionID": body.get("sessionID"),
            }
        elif action == "reply_404":
            self.reply_404 = bool(body.get("enabled", True))
        elif action == "models_empty":
            self.models_empty_remaining = int(body.get("count", 1))
        elif action == "restart":
            self.restart(int(body.get("down_ms", 500)), bool(body.get("keep_pending", False)))
        elif action == "emit":
            event = dict(body["event"])
            event.setdefault("id", self.ids.ascending("evt"))
            event.setdefault("created", now_ms())
            event.setdefault("data", {})
            self.broadcast(event)
            result["id"] = event["id"]
        elif action == "select_model":
            session = self.session(body["sessionID"])
            self.select_model(session, {"model": body["model"]})
        elif action == "rename":
            session = self.session(body["sessionID"])
            self.rename_session(session, {"title": body["title"]})
        elif action == "ask_permission":
            session = self.session(body.get("sessionID", SES_MAIN))
            request_id, _ = self.ask_permission(
                session,
                body.get("permissionAction", "shell"),
                body.get("resources", ["echo permission-probe"]),
                save=body.get("save"),
                source=body.get("source"),
            )
            result["id"] = request_id
        elif action == "create_form":
            session_id = body.get("sessionID", SES_MAIN)
            directory = body.get("directory") or (
                self.session(session_id).directory if session_id != GLOBAL else self.args.workspace
            )
            result["id"] = self.create_form(session_id, directory, body.get("title", "Harness form"))["record"]["id"]
        elif action == "create_shell":
            ago = body.get("startedAgoMs")
            result["id"] = self.create_shell(
                body.get("directory") or self.args.workspace,
                body.get("command", "sleep 30"),
                body.get("sessionID"),
                started=now_ms() - ago if ago is not None else None,
                shell_id=body.get("id"),
            )["id"]
        elif action == "exit_shell":
            self.exit_shell(body["id"], body.get("exit", 0), body.get("status", "exited"))
        elif action == "delete_shell":
            self.delete_shell(body["id"])
        elif action == "set_running":
            self.set_running(self.session(body["sessionID"]), bool(body.get("running", True)), body.get("outcome", "succeeded"))
        elif action == "state":
            pass
        else:
            raise invalid_request(f"Unknown control action: {action}")
        self.record("control", action=action, args={key: value for key, value in body.items() if key not in ("action", "event")})
        result["state"] = self.summary()
        return result

    def run_race(self, route, context):
        race = self.races.get(route)
        if race is None:
            return
        if race.get("once", True):
            self.races.pop(route, None)
        kind = race["kind"]
        self.record("race", route=route, race=kind)
        if kind == "rename":
            target = self.session(race.get("sessionID") or SES_OTHER)
            self.rename_session(target, {"title": "Raced title"})
        elif kind == "create":
            self.create_session({"location": {"directory": self.args.workspace}, "title": "Raced session"})
        elif kind == "delete":
            target_id = race.get("sessionID") or SES_OTHER
            with self.lock:
                target = self.sessions.pop(target_id, None)
            if target:
                self.emit("session.deleted", {"sessionID": target_id}, session=target)
        elif kind == "append":
            session = context or self.session(race.get("sessionID") or SES_MAIN)
            Runner(self, session).append_turn_now("Raced reply while the page was loading.")
        time.sleep(self.args.race_gap_ms / 1000)

    def restart(self, down_ms, keep_pending):
        def worker():
            time.sleep(0.05)
            self.restarting.set()
            self.restart_done.clear()
            self.record("restart", phase="down", down_ms=down_ms)
            httpd = self.httpd
            httpd.shutdown()
            with self.lock:
                clients = list(self.sse_clients)
                connections = list(self.connections)
                for session in self.sessions.values():
                    if session.runner is not None and session.runner.is_alive():
                        session.runner.interrupt(silent=True)
                    session.running = False
                # Shell commands live in the server process.
                self.shells.clear()
                if not keep_pending:
                    self.permissions.clear()
                    for waiter in self.permission_waiters.values():
                        waiter["event"].set()
                    self.permission_waiters.clear()
                    for form in self.forms.values():
                        if form["status"] == "pending":
                            form["status"] = "cancelled"
                self.pid += 1
            for client in clients:
                client.queue.put(None)
            for connection in connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            httpd.server_close()
            time.sleep(down_ms / 1000)
            self.restarting.clear()
            self.restart_done.set()

        threading.Thread(target=worker, daemon=True).start()


class Runner(threading.Thread):
    """Plays one session's inbox through scripted scenarios, like the real server loop."""

    def __init__(self, server, session, scope="input"):
        super().__init__(daemon=True)
        self.server = server
        self.session = session
        # "steer": deliver steered items only; "input": steers first, then one queued item per turn.
        self.scope = scope
        # Set once the loop decided to end (or was interrupted); wakes then start a new runner.
        self.exiting = False
        self.stop = threading.Event()
        self.silent = False
        self.assistant = None
        self.parts = {}
        self.open_parts = []
        self.step_open = False
        self.finalizing = False

    # ---- plumbing

    def interrupt(self, silent=False):
        self.silent = silent
        self.exiting = True
        self.stop.set()

    def sleep(self, ms):
        if self.stop.wait(ms / 1000):
            raise Interrupted()

    def pause(self):
        if self.finalizing:
            return
        self.sleep(self.server.args.step_delay_ms)

    def emit(self, event_type, data, session=None, **kwargs):
        if self.silent:
            return None
        return self.server.emit(event_type, data, session=session or self.session, **kwargs)

    def model_ref(self, session=None):
        session = session or self.session
        return dict(session.model) if session.model else default_model_ref()

    def set_running(self, session, running):
        with self.server.lock:
            session.running = running

    # ---- main loop

    def run(self):
        session = self.session
        executing = False
        try:
            while True:
                batch = self.next_batch(session)
                if not batch:
                    break
                if not executing:
                    self.set_running(session, True)
                    self.emit("session.execution.started", {"sessionID": session.id})
                    executing = True
                    self.pause()
                self.maybe_instructions(session)
                for item in batch:
                    self.deliver(session, item)
                text = batch[-1]["payload"].get("text", "")
                match = SCENARIO_RE.search(text)
                scenario = match.group(1) if match else "text"
                if session.title is None:
                    self.usage(session, 42, 3)
                    with self.server.lock:
                        session.title = "Mock session title"
                    self.emit("session.renamed", {"sessionID": session.id, "title": session.title})
                    self.pause()
                outcome = getattr(self, "scenario_" + scenario.replace("-", "_"), self.scenario_text)()
                if outcome == "failed":
                    return
            if executing:
                self.finish_execution(session, "succeeded")
        except Interrupted:
            self.handle_interrupt(session)
        finally:
            self.set_running(session, False)

    def next_batch(self, session):
        """Like the server's inbox promotion: every waiting steer at once; otherwise, for an
        "input" wake, the oldest queued item. Nothing else ends the loop."""
        with self.server.lock:
            steers = [item for item in session.inbox if item.get("delivery", "steer") == "steer"]
            if steers:
                return steers
            if self.scope == "input" and session.inbox:
                return [session.inbox[0]]
            self.exiting = True
            return None

    def maybe_instructions(self, session):
        if session.instructions_sent:
            return
        session.instructions_sent = True
        self.emit(
            "session.instructions.updated",
            {"sessionID": session.id, "delta": {"core/environment": secrets.token_hex(32)}},
            session=session,
            metadata={"instructions": {"initial": True}},
            version=2,
        )

    def deliver(self, session, item):
        with self.server.lock:
            if item in session.inbox:
                session.inbox.remove(item)
            session.messages.append(user_entry(item["id"], now_ms(), item["payload"].get("text", ""), item["payload"].get("files")))
            session.updated = now_ms()
        self.emit("session.inbox.delivered", {"sessionID": session.id, "inboxID": item["id"]})
        self.pause()

    def finish_execution(self, session, outcome, error=None):
        data = {"sessionID": session.id}
        if error:
            data["error"] = error
        event = self.emit(f"session.execution.{outcome}", data, session=session)
        at = now_ms()
        event_id = event["id"] if event else self.server.ids.ascending("evt")
        with self.server.lock:
            session.messages.append(idle_entry("msg_" + event_id[4:], at, outcome))
            session.outcome = outcome
            session.idle = at
            session.updated = at
            session.running = False

    def handle_interrupt(self, session):
        if self.silent:
            return
        self.finalizing = True
        for kind, ordinal in list(self.open_parts):
            self.end_part(kind, ordinal)
        if self.step_open and self.assistant is not None:
            error = {"type": "aborted", "message": "Step interrupted"}
            self.emit("session.step.streamed", {"sessionID": session.id, "assistantMessageID": self.assistant["id"]})
            self.emit(
                "session.step.failed",
                {"sessionID": session.id, "assistantMessageID": self.assistant["id"], "error": error},
            )
            with self.server.lock:
                self.assistant["time"].setdefault("streamed", now_ms())
                self.assistant["time"]["completed"] = now_ms()
                self.assistant["finish"] = "error"
                self.assistant["error"] = error
            self.step_open = False
        event = self.emit("session.execution.interrupted", {"sessionID": session.id, "reason": "user"})
        at = now_ms()
        with self.server.lock:
            session.messages.append(idle_entry("msg_" + event["id"][4:], at, "interrupted"))
            session.outcome = "interrupted"
            session.idle = at
            session.updated = at

    # ---- building blocks (event + history in lockstep)

    def usage(self, session, tokens_in, tokens_out):
        with self.server.lock:
            session.add_usage(tokens_in, tokens_out)
            data = {"sessionID": session.id, "cost": session.cost, "tokens": json.loads(json.dumps(session.tokens))}
        self.emit("session.usage.updated", data, session=session)

    def begin_step(self, session=None, reuse=False):
        session = session or self.session
        started = now_ms()
        if not reuse or self.assistant is None:
            agent = session.agent or "build"
            self.assistant = assistant_entry(self.server.ids.ascending("msg"), started, agent, self.model_ref(session))
            self.parts = {}
            with self.server.lock:
                session.messages.append(self.assistant)
        self.step_open = True
        self.emit(
            "session.step.started",
            {
                "sessionID": session.id,
                "agent": self.assistant["agent"],
                "model": dict(self.assistant["model"]),
                "assistantMessageID": self.assistant["id"],
                "started": started,
            },
            session=session,
        )
        self.pause()

    def part_data(self, session, extra):
        return {"sessionID": session.id, "assistantMessageID": self.assistant["id"], **extra}

    def start_part(self, kind, ordinal=0, session=None):
        session = session or self.session
        part = {"type": kind, "text": ""}
        extra = {"ordinal": ordinal}
        if kind == "reasoning":
            part["state"] = {"reasoningField": "reasoning_content"}
            part["time"] = {"created": now_ms()}
            extra["state"] = {"reasoningField": "reasoning_content"}
        with self.server.lock:
            self.assistant["content"].append(part)
            self.parts[(kind, ordinal)] = part
        self.open_parts.append((kind, ordinal))
        self.emit(f"session.{kind}.started", self.part_data(session, extra), session=session)
        self.pause()

    def delta_part(self, kind, text, ordinal=0, session=None):
        session = session or self.session
        with self.server.lock:
            self.parts[(kind, ordinal)]["text"] += text
        self.emit(f"session.{kind}.delta", self.part_data(session, {"ordinal": ordinal, "delta": text}), session=session)

    def end_part(self, kind, ordinal=0, session=None):
        session = session or self.session
        part = self.parts[(kind, ordinal)]
        extra = {"ordinal": ordinal, "text": part["text"]}
        if kind == "reasoning":
            part["time"]["completed"] = now_ms()
            extra["state"] = {"reasoningField": "reasoning_content"}
        if (kind, ordinal) in self.open_parts:
            self.open_parts.remove((kind, ordinal))
        self.emit(f"session.{kind}.ended", self.part_data(session, extra), session=session)
        self.pause()

    def text_block(self, text, ordinal=0, session=None):
        self.start_part("text", ordinal, session)
        self.delta_part("text", text, ordinal, session)
        self.end_part("text", ordinal, session)

    def tool_start(self, call_id, name, session=None):
        session = session or self.session
        part = {"type": "tool", "id": call_id, "name": name, "executed": False,
                "state": {"status": "pending", "input": {}}, "time": {"created": now_ms()}}
        with self.server.lock:
            self.assistant["content"].append(part)
            self.parts[("tool", call_id)] = part
        self.emit("session.tool.input.started", self.part_data(session, {"id": call_id, "name": name}), session=session)
        self.pause()

    def tool_called(self, call_id, tool_input, session=None):
        session = session or self.session
        part = self.parts[("tool", call_id)]
        self.emit("session.tool.input.ended", self.part_data(session, {"id": call_id, "text": json.dumps(tool_input)}), session=session)
        with self.server.lock:
            part["state"] = {"status": "running", "input": tool_input}
            part["time"]["ran"] = now_ms()
        self.emit(
            "session.tool.called",
            self.part_data(session, {"id": call_id, "input": tool_input, "executed": False}),
            session=session,
        )
        self.pause()

    def tool_progress(self, call_id, metadata, session=None):
        session = session or self.session
        self.emit("session.tool.progress", self.part_data(session, {"id": call_id, "metadata": metadata}), session=session)
        self.pause()

    def tool_success(self, call_id, texts, metadata, session=None):
        session = session or self.session
        part = self.parts[("tool", call_id)]
        content = [{"type": "text", "text": text} for text in texts]
        with self.server.lock:
            part["state"] = {"status": "completed", "input": part["state"].get("input", {}), "content": content, "metadata": metadata}
            part["time"]["completed"] = now_ms()
        self.emit(
            "session.tool.success",
            self.part_data(session, {"id": call_id, "content": content, "metadata": metadata, "executed": False}),
            session=session,
        )
        self.pause()

    def tool_failed(self, call_id, error, session=None):
        session = session or self.session
        part = self.parts[("tool", call_id)]
        with self.server.lock:
            part["state"] = {"status": "error", "input": part["state"].get("input", {}), "error": error}
            part["time"]["completed"] = now_ms()
        self.emit(
            "session.tool.failed",
            self.part_data(session, {"id": call_id, "error": error, "executed": False}),
            session=session,
        )
        self.pause()

    def streamed(self, session=None):
        session = session or self.session
        with self.server.lock:
            self.assistant["time"]["streamed"] = now_ms()
        self.emit("session.step.streamed", self.part_data(session, {}), session=session)
        self.pause()

    def end_step(self, finish, tokens_out, session=None, raw=None):
        session = session or self.session
        raw = raw or ("tool_calls" if finish == "tool-calls" else finish)
        with self.server.lock:
            finish_assistant(self.assistant, now_ms(), finish, 42, tokens_out, raw)
        self.step_open = False
        self.emit(
            "session.step.ended",
            self.part_data(
                session,
                {"finish": finish, "rawFinish": raw, "cost": cost_of(42, tokens_out), "tokens": tokens(42, tokens_out)},
            ),
            session=session,
        )
        self.usage(session, 42, tokens_out)
        self.pause()

    def text_step(self, text, tokens_out=3, session=None):
        self.begin_step(session)
        self.text_block(text, 0, session)
        self.streamed(session)
        self.end_step("stop", tokens_out, session)

    def shell_run(self, call_id, session):
        shell_id = self.server.ids.ascending("sh")
        self.emit(
            "shell.created",
            {
                "info": {
                    "id": shell_id,
                    "status": "running",
                    "command": "echo permission-probe",
                    "cwd": session.directory,
                    "shell": "/usr/bin/bash",
                    "file": f"/state/data/opencode/shell/{session.project_id}/{shell_id}.out",
                    "metadata": {"sessionID": session.id},
                    "time": {"started": now_ms()},
                }
            },
            session=session,
        )
        self.tool_progress(call_id, {"shellID": shell_id}, session)
        self.emit("shell.exited", {"id": shell_id, "exit": 0, "status": "exited"}, session=session)
        self.tool_success(
            call_id,
            ["permission-probe\n", "Command exited with code 0."],
            {"status": "completed", "truncated": False, "exit": 0},
            session,
        )

    def shell_with_permission(self, session, call_id="call_mock_shell"):
        self.tool_start(call_id, "shell", session)
        self.tool_called(call_id, {"command": "echo permission-probe", "description": "Echo a probe string"}, session)
        self.streamed(session)
        decision = self.wait_decision(session, call_id)
        if decision == "reject":
            self.tool_failed(
                call_id, {"type": "permission.rejected", "message": "The user rejected permission to use this tool"}, session
            )
            self.end_step("tool-calls", 12, session)
            self.text_step("The shell command was rejected.", 2, session)
            return
        self.shell_run(call_id, session)
        self.end_step("tool-calls", 12, session)
        self.text_step("The shell command completed.", 2, session)

    def wait_decision(self, session, call_id):
        request_id, waiter = self.server.ask_permission(
            session,
            "shell",
            ["echo permission-probe"],
            save=["echo *"],
            source={"type": "tool", "messageID": self.assistant["id"], "id": call_id},
        )
        while not waiter["event"].wait(0.05):
            if self.stop.is_set():
                with self.server.lock:
                    self.server.permissions.pop(request_id, None)
                    self.server.permission_waiters.pop(request_id, None)
                raise Interrupted()
        if self.stop.is_set():
            raise Interrupted()
        return waiter["decision"] or "reject"

    def append_turn_now(self, text):
        """Race helper: a complete assistant turn emitted synchronously (no inbox)."""
        session = self.session
        self.server.args.step_delay_ms, saved = 0, self.server.args.step_delay_ms
        try:
            self.set_running(session, True)
            self.emit("session.execution.started", {"sessionID": session.id})
            self.text_step(text)
            self.finish_execution(session, "succeeded")
        finally:
            self.server.args.step_delay_ms = saved
            self.set_running(session, False)

    # ---- scenarios (sequences copied from tests/fixtures/v2-2.0.8/events)

    def scenario_text(self):
        self.text_step("Hello from the mock provider.")

    def scenario_child(self):
        self.text_step("Hello from the child subagent.")

    def scenario_reasoning(self):
        self.begin_step()
        self.start_part("reasoning")
        self.start_part("text")
        self.delta_part("reasoning", "Let me think about this carefully.")
        self.end_part("reasoning")
        self.delta_part("text", "After thinking, the answer is 42.")
        self.end_part("text")
        self.streamed()
        self.end_step("stop", 5)

    def scenario_tools(self):
        workspace = self.session.directory
        self.begin_step()
        self.start_part("text")
        self.tool_start("call_mock_read", "read")
        self.tool_start("call_mock_glob", "glob")
        self.tool_called("call_mock_read", {"path": "README.md"})
        self.tool_called("call_mock_glob", {"pattern": "**/*.txt"})
        self.delta_part("text", "Running two tools at once.")
        self.end_part("text")
        self.streamed()
        self.tool_success("call_mock_read", [README_TEXT], {"truncated": False})
        self.tool_success("call_mock_glob", [f"{workspace}/notes.txt"], {"count": 1, "truncated": False})
        self.end_step("tool-calls", 12)
        self.text_step("Both tool calls finished.", 2)

    def scenario_permission(self):
        self.begin_step()
        self.shell_with_permission(self.session)

    def scenario_child_permission(self):
        parent = self.session
        call_id = "call_mock_subagent_fg"
        self.begin_step()
        parent_assistant = self.assistant
        prompt = "[[scenario:permission]] Run the probe command."
        tool_input = {"agent": "general", "description": "Mock child shell task", "prompt": prompt}
        self.tool_start(call_id, "subagent")
        self.tool_called(call_id, tool_input)
        self.streamed()
        parent_parts = self.parts
        child = self.spawn_child(parent, "Mock child shell task", prompt, call_id)
        self.begin_step(child)
        self.shell_with_permission(child)
        self.finish_execution(child, "succeeded")
        self.assistant, self.parts = parent_assistant, parent_parts
        self.tool_success(
            call_id,
            [f'<subagent sessionID="{child.id}" state="completed">\nThe shell command completed.\n</subagent>'],
            {"sessionID": child.id, "status": "completed", "truncated": False},
        )
        self.end_step("tool-calls", 12)
        self.text_step("The child subagent finished.", 2)

    scenario_subagent_permission = scenario_child_permission

    def spawn_child(self, parent, description, prompt, call_id):
        created = now_ms()
        with self.server.lock:
            child = Session(
                self.server.ids.descending("ses"), parent.directory, created, title=description, parent_id=parent.id, agent="general"
            )
            self.server.sessions[child.id] = child
        self.emit(
            "session.created",
            {
                "sessionID": child.id,
                "slug": child.slug,
                "version": VERSION,
                "projectID": child.project_id,
                "parentID": parent.id,
                "location": {"directory": child.directory},
                "subpath": "",
                "title": description,
                "agent": "general",
            },
            session=child,
        )
        self.tool_progress(call_id, {"sessionID": child.id, "status": "running"}, parent)
        inbox_id = self.server.ids.ascending("msg")
        payload = {"text": f"You are a subagent spawned by another session.\n{prompt}"}
        self.emit(
            "session.inbox.enqueued",
            {"inboxID": inbox_id, "sessionID": child.id, "item": {"type": "user", "payload": payload, "delivery": "steer"}},
            session=child,
        )
        self.set_running(child, True)
        self.emit("session.execution.started", {"sessionID": child.id}, session=child)
        self.maybe_instructions(child)
        with self.server.lock:
            child.messages.append(user_entry(inbox_id, now_ms(), payload["text"]))
        self.emit("session.inbox.delivered", {"sessionID": child.id, "inboxID": inbox_id}, session=child)
        return child

    def scenario_subagent(self):
        parent = self.session
        call_id = "call_mock_subagent"
        prompt = "[[scenario:child]] Reply with a short greeting."
        self.begin_step()
        self.tool_start(call_id, "subagent")
        self.tool_called(call_id, {"agent": "general", "description": "Mock child task", "prompt": prompt, "background": True})
        self.streamed()
        # Background child: created now, runs after the parent's turn ends.
        created = now_ms()
        with self.server.lock:
            child = Session(
                self.server.ids.descending("ses"), parent.directory, created, title="Mock child task", parent_id=parent.id, agent="general"
            )
            self.server.sessions[child.id] = child
        self.emit(
            "session.created",
            {
                "sessionID": child.id,
                "slug": child.slug,
                "version": VERSION,
                "projectID": child.project_id,
                "parentID": parent.id,
                "location": {"directory": child.directory},
                "subpath": "",
                "title": "Mock child task",
                "agent": "general",
            },
            session=child,
        )
        self.tool_progress(call_id, {"sessionID": child.id, "status": "running"})
        inbox_id = self.server.ids.ascending("msg")
        payload = {"text": f"You are a subagent spawned by another session.\n{prompt}"}
        self.emit(
            "session.inbox.enqueued",
            {"inboxID": inbox_id, "sessionID": child.id, "item": {"type": "user", "payload": payload, "delivery": "steer"}},
            session=child,
        )
        self.tool_success(
            call_id,
            [SUBAGENT_BACKGROUND_TEXT.format(child=child.id)],
            {"sessionID": child.id, "status": "running", "truncated": False},
        )
        self.end_step("tool-calls", 12)
        self.set_running(child, True)
        self.emit("session.execution.started", {"sessionID": child.id}, session=child)
        self.maybe_instructions(child)
        with self.server.lock:
            child.messages.append(user_entry(inbox_id, now_ms(), payload["text"]))
        self.emit("session.inbox.delivered", {"sessionID": child.id, "inboxID": inbox_id}, session=child)
        self.text_step("Launched a background subagent.", 2)
        self.finish_execution(parent, "succeeded")
        # Later: the child finishes, then the parent receives a synthetic completion.
        self.sleep(self.server.args.subagent_delay_ms)
        self.assistant, self.parts = None, {}
        self.text_step("Hello from the child subagent.", 3, child)
        self.finish_execution(child, "succeeded")
        synthetic_id = self.server.ids.ascending("msg")
        text = (
            f'<subagent sessionID="{child.id}" state="completed" description="Mock child task">\n'
            "Hello from the child subagent.\n</subagent>"
        )
        metadata = {"source": "subagent", "childID": child.id, "agent": "General", "state": "completed"}
        self.emit(
            "session.inbox.enqueued",
            {
                "inboxID": synthetic_id,
                "sessionID": parent.id,
                "item": {
                    "type": "synthetic",
                    "payload": {"text": text, "description": "Mock child task", "metadata": metadata},
                    "delivery": "steer",
                },
            },
        )
        self.set_running(parent, True)
        self.emit("session.execution.started", {"sessionID": parent.id})
        with self.server.lock:
            parent.messages.append(synthetic_entry(synthetic_id, now_ms(), text, "Mock child task", metadata))
        self.emit("session.inbox.delivered", {"sessionID": parent.id, "inboxID": synthetic_id})
        self.pause()
        self.assistant = None
        self.text_step("Hello from the mock provider.", 3)

    def scenario_form(self):
        self.text_step("I need some input first.", 5)
        session = self.session
        self.server.create_form(session.id, session.directory, "Harness form")
        self.server.create_form(GLOBAL, session.directory, "MCP server needs input")

    def scenario_error(self):
        session = self.session
        self.begin_step()
        error = {"type": "provider.internal", "message": "Mock provider exploded", "status": 500}
        with self.server.lock:
            self.assistant["time"]["completed"] = now_ms()
            self.assistant["finish"] = "error"
            self.assistant["error"] = error
        self.step_open = False
        self.emit("session.step.failed", self.part_data(session, {"error": error}))
        self.pause()
        event = self.emit("session.execution.failed", {"sessionID": session.id, "error": error})
        at = now_ms()
        with self.server.lock:
            session.messages.append(idle_entry("msg_" + event["id"][4:], at, "failed"))
            session.outcome = "failed"
            session.idle = at
            session.updated = at
            session.running = False
        return "failed"

    def scenario_retry(self):
        self.begin_step()
        delay = self.server.args.retry_delay_ms
        self.emit(
            "session.retry.scheduled",
            self.part_data(
                self.session,
                {
                    "attempt": 2,
                    "at": now_ms() + delay,
                    "error": {"type": "provider.internal", "message": "Mock provider temporarily unavailable", "status": 503},
                },
            ),
        )
        self.sleep(delay)
        self.begin_step(reuse=True)
        self.text_block("Recovered after a retry.")
        self.streamed()
        self.end_step("stop", 2)

    def scenario_slow(self):
        self.begin_step()
        self.start_part("text")
        for index in range(self.server.args.slow_deltas):
            self.delta_part("text", f"slow-{index} ")
            self.sleep(self.server.args.slow_delay_ms)
        self.end_part("text")
        self.streamed()
        self.end_step("stop", self.server.args.slow_deltas)

    def scenario_long(self):
        # The server merges deltas, so 300 provider deltas arrive as one event.
        self.begin_step()
        self.start_part("text")
        self.delta_part("text", "".join(f"word{index} " for index in range(300)))
        self.end_part("text")
        self.streamed()
        self.end_step("stop", 303)


# ---------------------------------------------------------------------- shapes


def default_model_ref():
    return {"id": MODEL_CATALOG[0]["id"], "providerID": MODEL_CATALOG[0]["providerID"]}


def user_entry(entry_id, created, text, files=None):
    entry = {"id": entry_id, "time": {"created": created}, "text": text}
    if files:
        entry["files"] = json.loads(json.dumps(files))
    entry["type"] = "user"
    return entry


def idle_entry(entry_id, created, outcome):
    return {"id": entry_id, "time": {"created": created}, "type": "idle", "outcome": outcome}


def synthetic_entry(entry_id, created, text, description, metadata):
    return {
        "id": entry_id,
        "metadata": metadata,
        "time": {"created": created},
        "text": text,
        "description": description,
        "type": "synthetic",
    }


def assistant_entry(entry_id, created, agent, model):
    return {"id": entry_id, "time": {"created": created}, "type": "assistant", "agent": agent, "model": model, "content": []}


def finish_assistant(entry, completed, finish, tokens_in, tokens_out, raw=None):
    entry["time"].setdefault("streamed", completed - 1)
    entry["time"]["completed"] = completed
    entry["finish"] = finish
    entry["rawFinish"] = raw or ("tool_calls" if finish == "tool-calls" else finish)
    entry["cost"] = cost_of(tokens_in, tokens_out)
    entry["tokens"] = tokens(tokens_in, tokens_out)


def completed_tool(call_id, name, tool_input, text, metadata, created):
    return {
        "type": "tool",
        "id": call_id,
        "name": name,
        "executed": False,
        "state": {"status": "completed", "input": tool_input, "content": [{"type": "text", "text": text}], "metadata": metadata},
        "time": {"created": created + 8, "ran": created + 12, "completed": created + 20},
    }


def text_reply(prompt):
    return "Hello from the mock provider."


def sniff_mime(data):
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return "image/png"
    if data.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if data[:6] in (b"GIF87a", b"GIF89a"):
        return "image/gif"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    if data.startswith(b"%PDF-"):
        return "application/pdf"
    try:
        data.decode("utf-8")
        return "text/plain"
    except UnicodeDecodeError:
        return "application/octet-stream"


def parse_data_uri(uri):
    """Returns (declared mime, base64 text) or raises ValueError."""
    if not uri.startswith("data:"):
        raise ValueError("not a data URI")
    header, sep, encoded = uri[5:].partition(",")
    if not sep or not header.endswith(";base64"):
        raise ValueError("data URI must be base64")
    return header[: -len(";base64")], encoded


def decode_files(files):
    if files is None:
        return []
    if not isinstance(files, list):
        raise invalid_request('Expected array\n  at ["files"]', field="files")
    result = []
    for index, item in enumerate(files):
        if not isinstance(item, dict) or not isinstance(item.get("uri"), str):
            raise invalid_request(f'Expected {{ uri, name? }}\n  at ["files"][{index}]', field="files")
        uri = item["uri"]
        if not uri.startswith("data:"):
            # The real server would resolve file:// paths on the server machine; the GTK client must never send them.
            raise invalid_request(f"Only data: URIs are accepted by the fake server: files[{index}]", field="files")
        try:
            _, encoded = parse_data_uri(uri)
            raw = base64.b64decode(encoded, validate=True)
        except (ValueError, binascii.Error):
            raise invalid_request(f"Attachment is not valid base64: files[{index}]", field="files")
        if base64.b64encode(raw).decode() != encoded:
            raise invalid_request(f"Attachment base64 is not canonical: files[{index}]", field="files")
        if len(raw) > MAX_ATTACHMENT_BYTES:
            raise invalid_request(f"Attachment exceeds {MAX_ATTACHMENT_BYTES} bytes: files[{index}]", field="files")
        record = {"data": encoded, "mime": sniff_mime(raw), "source": {"type": "inline"}}
        if item.get("name"):
            record["name"] = item["name"]
        result.append(record)
    return result


def summarize_files(files):
    result = []
    for item in files if isinstance(files, list) else []:
        if not isinstance(item, dict):
            result.append({"invalid": True})
            continue
        uri = item.get("uri") if isinstance(item.get("uri"), str) else ""
        summary = {"name": item.get("name"), "scheme": uri.split(":", 1)[0] if ":" in uri else None, "keys": sorted(item)}
        if uri.startswith("data:"):
            try:
                declared, encoded = parse_data_uri(uri)
                raw = base64.b64decode(encoded, validate=True)
                summary.update(
                    declaredMime=declared,
                    bytes=len(raw),
                    sniffedMime=sniff_mime(raw),
                    canonical=base64.b64encode(raw).decode() == encoded,
                )
            except (ValueError, binascii.Error):
                summary["base64Error"] = True
        result.append(summary)
    return result


def summarize_body(route, body):
    if not isinstance(body, dict):
        return None if body is None else {"type": type(body).__name__}
    summary = {"keys": sorted(body)}
    if route == "session.prompt":
        summary["id"] = body.get("id")
        text = body.get("text")
        summary["text"] = text[:2000] if isinstance(text, str) else text
        if "files" in body:
            summary["files"] = summarize_files(body.get("files"))
        for key in ("delivery", "agent", "agents", "resume", "model"):
            if key in body:
                summary[key] = body[key]
    elif route == "session.create":
        for key in ("location", "title", "agent", "model"):
            if key in body:
                summary[key] = body[key]
    elif route == "session.update":
        summary["title"] = body.get("title")
    elif route == "session.inbox.update":
        summary["delivery"] = body.get("delivery")
    elif route == "session.switchModel":
        summary["model"] = body.get("model")
    elif route == "session.permission.reply":
        summary["decision"] = body.get("decision")
        if "message" in body:
            summary["message"] = body["message"]
        if "reply" in body:
            summary["reply"] = body["reply"]
    elif route == "session.form.reply":
        answer = body.get("answer")
        summary["answerKeys"] = sorted(answer) if isinstance(answer, dict) else None
    return summary


def session_matches(session, filters):
    directory = filters.get("directory")
    if directory is not None and session.directory != directory:
        return False
    parent = filters.get("parentID")
    if parent is not None:
        if parent == "null":
            return session.parent_id is None
        return session.parent_id == parent
    return True


def parse_limit(query, default):
    value = first(query, "limit")
    if value is None:
        return default
    try:
        limit = int(value)
    except ValueError:
        raise invalid_request('Expected a number\n  at ["limit"]', kind="Query")
    if limit < 1:
        raise invalid_request('Expected a positive number\n  at ["limit"]', kind="Query")
    return limit


def first(query, key):
    values = query.get(key)
    return values[0] if values else None


def _event_session(event):
    data = event.get("data")
    if not isinstance(data, dict):
        return None
    if isinstance(data.get("sessionID"), str):
        return data["sessionID"]
    form = data.get("form")
    if isinstance(form, dict):
        return form.get("sessionID")
    info = data.get("info")
    if isinstance(info, dict) and isinstance(info.get("metadata"), dict):
        return info["metadata"].get("sessionID")
    return None


# --------------------------------------------------------------------- routing

ROUTES = [
    ("GET", r"/api/info", "server.info"),
    ("GET", r"/api/project", "project.list"),
    ("GET", r"/api/session", "session.list"),
    ("POST", r"/api/session", "session.create"),
    ("GET", r"/api/session/active", "session.active"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)", "session.get"),
    ("PATCH", r"/api/session/(?P<sessionID>[^/]+)", "session.update"),
    ("POST", r"/api/session/(?P<sessionID>[^/]+)/model", "session.switchModel"),
    ("POST", r"/api/session/(?P<sessionID>[^/]+)/prompt", "session.prompt"),
    ("POST", r"/api/session/(?P<sessionID>[^/]+)/interrupt", "session.interrupt"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)/inbox", "session.inbox.list"),
    ("PATCH", r"/api/session/(?P<sessionID>[^/]+)/inbox/(?P<inboxID>[^/]+)", "session.inbox.update"),
    ("DELETE", r"/api/session/(?P<sessionID>[^/]+)/inbox/(?P<inboxID>[^/]+)", "session.inbox.cancel"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)/message", "message.list"),
    ("GET", r"/api/model", "model.list"),
    ("GET", r"/api/model/default", "model.default"),
    ("GET", r"/api/permission/request", "permission.request.list"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)/permission", "session.permission.list"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)/permission/(?P<requestID>[^/]+)", "session.permission.get"),
    ("POST", r"/api/session/(?P<sessionID>[^/]+)/permission/(?P<requestID>[^/]+)/reply", "session.permission.reply"),
    ("GET", r"/api/form", "form.list"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)/form", "session.form.list"),
    ("POST", r"/api/session/(?P<sessionID>[^/]+)/form", "session.form.create"),
    ("GET", r"/api/session/(?P<sessionID>[^/]+)/form/(?P<formID>[^/]+)", "session.form.get"),
    ("DELETE", r"/api/session/(?P<sessionID>[^/]+)/form/(?P<formID>[^/]+)", "session.form.cancel"),
    ("POST", r"/api/session/(?P<sessionID>[^/]+)/form/(?P<formID>[^/]+)/reply", "session.form.reply"),
    ("GET", r"/api/shell", "shell.list"),
    ("POST", r"/api/shell", "shell.create"),
    ("GET", r"/api/shell/(?P<shellID>[^/]+)", "shell.get"),
    ("DELETE", r"/api/shell/(?P<shellID>[^/]+)", "shell.remove"),
    ("GET", r"/api/event", "event.subscribe"),
]
COMPILED_ROUTES = [(method, re.compile(pattern + r"\Z"), name) for method, pattern, name in ROUTES]


def match_route(method, path):
    path_known = False
    for route_method, pattern, name in COMPILED_ROUTES:
        found = pattern.match(path)
        if found:
            path_known = True
            if route_method == method:
                return name, found.groupdict()
    return ("method.not_allowed" if path_known else None), {}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "opencode-fake/" + VERSION

    @property
    def app(self):
        return self.server.app

    def setup(self):
        super().setup()
        with self.app.lock:
            self.app.connections.add(self.connection)

    def finish(self):
        try:
            super().finish()
        finally:
            with self.app.lock:
                self.app.connections.discard(self.connection)

    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.dispatch("GET")

    def do_POST(self):
        self.dispatch("POST")

    def do_PATCH(self):
        self.dispatch("PATCH")

    def do_DELETE(self):
        self.dispatch("DELETE")

    def do_PUT(self):
        self.dispatch("PUT")

    # ---- responses

    def send_json(self, value, status=200, headers=None):
        body = compact(value).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        for name, header_value in (headers or {}).items():
            self.send_header(name, header_value)
        self.end_headers()
        self.wfile.write(body)
        self.wfile.flush()

    def send_empty(self, status=204):
        self.send_response(status)
        if status != 204:
            self.send_header("content-length", "0")
        self.end_headers()
        self.wfile.flush()

    def read_body(self):
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length) if length else b""
        if not raw:
            return None, raw
        try:
            return json.loads(raw), raw
        except ValueError:
            return ValueError("invalid json"), raw

    def auth_state(self):
        header = self.headers.get("authorization") or ""
        if not header:
            return "missing"
        scheme, _, token = header.partition(" ")
        if scheme.lower() != "basic":
            return "bad"
        try:
            user, _, password = base64.b64decode(token, validate=True).decode().partition(":")
        except (binascii.Error, UnicodeDecodeError):
            return "bad"
        if user == USERNAME and secrets.compare_digest(password, self.app.password):
            return "ok"
        return "bad"

    # ---- dispatch

    def dispatch(self, method):
        url = urlsplit(self.path)
        path = url.path
        query = parse_qs(url.query, keep_blank_values=True)
        if path.startswith("/__"):
            self.control_route(method, path)
            return
        body, _raw = self.read_body()
        route, params = match_route(method, path)
        auth = self.auth_state()
        log = {
            "method": method,
            "path": path,
            "route": route,
            "params": params,
            "query": {key: values[0] if len(values) == 1 else values for key, values in query.items()},
            "auth": auth,
            "cf": bool(self.headers.get("cf-access-client-id")),
        }
        if self.headers.get("x-opencode-directory"):
            log["directoryHeader"] = self.headers["x-opencode-directory"]
        if route:
            log["body"] = summarize_body(route, body if not isinstance(body, Exception) else None)
        if auth != "ok":
            self.send_response(401)
            self.send_header("www-authenticate", 'Basic realm="Secure Area"')
            self.send_header("content-length", "0")
            self.send_header("connection", "close")
            self.end_headers()
            self.close_connection = True
            self.app.record("http", status=401, **log)
            return
        if route == "event.subscribe":
            self.app.record("http", status=200, **log)
            self.event_stream()
            return
        delay = self.app.delays.get(route)
        if delay:
            time.sleep(delay / 1000)
        try:
            if isinstance(body, Exception):
                raise invalid_request("Invalid JSON body", kind="Body")
            if route is None:
                raise ApiError(404, {"_tag": "RouteNotFoundError", "message": f"Route not found: {method} {path}"})
            if route == "method.not_allowed":
                raise ApiError(405, {"_tag": "MethodNotAllowedError", "message": f"Method not allowed: {method} {path}"})
            with self.app.lock:
                remaining = self.app.fail_routes.get(route, 0)
                if remaining:
                    self.app.fail_routes[route] = remaining - 1
            if remaining:
                raise ApiError(503, {"_tag": "UnavailableError", "message": f"Injected transient failure for {route}"})
            status, value = self.route_request(route, params, query, body or {})
        except ApiError as error:
            status, value = error.status, error.body
            log["error"] = error.body.get("_tag") if isinstance(error.body, dict) else None
        except Exception as error:  # noqa: BLE001 - surface server bugs to the test
            status, value = 500, {"_tag": "UnknownError", "message": f"fake server bug: {error!r}", "ref": "fake"}
            log["error"] = "UnknownError"
            print(f"fake server error on {method} {path}: {error!r}", file=sys.stderr)
        self.app.record("http", status=status, **log)
        if value is None:
            self.send_empty(status)
        else:
            self.send_json(value, status)

    def route_request(self, route, params, query, body):
        app = self.app
        session_param = params.get("sessionID")
        if route == "server.info":
            return 200, app.info()
        if route == "project.list":
            return 200, app.list_projects()
        if route == "session.list":
            value = app.list_sessions(query)
            app.run_race(route, None)
            return 200, value
        if route == "session.create":
            if not isinstance(body, dict):
                raise invalid_request("Expected an object")
            return 200, {"data": app.create_session(body).info()}
        if route == "session.active":
            return 200, {"data": app.active()}
        if route in ("session.form.list", "session.form.create", "session.form.get", "session.form.cancel", "session.form.reply"):
            return self.handle_form(route, params, query, body)
        session = app.session(session_param) if session_param is not None else None
        if route == "session.get":
            value = {"data": session.info()}
            app.run_race(route, session)
            return 200, value
        if route == "session.update":
            app.rename_session(session, body)
            return 204, None
        if route == "session.switchModel":
            app.select_model(session, body)
            return 204, None
        if route == "session.prompt":
            record, _created = app.prompt(session, body)
            return 200, {"data": record}
        if route == "session.interrupt":
            resume = first(query, "resume")
            if resume not in (None, "true", "false"):
                raise invalid_request('Expected "true" | "false"\n  at ["resume"]', kind="Query")
            return 200, app.interrupt(session, resume == "true")
        if route == "session.inbox.list":
            with app.lock:
                return 200, {"data": json.loads(json.dumps(session.inbox))}
        if route == "session.inbox.update":
            app.update_inbox(session, params["inboxID"], body)
            return 204, None
        if route == "session.inbox.cancel":
            app.cancel_inbox(session, params["inboxID"])
            return 204, None
        if route == "message.list":
            value = app.list_messages(session, query)
            app.run_race(route, session)
            return 200, value
        if route in ("model.list", "model.default"):
            directory = app.location_directory(query, self.headers)
            if route == "model.list":
                return 200, app.list_models(directory)
            return 200, app.default_model(directory)
        if route == "permission.request.list":
            directory = app.location_directory(query, self.headers)
            return 200, {"location": {"directory": directory}, "data": app.permissions_for(directory=directory)}
        if route == "session.permission.list":
            return 200, {"data": app.permissions_for(session_id=session.id)}
        if route == "session.permission.get":
            found = [item for item in app.permissions_for(session_id=session.id) if item["id"] == params["requestID"]]
            if not found:
                raise ApiError(
                    404,
                    {
                        "_tag": "PermissionNotFoundError",
                        "requestID": params["requestID"],
                        "message": f"Permission request not found: {params['requestID']}",
                    },
                )
            return 200, {"data": found[0]}
        if route == "session.permission.reply":
            app.reply_permission(session, params["requestID"], body)
            return 204, None
        if route == "form.list":
            directory = app.location_directory(query, self.headers)
            return 200, {"location": {"directory": directory}, "data": app.forms_for(directory=directory)}
        if route == "shell.list":
            directory = app.location_directory(query, self.headers)
            return 200, {"location": {"directory": directory}, "data": app.list_shells(directory)}
        if route == "shell.create":
            if not isinstance(body, dict) or not isinstance(body.get("command"), str):
                raise invalid_request('Expected string\n  at ["command"]')
            directory = app.location_directory(query, self.headers)
            metadata = body.get("metadata") if isinstance(body.get("metadata"), dict) else {}
            info = app.create_shell(body.get("cwd") or directory, body["command"], metadata.get("sessionID"))
            return 200, {"location": {"directory": directory}, "data": info}
        if route == "shell.get":
            directory = app.location_directory(query, self.headers)
            return 200, {"location": {"directory": directory}, "data": app.shell(params["shellID"])["info"]}
        if route == "shell.remove":
            try:
                app.delete_shell(params["shellID"])
            except ApiError:
                pass
            return 204, None
        raise ApiError(404, {"_tag": "RouteNotFoundError", "message": f"Route not found: {route}"})

    def handle_form(self, route, params, query, body):
        app = self.app
        session, directory = app.form_owner(params["sessionID"], query, self.headers)
        owner_id = session.id if session else GLOBAL
        if route == "session.form.list":
            return 200, {"data": app.forms_for(directory=None if session else directory, session_id=owner_id)}
        if route == "session.form.create":
            return 200, {"data": app.create_form(owner_id, directory, body=body)["record"]}
        if route == "session.form.get":
            return 200, {"data": app.form_detail(owner_id, params["formID"])}
        if route == "session.form.cancel":
            app.settle_form(owner_id, directory, params["formID"], "cancelled")
            return 204, None
        answer = body.get("answer") if isinstance(body, dict) else None
        if not isinstance(answer, dict):
            raise invalid_request('Expected object\n  at ["answer"]')
        app.settle_form(owner_id, directory, params["formID"], "replied", answer)
        return 204, None

    # ---- SSE

    def write_chunk(self, data):
        self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
        self.wfile.flush()

    def event_stream(self):
        app = self.app
        self.send_response(200)
        for name, value in (
            ("cache-control", "no-cache, no-transform"),
            ("connection", "keep-alive"),
            ("content-type", "text/event-stream"),
            ("transfer-encoding", "chunked"),
            ("vary", "Origin"),
            ("x-accel-buffering", "no"),
            ("x-content-type-options", "nosniff"),
        ):
            self.send_header(name, value)
        self.end_headers()
        with app.lock:
            app.sse_count += 1
            client = SseClient(app.sse_count, self.connection)
            if app.pending_drop_after is not None:
                client.drop_after = app.pending_drop_after
                app.pending_drop_after = None
            app.sse_clients.append(client)
        app.record("sse.open", n=client.number)
        reason = "client"
        self.close_connection = True
        try:
            connected = {"id": app.ids.ascending("evt"), "type": "server.connected", "data": {}}
            self.write_chunk(f"data: {compact(connected)}\n\n".encode())
            self.write_chunk(b": heartbeat\n\n")
            while True:
                try:
                    event = client.queue.get(timeout=app.args.heartbeat_s)
                except queue.Empty:
                    self.write_chunk(b": heartbeat\n\n")
                    continue
                if event is None:
                    reason = "dropped"
                    break
                self.write_chunk(f"data: {compact(event)}\n\n".encode())
                client.sent += 1
                if client.drop_after is not None:
                    client.drop_after -= 1
                    if client.drop_after <= 0:
                        reason = "dropped-after"
                        break
            # Abrupt close: no terminating chunk, like a server crash or proxy reset.
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        except (BrokenPipeError, ConnectionResetError, OSError):
            reason = "client"
        finally:
            with app.lock:
                if client in app.sse_clients:
                    app.sse_clients.remove(client)
            app.record("sse.close", n=client.number, reason=reason, sent=client.sent)

    # ---- test control

    def control_route(self, method, path):
        body, _raw = self.read_body()
        try:
            if path == "/__control" and method == "GET":
                self.send_json(self.app.summary())
            elif path == "/__control" and method == "POST":
                if not isinstance(body, dict):
                    raise invalid_request("Expected an object")
                self.send_json(self.app.control(body))
            elif path == "/__log" and method == "GET":
                data = b""
                if self.app.log_path and os.path.exists(self.app.log_path):
                    with open(self.app.log_path, "rb") as stream:
                        data = stream.read()
                self.send_response(200)
                self.send_header("content-type", "application/x-ndjson")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            else:
                raise ApiError(404, {"_tag": "RouteNotFoundError", "message": f"Unknown control route {path}"})
        except ApiError as error:
            self.send_json(error.body, error.status)


class HttpServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--address-file", type=str, help="write http://host:port here once listening")
    parser.add_argument("--log-file", type=str, help="append the JSON-lines request log here")
    parser.add_argument("--log-stdout", action="store_true", help="also print log records to stdout")
    parser.add_argument("--password", default=os.environ.get("FAKE_OPENCODE_PASSWORD"),
                        help="Basic password (default: $FAKE_OPENCODE_PASSWORD, else random; see --password-file)")
    parser.add_argument("--password-file", type=str, help="write the effective password here (0600)")
    parser.add_argument("--workspace", default="/state/workspace")
    parser.add_argument("--other-directory", default="/state/other")
    parser.add_argument("--cwd", default="/state/home", help="directory used when no location is given")
    parser.add_argument("--history-turns", type=int, default=30, help="extra plain turns seeded into the main session")
    parser.add_argument("--extra-sessions", type=int, default=0, help="extra root sessions (to force list paging)")
    parser.add_argument("--boot-permission", action="store_true", help="seed a pending permission on the main session")
    parser.add_argument("--boot-form", action="store_true", help="seed a pending form on the main session")
    parser.add_argument("--background-jobs", action="store_true",
                        help="seed a running child session and two running shells (sidebar Background section)")
    parser.add_argument("--heartbeat-s", type=float, default=15.0)
    parser.add_argument("--step-delay-ms", type=int, default=20)
    parser.add_argument("--slow-delay-ms", type=int, default=500)
    parser.add_argument("--slow-deltas", type=int, default=20)
    parser.add_argument("--retry-delay-ms", type=int, default=1000)
    parser.add_argument("--subagent-delay-ms", type=int, default=1000)
    parser.add_argument("--model-ready-ms", type=int, default=300)
    parser.add_argument("--race-gap-ms", type=int, default=300)
    parser.add_argument("--drop-sse-after", type=int, help="the first SSE stream closes after N events")
    parser.add_argument("--delay", action="append", metavar="ROUTE=MS", help="delay a route, e.g. session.list=800")
    parser.add_argument("--delay-bootstrap-ms", type=int, default=0, help="delay info/project/session list/active")
    parser.add_argument("--race", action="append", metavar="ROUTE:KIND",
                        help="once: mutate state + emit an event while ROUTE is in flight (rename|create|delete|append)")
    parser.add_argument("--reply-404", action="store_true", help="permission reply and form settle routes return 404")
    parser.add_argument("--models-empty-once", action="store_true",
                        help="first /api/model returns [] then model.updated (observed at 2.0.8 startup)")
    return parser.parse_args(argv)


def main(argv=None):
    args = parse_args(argv)
    if not args.password:
        args.password = secrets.token_hex(16)
    if args.password_file:
        fd = os.open(args.password_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as stream:
            stream.write(args.password)
    if args.log_file:
        open(args.log_file, "a", encoding="utf-8").close()
    app = Server(args)
    port = args.port
    first_start = True
    while True:
        httpd = HttpServer((args.host, port), Handler)
        httpd.app = app
        app.httpd = httpd
        host, port = httpd.server_address[:2]
        if first_start:
            app.address = f"http://{host}:{port}"
            if args.address_file:
                tmp = args.address_file + ".tmp"
                with open(tmp, "w", encoding="utf-8") as stream:
                    stream.write(app.address + "\n")
                os.replace(tmp, args.address_file)
            first_start = False
        else:
            app.record("restart", phase="up", pid=app.pid)
        try:
            httpd.serve_forever(poll_interval=0.1)
        except KeyboardInterrupt:
            return
        if not app.restarting.is_set() and not app.restart_done.is_set():
            return
        app.restart_done.wait()
        app.restart_done.clear()


if __name__ == "__main__":
    main()
