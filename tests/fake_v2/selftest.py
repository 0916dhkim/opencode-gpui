#!/usr/bin/env python3
"""Self-test for tests/fake_opencode_server.py (no GTK needed).

Starts the fake on an ephemeral loopback port, hits every route, and checks
responses and SSE traffic against the real 2.0.8 captures in
tests/fixtures/v2-2.0.8: JSON shapes (keys/types inferred from all fixture
samples), exact bodies for the static error fixtures, SSE headers and framing,
per-scenario event type sequences, and every fault switch.

Usage: python3 tests/fake_v2/selftest.py [-v]
"""

import base64
import glob
import json
import os
import queue
import re
import secrets
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlencode, urlsplit

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "v2-2.0.8"
SERVER = ROOT / "tests" / "fake_opencode_server.py"
WORKSPACE = "/state/workspace"
CWD = "/state/home"
SES_MAIN = "ses_f90000000001ffeIntegration"
SES_OTHER = "ses_f90000000002ffeSecondSessn"
SES_CHILD = "ses_f90000000003ffeChildOfMain"
SES_BG_CHILD = "ses_f90000000004ffeBgChildTask"
SES_BG_OWNER = "ses_f90000000005ffeBgShellOwnr"
OTHER_DIR = "/state/other"
BOOT_PERMISSION = "per_000000000001BootPermissn1"
PIXEL = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
OPEN_KEYS = {"input", "metadata", "settings", "delta", "answer", "body"}
VERBOSE = "-v" in sys.argv

FAILURES = []
PASSES = [0]


def check(condition, message):
    if condition:
        PASSES[0] += 1
        if VERBOSE:
            print(f"  ok   {message}")
    else:
        FAILURES.append(message)
        print(f"  FAIL {message}")
    return condition


def section(title):
    print(f"== {title}")


# ------------------------------------------------------------------ fixtures


def fixture(name):
    return json.loads((FIXTURES / f"{name}.json").read_text())


def fixture_bodies(*patterns):
    bodies = []
    for pattern in patterns:
        for path in sorted(FIXTURES.glob(f"{pattern}.json")):
            body = json.loads(path.read_text())["response"]["body"]
            if body is not None:
                bodies.append(body)
    assert bodies, patterns
    return bodies


def fixture_events(scenario):
    return [json.loads(line) for line in (FIXTURES / "events" / f"{scenario}.jsonl").read_text().splitlines() if line]


# ---------------------------------------------------------- schema inference


def jtype(value):
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, (int, float)):
        return "number"
    if isinstance(value, str):
        return "string"
    if isinstance(value, list):
        return "array"
    return "object"


def discriminator(value):
    if isinstance(value, dict) and isinstance(value.get("type"), str):
        return value["type"]
    return "*"


def new_schema():
    return {"types": set(), "keys": {}, "required": None, "items": {}}


def add_sample(schema, value):
    kind = jtype(value)
    schema["types"].add(kind)
    if kind == "object":
        keys = set(value)
        schema["required"] = keys if schema["required"] is None else schema["required"] & keys
        for key, item in value.items():
            add_sample(schema["keys"].setdefault(key, new_schema()), item)
    elif kind == "array":
        for item in value:
            add_sample(schema["items"].setdefault(discriminator(item), new_schema()), item)
    return schema


def infer(samples):
    schema = new_schema()
    for sample in samples:
        add_sample(schema, sample)
    return schema


def validate(value, schema, path="$", errors=None, open_object=False):
    errors = [] if errors is None else errors
    kind = jtype(value)
    if schema["types"] and schema["types"] != {"null"} and kind not in schema["types"]:
        errors.append(f"{path}: {kind} not in {sorted(schema['types'])}")
        return errors
    if kind == "object" and not open_object and "object" in schema["types"]:
        for key in sorted(schema["required"] or ()):
            if key not in value:
                errors.append(f"{path}: missing {key}")
        for key, item in value.items():
            if key not in schema["keys"]:
                errors.append(f"{path}: unexpected key {key}")
                continue
            validate(item, schema["keys"][key], f"{path}.{key}", errors, key in OPEN_KEYS)
    elif kind == "array" and schema["items"]:
        for index, item in enumerate(value):
            sub = schema["items"].get(discriminator(item))
            if sub is None:
                errors.append(f"{path}[{index}]: unknown item kind {discriminator(item)}")
                continue
            validate(item, sub, f"{path}[{index}]", errors)
    return errors


def check_shape(value, samples, label):
    errors = validate(value, infer(samples))
    check(not errors, f"{label} matches fixture shape" + (f": {errors[:5]}" if errors else ""))


# ------------------------------------------------------------------- client


class Client:
    def __init__(self, base, password):
        self.base = base
        self.auth = "Basic " + base64.b64encode(f"opencode:{password}".encode()).decode()

    def request(self, method, path, query=None, body=None, auth=True, headers=None, timeout=10):
        url = self.base + path
        if query:
            url += "?" + (query if isinstance(query, str) else urlencode(query))
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(url, data=data, method=method)
        if auth:
            request.add_header("Authorization", auth if isinstance(auth, str) else self.auth)
        if data is not None:
            request.add_header("Content-Type", "application/json")
        for name, value in (headers or {}).items():
            request.add_header(name, value)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                is_json = response.headers.get("content-type") == "application/json"
                return response.status, dict(response.headers), (json.loads(raw) if raw and is_json else None), raw
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                parsed = json.loads(raw) if raw else None
            except ValueError:
                parsed = raw
            return error.code, dict(error.headers), parsed, raw

    def get(self, path, query=None, **kwargs):
        return self.request("GET", path, query, **kwargs)

    def post(self, path, body=None, **kwargs):
        return self.request("POST", path, body=body if body is not None else {}, **kwargs)

    def control(self, **body):
        status, _, value, _ = self.request("POST", "/__control", body=body, auth=False)
        assert status == 200, (status, value)
        return value


class SseReader:
    """Raw-socket SSE reader that de-chunks by hand to check the exact framing."""

    def __init__(self, base, auth):
        parts = urlsplit(base)
        self.sock = socket.create_connection((parts.hostname, parts.port), timeout=10)
        self.sock.sendall(
            f"GET /api/event HTTP/1.1\r\nHost: {parts.netloc}\r\nAuthorization: {auth}\r\n"
            "Accept: text/event-stream\r\n\r\n".encode()
        )
        self.file = self.sock.makefile("rb")
        status_line = self.file.readline().decode()
        self.status = int(status_line.split()[1])
        self.headers = {}
        while True:
            line = self.file.readline().decode()
            if line in ("\r\n", "\n", ""):
                break
            name, _, value = line.partition(":")
            self.headers[name.strip().lower()] = value.strip()
        self.frames = []
        self.events = []
        self.queue = queue.Queue()
        self.closed = threading.Event()
        self.clean_end = False
        self.lock = threading.Lock()
        if self.status == 200:
            threading.Thread(target=self._pump, daemon=True).start()
        else:
            self.closed.set()

    def _pump(self):
        buffer = b""
        try:
            self.sock.settimeout(None)
            while True:
                size_line = self.file.readline()
                if not size_line:
                    break
                size = int(size_line.strip(), 16)
                if size == 0:
                    self.clean_end = True
                    break
                chunk = self.file.read(size)
                self.file.read(2)
                buffer += chunk
                while b"\n\n" in buffer:
                    frame, buffer = buffer.split(b"\n\n", 1)
                    text = frame.decode() + "\n\n"
                    with self.lock:
                        self.frames.append(text)
                    if text.startswith("data: "):
                        event = json.loads(text[6:])
                        with self.lock:
                            self.events.append((time.monotonic(), event))
                        self.queue.put(event)
        except (OSError, ValueError):
            pass
        finally:
            self.closed.set()

    def wait(self, predicate, timeout=10, after=0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                for index, (_, event) in enumerate(self.events[after:], after):
                    if predicate(event):
                        return event, index
            if self.closed.is_set():
                break
            time.sleep(0.02)
        return None, None

    def snapshot(self):
        with self.lock:
            return [event for _, event in self.events]

    def close(self):
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


def types_of(events, drop=("server.connected",)):
    return [event["type"] for event in events if event["type"] not in drop]


def collapse(types):
    result = []
    for item in types:
        if not result or result[-1] != item:
            result.append(item)
    return result


def sequence_equal(ours, theirs, label, collapse_runs=False):
    a, b = (collapse(ours), collapse(theirs)) if collapse_runs else (ours, theirs)
    if a == b:
        check(True, f"{label}: event sequence matches fixture ({len(a)} events)")
        return
    mismatch = next((i for i, (x, y) in enumerate(zip(a, b)) if x != y), min(len(a), len(b)))
    check(
        False,
        f"{label}: event sequence differs at {mismatch}: ours={a[mismatch:mismatch + 4]} fixture={b[mismatch:mismatch + 4]}"
        f" (ours {len(a)}, fixture {len(b)})",
    )


# ----------------------------------------------------------------- the test


class Harness:
    def __init__(self, extra_args=()):
        self.tmp = tempfile.mkdtemp(prefix="fake-v2-selftest-")
        self.password = secrets.token_hex(12)
        self.address_file = os.path.join(self.tmp, "address")
        self.log_file = os.path.join(self.tmp, "log.jsonl")
        env = dict(os.environ, FAKE_OPENCODE_PASSWORD=self.password)
        self.process = subprocess.Popen(
            [
                sys.executable,
                str(SERVER),
                "--address-file", self.address_file,
                "--log-file", self.log_file,
                "--history-turns", "4",
                "--extra-sessions", "7",
                "--boot-permission",
                "--heartbeat-s", "1",
                "--step-delay-ms", "3",
                "--slow-delay-ms", "150",
                "--slow-deltas", "12",
                "--retry-delay-ms", "200",
                "--subagent-delay-ms", "200",
                "--model-ready-ms", "100",
                "--race-gap-ms", "300",
                "--models-empty-once",
                *extra_args,
            ],
            env=env,
        )
        deadline = time.monotonic() + 10
        while not os.path.exists(self.address_file):
            if time.monotonic() > deadline or self.process.poll() is not None:
                raise SystemExit("fake server did not start")
            time.sleep(0.05)
        self.base = open(self.address_file).read().strip()
        self.client = Client(self.base, self.password)
        self.readers = []

    def reader(self):
        reader = SseReader(self.base, self.client.auth)
        self.readers.append(reader)
        reader.wait(lambda event: event["type"] == "server.connected", 5)
        return reader

    def log(self):
        return [json.loads(line) for line in open(self.log_file) if line.strip()]

    def stop(self):
        for reader in self.readers:
            reader.close()
        self.process.terminate()
        try:
            self.process.wait(5)
        except subprocess.TimeoutExpired:
            self.process.kill()


def run_prompt(h, session_id, text, reader, terminal_session=None, files=None, prompt_id=None, timeout=15):
    body = {"text": text}
    if prompt_id:
        body["id"] = prompt_id
    if files:
        body["files"] = files
    start = len(reader.snapshot())
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/prompt", body)
    check(status == 200, f"prompt {text[:40]!r} -> 200 (got {status} {value})")
    return start, value


def wait_terminal(reader, session_id, after, timeout=15, count=1):
    found = 0
    index = after
    while found < count:
        event, idx = reader.wait(
            lambda e: e["type"] in ("session.execution.succeeded", "session.execution.failed", "session.execution.interrupted")
            and e["data"].get("sessionID") == session_id,
            timeout,
            index,
        )
        if event is None:
            return False
        found += 1
        index = idx + 1
    return True


def created_index(reader, session_id):
    _, index = reader.wait(lambda e: e["type"] == "session.created" and e["data"]["sessionID"] == session_id, 5)
    assert index is not None, f"no session.created for {session_id}"
    return index


def new_session(h, title=None):
    body = {"location": {"directory": WORKSPACE}}
    if title:
        body["title"] = title
    status, _, value, _ = h.client.post("/api/session", body)
    assert status == 200, (status, value)
    return value["data"]["id"]


def test_auth(h):
    section("auth")
    status, headers, body, raw = h.client.get("/api/info", auth=False)
    lower = {k.lower(): v for k, v in headers.items()}
    check(status == 401, "missing auth -> 401")
    check(lower.get("www-authenticate") == 'Basic realm="Secure Area"', "401 carries www-authenticate")
    check(raw == b"", "401 has no body")
    status, _, _, _ = h.client.get("/api/info", auth="Basic " + base64.b64encode(b"opencode:wrong").decode())
    check(status == 401, "wrong password -> 401")
    status, _, _, _ = h.client.get("/api/info", auth="Basic " + base64.b64encode(f"root:{h.password}".encode()).decode())
    check(status == 401, "wrong user -> 401")
    reader = SseReader(h.base, "Basic " + base64.b64encode(b"opencode:wrong").decode())
    check(reader.status == 401, "SSE with wrong auth -> 401")


def test_bootstrap(h):
    section("bootstrap routes")
    status, headers, info, _ = h.client.get("/api/info")
    check(status == 200 and info["version"] == "2.0.8", "info 200 version 2.0.8")
    check(headers.get("content-type") == "application/json", "JSON content-type")
    check_shape(info, fixture_bodies("info"), "info")
    status, _, projects, _ = h.client.get("/api/project")
    check(status == 200 and isinstance(projects, list), "project list is a bare array")
    check_shape(projects, fixture_bodies("project.list*"), "project list")
    check({p["canonical"] for p in projects} >= {WORKSPACE, CWD}, "projects include workspace and cwd")
    status, _, active, _ = h.client.get("/api/session/active")
    check(status == 200 and active == {"data": {}}, "session.active idle == fixture")
    # No session-list fixture has an empty page; assume the message-list convention (null cursors).
    session_page_samples = fixture_bodies("session.list.*") + [{"data": [], "cursor": {"previous": None, "next": None}}]

    # root paging
    seen, cursor, pages = [], None, 0
    while True:
        query = {"parentID": "null", "limit": 3}
        if cursor:
            query["cursor"] = cursor
        status, _, page, _ = h.client.get("/api/session", query)
        check(status == 200, f"session list page {pages} -> 200")
        check_shape(page, session_page_samples, f"session list page {pages}")
        pages += 1
        if not page["data"]:
            check(page["cursor"] == {"previous": None, "next": None}, "empty session page has null cursors")
            break
        seen.extend(item["id"] for item in page["data"])
        check(all("parentID" not in item for item in page["data"]), f"page {pages} has only root sessions")
        cursor = page["cursor"]["next"]
        if pages > 20:
            break
    check(len(seen) == len(set(seen)), "no duplicate sessions across pages")
    check(SES_MAIN in seen and SES_OTHER in seen and SES_CHILD not in seen, "root list has seeds but not the child")
    check(len(seen) == 9, f"all 9 root sessions paged (got {len(seen)} over {pages} pages)")
    decoded = json.loads(base64.urlsafe_b64decode(cursor + "=" * (-len(cursor) % 4)))
    check(decoded.get("parentID") == "null" and "limit" not in decoded, "cursor encodes parentID filter, not limit")

    status, _, page, _ = h.client.get("/api/session", {"limit": 100})
    check(any(item["id"] == SES_CHILD and item.get("parentID") == SES_MAIN for item in page["data"]), "unfiltered list includes children")
    status, _, page, _ = h.client.get("/api/session", {"limit": 100, "directory": "/state/other"})
    check([item["id"] for item in page["data"]] == [SES_OTHER], "directory filter")
    status, _, page, _ = h.client.get("/api/session", {"parentID": "null", "limit": 2, "order": "asc"})
    status2, _, page2, _ = h.client.get("/api/session", {"parentID": "null", "limit": 2, "order": "asc", "cursor": page["cursor"]["next"]})
    check(status2 == 200, "session list accepts order+cursor (unlike messages)")
    status, _, body, _ = h.client.get("/api/session", {"cursor": "not-a-cursor"})
    check(status == 400 and body.get("_tag") == "InvalidCursorError", "bad session cursor -> 400 InvalidCursorError")

    status, _, body, _ = h.client.get("/api/session/abc")
    check((status, body) == (400, fixture("error.400.invalidSessionID")["response"]["body"]), "invalid session id == fixture")
    status, _, body, _ = h.client.get("/api/session/ses_doesnotexist000000000000000")
    check((status, body) == (404, fixture("error.404.session")["response"]["body"]), "unknown session == fixture")
    status, _, body, _ = h.client.post("/api/session/ses_doesnotexist000000000000000/prompt", {"text": "hello"})
    check((status, body) == (404, fixture("error.404.promptUnknownSession")["response"]["body"]), "prompt unknown session == fixture")


def test_messages(h):
    section("message paging")
    samples = fixture_bodies("session.messages.*")
    ids, cursor, pages = [], None, 0
    while True:
        query = {"limit": 5}
        if cursor:
            query["cursor"] = cursor
        status, _, page, _ = h.client.get(f"/api/session/{SES_MAIN}/message", query)
        check_shape(page, samples, f"message page {pages}")
        pages += 1
        if not page["data"]:
            check(page["cursor"] == {"previous": None, "next": None}, "final empty message page has null cursors")
            break
        ids.extend(entry["id"] for entry in page["data"])
        cursor = page["cursor"]["next"]
        if pages > 50:
            break
    check(ids == sorted(ids, reverse=True), "pages are newest-first and contiguous")
    check(len(ids) == len(set(ids)), "no duplicate entries")
    kinds = set()
    status, _, full, _ = h.client.get(f"/api/session/{SES_MAIN}/message", {"limit": 500})
    for entry in full["data"]:
        kinds.add(entry["type"])
        for part in entry.get("content", []):
            kinds.add("part:" + part["type"])
    check({"user", "assistant", "idle", "synthetic", "part:text", "part:reasoning", "part:tool"} <= kinds, f"seed history covers entry kinds {sorted(kinds)}")
    check(any(entry.get("files") for entry in full["data"]), "seed history has a stored attachment")
    status, _, asc, _ = h.client.get(f"/api/session/{SES_MAIN}/message", {"limit": 3, "order": "asc"})
    check([e["id"] for e in asc["data"]] == sorted(ids)[:3], "order=asc returns oldest first")
    decoded = json.loads(base64.urlsafe_b64decode(asc["cursor"]["next"] + "=="))
    check(decoded.get("order") == "asc", "message cursor encodes order")
    status, _, body, _ = h.client.get(f"/api/session/{SES_MAIN}/message", {"limit": 3, "order": "asc", "cursor": asc["cursor"]["next"]})
    check((status, body) == (400, fixture("error.400.orderWithCursor")["response"]["body"]), "order+cursor == fixture 400")
    status, _, body, _ = h.client.get(f"/api/session/{SES_MAIN}/message", {"cursor": "%%%"})
    check(status == 400 and body.get("_tag") == "InvalidCursorError", "bad message cursor -> 400")


def test_models(h):
    section("models")
    status, _, first_list, _ = h.client.get("/api/model", {"location[directory]": WORKSPACE})
    check(status == 200 and first_list["data"] == [] and first_list["location"] == {"directory": WORKSPACE}, "--models-empty-once: first list is empty")
    status, _, default, _ = h.client.get("/api/model/default", {"location[directory]": WORKSPACE})
    check(status == 200 and "data" not in default, "default omits data while catalog is empty (UndefinedOr)")
    time.sleep(0.2)
    status, _, models, _ = h.client.get("/api/model", {"location[directory]": WORKSPACE})
    check(len(models["data"]) == 2, "second list is populated")
    check_shape(models, fixture_bodies("model.list*"), "model list")
    status, _, models, _ = h.client.get("/api/model", {"directory": WORKSPACE})
    check(models["location"] == {"directory": CWD}, "plain directory= is ignored (falls back to cwd)")
    status, _, models, _ = h.client.get("/api/model")
    check(models["location"] == {"directory": CWD}, "no location -> cwd")
    status, _, models, _ = h.client.get("/api/model", headers={"x-opencode-directory": WORKSPACE})
    check(models["location"] == {"directory": WORKSPACE}, "x-opencode-directory header selects location")
    status, _, default, _ = h.client.get("/api/model/default", {"location[directory]": WORKSPACE})
    check_shape(default, fixture_bodies("model.default"), "model default")
    status, _, agents, _ = h.client.get("/api/agent", {"location[directory]": WORKSPACE})
    check(
        status == 200
        and agents["location"] == {"directory": WORKSPACE}
        and agents["data"][0]["id"] == "build"
        and agents["data"][0]["model"] == {"id": "mock-model", "providerID": "mock", "variant": "high"},
        "agent list returns default build agent with model and variant",
    )


def test_lifecycle(h, reader):
    section("session lifecycle (create/rename/model)")
    start = len(reader.snapshot())
    status, _, created, _ = h.client.post("/api/session", {"location": {"directory": WORKSPACE}})
    check(status == 200, "create -> 200")
    check_shape(created, fixture_bodies("session.create"), "session create")
    session_id = created["data"]["id"]
    status, _, titled, _ = h.client.post("/api/session", {"location": {"directory": WORKSPACE}, "title": "Titled harness session"})
    check_shape(titled, fixture_bodies("session.create.titled"), "titled create")
    status, _, pinned, _ = h.client.post(
        "/api/session",
        {
            "location": {"directory": WORKSPACE},
            "agent": "build",
            "model": {"providerID": "mock", "id": "mock-model", "variant": "high"},
        },
    )
    check(
        status == 200
        and pinned["data"].get("agent") == "build"
        and pinned["data"].get("model") == {"id": "mock-model", "providerID": "mock", "variant": "high"},
        "create with agent and model persists both on session",
    )
    status, _, body, raw = h.client.request("PATCH", f"/api/session/{session_id}", body={"title": "Renamed by harness"})
    check(status == 204 and raw == b"", "rename -> 204 empty")
    status, _, body, _ = h.client.request("PATCH", f"/api/session/{session_id}", body={"title": "   "})
    check(status == 400 and body.get("_tag") == "InvalidRequestError", "blank rename -> 400")
    status, _, got, _ = h.client.get(f"/api/session/{session_id}")
    check(got["data"]["title"] == "Renamed by harness", "get after rename shows the title")
    check_shape(got, fixture_bodies("session.get*"), "session get")
    status, _, _, raw = h.client.post(f"/api/session/{session_id}/model", {"model": {"providerID": "mock", "id": "mock-model-alt", "variant": "high"}})
    check(status == 204 and raw == b"", "model switch -> 204")
    status, _, got, _ = h.client.get(f"/api/session/{session_id}")
    check(got["data"].get("model") == {"id": "mock-model-alt", "providerID": "mock", "variant": "high"}, "model persisted on session")
    check_shape(got, fixture_bodies("session.get.modelSwitched"), "session get (model switched)")
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/model", {"model": {"providerID": "mock", "id": "nope"}})
    check(status == 400, "unknown model -> 400")
    reader.wait(lambda e: e["type"] == "session.model.selected" and e["data"]["sessionID"] == session_id, 3, start)
    ours = [e for e in reader.snapshot()[start:] if e["data"].get("sessionID") in (session_id, titled["data"]["id"])]
    sequence_equal(types_of(ours), types_of(fixture_events("session-lifecycle")), "session-lifecycle")
    status, _, projects, _ = h.client.get("/api/project")
    check_shape(projects, fixture_bodies("project.list*"), "project list after create")


def events_for(reader, start, session_ids):
    return [e for e in reader.snapshot()[start:] if e["data"].get("sessionID") in session_ids or e["type"].startswith("shell.")]


def test_scenarios(h, reader):
    section("scenario event sequences")
    for scenario, fixture_name in (
        ("text", "text"),
        ("reasoning", "reasoning"),
        ("tools", "tools"),
        ("error", "error"),
        ("retry", "retry"),
        ("long", "long"),
    ):
        session_id = new_session(h)
        start = created_index(reader, session_id)
        run_prompt(h, session_id, f"Run {scenario}. [[scenario:{scenario}]]", reader, prompt_id=None)
        check(wait_terminal(reader, session_id, start), f"{scenario}: execution reached a terminal event")
        ours = events_for(reader, start, {session_id})
        sequence_equal(types_of(ours), types_of(fixture_events(fixture_name)), scenario)
        status, _, page, _ = h.client.get(f"/api/session/{session_id}/message", {"limit": 50})
        check_shape(page, fixture_bodies("session.messages.*"), f"{scenario} history")
        expected = fixture(f"session.messages.{fixture_name}")["response"]["body"]["data"] if (FIXTURES / f"session.messages.{fixture_name}.json").exists() else None
        if expected is not None:
            check([e["type"] for e in page["data"]] == [e["type"] for e in expected], f"{scenario}: history entry types match fixture")
            ours_parts = [[p["type"] for p in e.get("content", [])] for e in page["data"]]
            theirs_parts = [[p["type"] for p in e.get("content", [])] for e in expected]
            check(ours_parts == theirs_parts, f"{scenario}: history content kinds match fixture")
        idle_ids = [e["id"] for e in page["data"] if e["type"] == "idle"]
        terminal_ids = ["msg_" + e["id"][4:] for e in ours if e["type"].startswith("session.execution.") and e["type"] != "session.execution.started"]
        check(idle_ids and set(idle_ids) <= set(terminal_ids), f"{scenario}: idle entry id == terminal event id (evt_->msg_)")
        assistants = {e["id"] for e in page["data"] if e["type"] == "assistant"}
        streamed = {e["data"]["assistantMessageID"] for e in ours if "assistantMessageID" in e["data"]}
        check(assistants == streamed, f"{scenario}: history assistant ids == event assistantMessageIDs")
        if scenario == "tools":
            tool_events = [e for e in ours if e["type"] == "session.tool.success"]
            check(len({(e["data"]["assistantMessageID"], e["data"]["id"]) for e in tool_events}) == 2, "tools: two tool calls keyed by message+tool id")

    section("attachment")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    files = [{"uri": f"data:image/png;base64,{PIXEL}", "name": "pixel.png"}]
    _, value = run_prompt(h, session_id, "Describe the attached image. [[scenario:text]]", reader, files=files)
    check_shape(value, fixture_bodies("session.prompt*"), "prompt with attachment response")
    check(value["data"]["payload"]["files"][0]["mime"] == "image/png", "attachment stored with sniffed mime")
    wait_terminal(reader, session_id, start)
    sequence_equal(types_of(events_for(reader, start, {session_id})), types_of(fixture_events("attachment")), "attachment")
    status, _, page, _ = h.client.get(f"/api/session/{session_id}/message", {"limit": 50})
    check(page["data"][-1].get("files") == fixture("session.messages.attachment")["response"]["body"]["data"][-1]["files"], "stored file == fixture")
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/prompt", {"text": "x", "files": [{"uri": f"data:text/plain;base64,{PIXEL}", "name": "pixel.txt"}]})
    check(status == 200 and body["data"]["payload"]["files"][0]["mime"] == "image/png", "declared data-URL MIME is ignored (sniffed from bytes)")
    wait_terminal(reader, session_id, start, count=2)
    for label, uri in (("bad base64", "data:image/png;base64,@@@"), ("unpadded base64", "data:image/png;base64," + PIXEL.rstrip("=")), ("file:// path", "file:///etc/hosts")):
        status, _, body, _ = h.client.post(f"/api/session/{session_id}/prompt", {"text": "x", "files": [{"uri": uri}]})
        check(status == 400 and body.get("field") == "files", f"{label} -> 400 field=files")
    big = base64.b64encode(b"\0" * (20 * 1024 * 1024 + 1)).decode()
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/prompt", {"text": "x", "files": [{"uri": "data:application/octet-stream;base64," + big}]}, timeout=60)
    check(status == 400 and body.get("field") == "files", "20 MiB + 1 attachment -> 400 field=files")

    section("prompt ids")
    session_id = new_session(h, "Prompt ids")
    prompt_id = "msg_0d58b846f00123EB3tzqlo6tYt"
    status, _, first_value, _ = h.client.post(f"/api/session/{session_id}/prompt", {"id": prompt_id, "text": "Say hello. [[scenario:text]]"})
    check_shape(first_value, fixture_bodies("session.prompt"), "prompt response")
    status, _, again, _ = h.client.post(f"/api/session/{session_id}/prompt", {"id": prompt_id, "text": "Say hello. [[scenario:text]]"})
    check(status == 200 and again == first_value, "same id re-post returns the accepted prompt")
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/prompt", {"id": prompt_id, "text": "different"})
    check(status == 409, "same id, different prompt -> 409")
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/prompt", {"id": "prm_1", "text": "x"})
    check(status == 400 and body.get("_tag") == "InvalidRequestError", "id without msg_ -> 400")
    wait_terminal(reader, session_id, 0)
    status, _, page, _ = h.client.get(f"/api/session/{session_id}/message", {"limit": 50})
    check(sum(1 for e in page["data"] if e["type"] == "user") == 1, "idempotent prompt created one user entry")
    check(any(e["id"] == prompt_id for e in page["data"]), "user entry id == prompt id")

    section("permission")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    run_prompt(h, session_id, "Run a shell command. [[scenario:permission]]", reader)
    asked, _ = reader.wait(lambda e: e["type"] == "permission.asked" and e["data"]["sessionID"] == session_id, 10, start)
    check(asked is not None, "permission.asked emitted")
    request_id = asked["data"]["id"]
    status, _, active, _ = h.client.get("/api/session/active")
    check(active["data"].get(session_id) == {"type": "running"}, "session active while waiting for permission")
    check(all(value == {"type": "running"} for value in active["data"].values()), "active entries are {type: running} like the fixture")
    status, _, listed, _ = h.client.get("/api/permission/request", {"location[directory]": WORKSPACE})
    check(any(p["id"] == request_id for p in listed["data"]), "location list has the request")
    check_shape(listed, fixture_bodies("permission.request.list*"), "permission request list")
    status, _, listed, _ = h.client.get("/api/permission/request", {"directory": WORKSPACE})
    check(listed["location"] == {"directory": CWD} and not any(p["id"] == request_id for p in listed["data"]), "plain directory= ignored for permission list")
    status, _, listed, _ = h.client.get(f"/api/session/{session_id}/permission")
    check_shape(listed, fixture_bodies("session.permission.list"), "session permission list")
    status, _, got, _ = h.client.get(f"/api/session/{session_id}/permission/{request_id}")
    check_shape(got, fixture_bodies("session.permission.get"), "session permission get")
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/permission/per_unknown/reply", {"decision": "once"})
    check(status == 404 and body.get("_tag") == "PermissionNotFoundError" and body.get("requestID") == "per_unknown", "unknown request -> 404 PermissionNotFoundError")
    status, _, body, _ = h.client.post(f"/api/session/{SES_OTHER}/permission/{request_id}/reply", {"decision": "once"})
    check(status == 404, "reply under the wrong session -> 404")
    status, _, body, _ = h.client.post(f"/api/session/{session_id}/permission/{request_id}/reply", {"decision": "maybe"})
    check(status == 400, "invalid decision -> 400")
    status, _, _, raw = h.client.post(f"/api/session/{session_id}/permission/{request_id}/reply", {"decision": "once"})
    check(status == 204 and raw == b"", "reply -> 204")
    wait_terminal(reader, session_id, start)
    sequence_equal(types_of(events_for(reader, start, {session_id})), types_of(fixture_events("permission")), "permission")
    status, _, page, _ = h.client.get(f"/api/session/{session_id}/message", {"limit": 50})
    check_shape(page, fixture_bodies("session.messages.*"), "permission history")

    section("child permission")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    run_prompt(h, session_id, "Delegate a shell command. [[scenario:child-permission]]", reader)
    asked, _ = reader.wait(lambda e: e["type"] == "permission.asked", 10, start)
    child_id = asked["data"]["sessionID"] if asked else None
    check(child_id and child_id != session_id, "permission.asked is on the child session")
    status, _, roots, _ = h.client.get("/api/session", {"parentID": "null", "limit": 100})
    check(child_id not in {s["id"] for s in roots["data"]}, "child is not in the root list")
    status, _, listed, _ = h.client.get("/api/permission/request", {"location[directory]": WORKSPACE})
    check(any(p["sessionID"] == child_id for p in listed["data"]), "location list includes the child request")
    status, _, _, _ = h.client.post(f"/api/session/{child_id}/permission/{asked['data']['id']}/reply", {"decision": "once"})
    check(status == 204, "child reply -> 204")
    wait_terminal(reader, session_id, start)
    status, _, got, _ = h.client.get(f"/api/session/{child_id}")
    check_shape(got, fixture_bodies("session.get.child"), "child session get")
    sequence_equal(types_of(events_for(reader, start, {session_id, child_id})), types_of(fixture_events("subagent-permission")), "child-permission")

    section("subagent")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    run_prompt(h, session_id, "Delegate this. [[scenario:subagent]]", reader)
    check(wait_terminal(reader, session_id, start, count=2), "subagent: parent ran twice (turn + synthetic completion)")
    created = [e for e in reader.snapshot()[start:] if e["type"] == "session.created" and e["data"].get("parentID") == session_id]
    child_id = created[0]["data"]["sessionID"] if created else None
    sequence_equal(types_of(events_for(reader, start, {session_id, child_id})), types_of(fixture_events("subagent")), "subagent")
    synthetic = [e for e in reader.snapshot()[start:] if e["type"] == "session.inbox.enqueued" and e["data"]["item"]["type"] == "synthetic"]
    check(len(synthetic) == 1, "synthetic completion arrives as inbox.enqueued item.type synthetic")
    status, _, page, _ = h.client.get(f"/api/session/{session_id}/message", {"limit": 50})
    check([e["type"] for e in page["data"]] == [e["type"] for e in fixture("session.messages.subagent")["response"]["body"]["data"]], "subagent history entry types match fixture")
    check_shape(page, fixture_bodies("session.messages.*"), "subagent history")

    section("interrupt")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    run_prompt(h, session_id, "Stream slowly. [[scenario:slow]]", reader)
    _, delta_index = reader.wait(lambda e: e["type"] == "session.text.delta" and e["data"]["sessionID"] == session_id, 10, start)
    status, _, busy, _ = h.client.post(f"/api/session/{session_id}/prompt", {"text": "Follow-up sent while busy. [[scenario:text]]"})
    check_shape(busy, fixture_bodies("session.prompt.whileBusy"), "prompt while busy")
    reader.wait(lambda e: e["type"] == "session.text.delta" and e["data"]["sessionID"] == session_id, 10, delta_index + 2)
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/interrupt")
    check((status, value) == (200, {"interrupted": True}), "interrupt while running -> {interrupted:true}")
    check(wait_terminal(reader, session_id, start), "execution.interrupted emitted")
    status, _, inbox, _ = h.client.get(f"/api/session/{session_id}/inbox")
    check([i["id"] for i in inbox["data"]] == [busy["data"]["id"]], "steered follow-up survives the interrupt in the inbox")
    check_shape(inbox, fixture_bodies("session.inbox.list*"), "inbox list")
    status, _, _, raw = h.client.request("DELETE", f"/api/session/{session_id}/inbox/{busy['data']['id']}")
    check(status == 204, "inbox cancel -> 204")
    reader.wait(lambda e: e["type"] == "session.inbox.cancelled" and e["data"]["sessionID"] == session_id, 3, start)
    sequence_equal(types_of(events_for(reader, start, {session_id})), types_of(fixture_events("interrupt")), "interrupt", collapse_runs=True)
    status, _, page, _ = h.client.get(f"/api/session/{session_id}/message", {"limit": 50})
    check_shape(page, fixture_bodies("session.messages.*"), "interrupted history")
    check(page["data"][0] == {**page["data"][0], "type": "idle", "outcome": "interrupted"}, "idle outcome interrupted")
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/interrupt")
    check((status, value) == (200, fixture("session.interrupt.idle")["response"]["body"]), "interrupt idle == fixture")

    test_steer_queue(h, reader)

    section("history (several turns)")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    for turn in range(1, 5):
        index = len(reader.snapshot())
        run_prompt(h, session_id, f"History turn {turn}. [[scenario:text]]", reader)
        wait_terminal(reader, session_id, index)
    sequence_equal(types_of(events_for(reader, start, {session_id})), types_of(fixture_events("history")), "history")


def delivered_order(reader, session_id, after):
    return [
        e["data"]["inboxID"]
        for e in reader.snapshot()[after:]
        if e["type"] == "session.inbox.delivered" and e["data"]["sessionID"] == session_id
    ]


def delivered_turns(reader, session_id, after):
    """Inbox IDs delivered since `after`, one list per turn (delivered together before a step)."""
    turns, current = [], []
    for e in reader.snapshot()[after:]:
        if e["data"].get("sessionID") != session_id:
            continue
        if e["type"] == "session.inbox.delivered":
            current.append(e["data"]["inboxID"])
        elif e["type"] == "session.step.started" and current:
            turns.append(current)
            current = []
    if current:
        turns.append(current)
    return turns


def count_type(reader, session_id, after, event_type):
    return sum(1 for e in reader.snapshot()[after:] if e["type"] == event_type and e["data"].get("sessionID") == session_id)


def inbox_state(h, session_id):
    _, _, inbox, _ = h.client.get(f"/api/session/{session_id}/inbox")
    return [(item["id"], item["delivery"]) for item in inbox["data"]]


def settle(reader, seconds=0.8):
    """Waits until no event arrived for `seconds` (bounded)."""
    deadline = time.monotonic() + 15
    count = -1
    while time.monotonic() < deadline:
        now = len(reader.snapshot())
        if now == count:
            return
        count = now
        time.sleep(seconds)


def busy_session(h, reader):
    session_id = new_session(h)
    start = created_index(reader, session_id)
    run_prompt(h, session_id, "Stream slowly. [[scenario:slow]]", reader)
    reader.wait(lambda e: e["type"] == "session.text.delta" and e["data"]["sessionID"] == session_id, 10, start)
    return session_id, start


def post(h, session_id, text, delivery=None):
    body = {"text": text}
    if delivery:
        body["delivery"] = delivery
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/prompt", body)
    check(status == 200 and value["data"]["delivery"] == (delivery or "steer"), f"prompt {text!r} delivery={delivery} -> 200")
    return value["data"]["id"]


def test_steer_queue(h, reader):
    section("steer / queue (2.0.8 semantics)")
    session_id, start = busy_session(h, reader)
    queued = post(h, session_id, "Queue me. [[scenario:text]]", "queue")
    steered = post(h, session_id, "Steer me. [[scenario:text]]")
    check(wait_terminal(reader, session_id, start), "the run ends")
    settle(reader)
    order = delivered_order(reader, session_id, start)
    check(order[1:] == [steered, queued], f"steers are delivered before queued items ({order})")

    section("stop parks both kinds; PATCH / DELETE / resume")
    session_id, start = busy_session(h, reader)
    steer = post(h, session_id, "Parked steer. [[scenario:text]]")
    queue1 = post(h, session_id, "Parked queue 1. [[scenario:text]]", "queue")
    queue2 = post(h, session_id, "Parked queue 2. [[scenario:text]]", "queue")
    mark = len(reader.snapshot())
    status, _, raw_body, raw = h.client.request("PATCH", f"/api/session/{session_id}/inbox/{queue2}", body={"delivery": "steer"})
    check(status == 204 and raw == b"", "PATCH queue -> steer -> 204")
    event, _ = reader.wait(lambda e: e["type"] == "session.inbox.delivery.changed" and e["data"]["inboxID"] == queue2, 3, mark)
    check(
        event is not None and event["data"] == {"sessionID": session_id, "inboxID": queue2, "delivery": "steer"} and "location" not in event,
        "delivery.changed {sessionID, inboxID, delivery}, no location",
    )
    status, _, body, _ = h.client.request("PATCH", f"/api/session/{session_id}/inbox/{queue2}", body={"delivery": "steer"})
    check(status == 409 and body.get("_tag") == "ConflictError", "PATCH to the mode it already has -> 409 ConflictError")
    status, _, body, _ = h.client.request("PATCH", f"/api/session/{session_id}/inbox/msg_gone000000000000000000", body={"delivery": "queue"})
    check(status == 409, "PATCH an unknown item -> 409")
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/interrupt")
    check(value == {"interrupted": True}, "Stop (interrupt without resume)")
    check(wait_terminal(reader, session_id, start), "execution.interrupted")
    settle(reader)
    parked_at = len(reader.snapshot())
    time.sleep(0.6)
    check(
        not any(e["data"].get("sessionID") == session_id for e in reader.snapshot()[parked_at:]),
        "nothing runs after an interrupt without resume",
    )
    check(inbox_state(h, session_id) == [(steer, "steer"), (queue1, "queue"), (queue2, "steer")], "every waiting item stays parked")
    mark = len(reader.snapshot())
    status, _, _, raw = h.client.request("DELETE", f"/api/session/{session_id}/inbox/msg_gone000000000000000000")
    check(status == 204 and raw == b"", "DELETE an item that is not waiting -> 204")
    time.sleep(0.3)
    check(not any(e["type"] == "session.inbox.cancelled" for e in reader.snapshot()[mark:]), "... without inbox.cancelled")
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/interrupt", query="resume=true")
    check(status == 200 and value == {"interrupted": False}, "idle interrupt?resume=true -> {interrupted:false}")
    check(wait_terminal(reader, session_id, mark), "resume wakes the session for its steers")
    settle(reader)
    check(delivered_order(reader, session_id, mark) == [steer, queue2], "resume delivers the parked steers together")
    check(inbox_state(h, session_id) == [(queue1, "queue")], "queued items stay parked after resume")
    mark = len(reader.snapshot())
    status, _, value, _ = h.client.post(f"/api/session/{session_id}/interrupt", query="resume=true")
    time.sleep(0.6)
    check(not any(e["type"] == "session.execution.started" for e in reader.snapshot()[mark:]), "resume with only queued items parked wakes nothing")
    h.client.request("PATCH", f"/api/session/{session_id}/inbox/{queue1}", body={"delivery": "steer"})
    check(wait_terminal(reader, session_id, mark), "PATCH steer on a parked item wakes the session")
    settle(reader)
    check(delivered_order(reader, session_id, mark) == [queue1] and inbox_state(h, session_id) == [], "the switched item is delivered")

    # The client's Resume never sends interrupt?resume=true (above: queued items stay parked).
    section("Resume with a parked steer: bounce it; every parked item runs")
    session_id, start = busy_session(h, reader)
    steer1 = post(h, session_id, "Parked steer 1. [[scenario:text]]")
    queue1 = post(h, session_id, "Parked queue 1. [[scenario:text]]", "queue")
    steer2 = post(h, session_id, "Parked steer 2. [[scenario:text]]")
    queue2 = post(h, session_id, "Parked queue 2. [[scenario:text]]", "queue")
    h.client.post(f"/api/session/{session_id}/interrupt")
    check(wait_terminal(reader, session_id, start), "stopped")
    settle(reader)
    mark = len(reader.snapshot())
    status, _, _, _ = h.client.request("PATCH", f"/api/session/{session_id}/inbox/{steer1}", body={"delivery": "queue"})
    check(status == 204, "bounce 1/2: PATCH the parked steer -> queue -> 204")
    time.sleep(0.5)
    check(not any(e["type"] == "session.execution.started" for e in reader.snapshot()[mark:]), "a switch to queue wakes nothing")
    check(
        inbox_state(h, session_id) == [(steer1, "queue"), (queue1, "queue"), (steer2, "steer"), (queue2, "queue")],
        "the bounced item keeps its place",
    )
    status, _, _, _ = h.client.request("PATCH", f"/api/session/{session_id}/inbox/{steer1}", body={"delivery": "steer"})
    check(status == 204, "bounce 2/2: PATCH it back -> steer -> 204")
    check(wait_terminal(reader, session_id, mark), "the session wakes and finishes")
    settle(reader)
    turns = delivered_turns(reader, session_id, mark)
    check(turns == [[steer1, steer2], [queue1], [queue2]], f"steers together, then each queued item as its own turn ({turns})")
    check(inbox_state(h, session_id) == [], "nothing stays parked")
    check(count_type(reader, session_id, mark, "session.execution.started") == 1, "one execution")

    section("Resume with only queued items: steer the first; the rest follow")
    session_id, start = busy_session(h, reader)
    queued = [post(h, session_id, f"Parked queue {n}. [[scenario:text]]", "queue") for n in (1, 2, 3)]
    h.client.post(f"/api/session/{session_id}/interrupt")
    check(wait_terminal(reader, session_id, start), "stopped")
    settle(reader)
    mark = len(reader.snapshot())
    status, _, _, _ = h.client.request("PATCH", f"/api/session/{session_id}/inbox/{queued[0]}", body={"delivery": "steer"})
    check(status == 204, "PATCH the first queued item -> steer -> 204")
    check(wait_terminal(reader, session_id, mark), "the session wakes and finishes")
    settle(reader)
    turns = delivered_turns(reader, session_id, mark)
    check(turns == [[item] for item in queued], f"each queued item runs as its own turn, in order ({turns})")
    check(inbox_state(h, session_id) == [], "nothing stays parked")

    section("a new prompt delivers the parked items")
    session_id, start = busy_session(h, reader)
    queue = post(h, session_id, "Parked queue. [[scenario:text]]", "queue")
    steer = post(h, session_id, "Parked steer. [[scenario:text]]")
    h.client.post(f"/api/session/{session_id}/interrupt")
    check(wait_terminal(reader, session_id, start), "stopped")
    settle(reader)
    mark = len(reader.snapshot())
    new = post(h, session_id, "New prompt. [[scenario:text]]")
    check(wait_terminal(reader, session_id, mark), "the new prompt wakes the session")
    settle(reader)
    check(delivered_order(reader, session_id, mark) == [steer, new, queue], "parked steers go with the new one, then the queue")
    status, _, _, _ = h.client.request("DELETE", f"/api/session/{session_id}/inbox/{new}")
    check(status == 204, "DELETE an already-delivered prompt -> 204")


def test_forms(h, reader):
    section("forms")
    session_id = new_session(h)
    start = created_index(reader, session_id)
    create_body = fixture("session.form.create")["request"]["body"]
    status, _, created, _ = h.client.post(f"/api/session/{session_id}/form", create_body)
    check_shape(created, fixture_bodies("session.form.create"), "form create")
    form_id = created["data"]["id"]
    status, _, listed, _ = h.client.get("/api/form", {"location[directory]": WORKSPACE})
    check_shape(listed, fixture_bodies("form.list*"), "form list (location)")
    check(any(f["id"] == form_id for f in listed["data"]), "location form list has the form")
    status, _, listed, _ = h.client.get("/api/form", {"directory": WORKSPACE})
    check(listed == fixture("form.list.directoryQuery")["response"]["body"], "plain directory= form list == fixture (cwd, empty)")
    status, _, listed, _ = h.client.get(f"/api/session/{session_id}/form")
    check_shape(listed, fixture_bodies("session.form.list*"), "session form list")
    status, _, got, _ = h.client.get(f"/api/session/{session_id}/form/{form_id}")
    check_shape(got, fixture_bodies("session.form.get"), "form get")
    status, _, _, raw = h.client.request("DELETE", f"/api/session/{session_id}/form/{form_id}")
    check(status == 204 and raw == b"", "form cancel -> 204")
    status, _, body, _ = h.client.request("DELETE", f"/api/session/{session_id}/form/{form_id}")
    check(status == 409 and body.get("_tag") == "FormAlreadySettledError" and body.get("id") == form_id, "second cancel -> 409 FormAlreadySettledError")
    check_shape(body, fixture_bodies("session.form.cancel.again"), "409 body")
    status, _, listed, _ = h.client.get(f"/api/session/{session_id}/form")
    check(listed == fixture("session.form.list.afterCancel")["response"]["body"], "list after cancel == fixture")
    reader.wait(lambda e: e["type"] == "form.cancelled" and e["data"]["id"] == form_id, 3, start)
    ours = [e for e in reader.snapshot()[start:] if e["type"].startswith("form.")]
    sequence_equal(types_of(ours), types_of(fixture_events("form")), "form")
    status, _, body, _ = h.client.request("DELETE", f"/api/session/{session_id}/form/frm_unknown")
    check(status == 404 and body.get("_tag") == "FormNotFoundError", "unknown form -> 404 FormNotFoundError")

    section("form scenario (+ global owner)")
    index = len(reader.snapshot())
    run_prompt(h, session_id, "Need input. [[scenario:form]]", reader)
    wait_terminal(reader, session_id, index)
    reader.wait(lambda e: e["type"] == "form.created" and e["data"]["form"]["sessionID"] == "global", 3, index)
    created = [e for e in reader.snapshot()[index:] if e["type"] == "form.created"]
    owners = sorted(e["data"]["form"]["sessionID"] for e in created)
    check(owners == sorted([session_id, "global"]), f"form scenario creates session + global forms ({owners})")
    global_form = next(e["data"]["form"]["id"] for e in created if e["data"]["form"]["sessionID"] == "global")
    check(all(e.get("location") == {"directory": WORKSPACE} for e in created), "form.created carries location")
    status, _, listed, _ = h.client.get("/api/form", {"location[directory]": WORKSPACE})
    check(global_form in {f["id"] for f in listed["data"]}, "global form listed by location")
    status, _, listed, _ = h.client.get("/api/session/global/form", {"location[directory]": WORKSPACE})
    check([f["id"] for f in listed["data"]] == [global_form], "global owner form list")
    status, _, _, _ = h.client.request("DELETE", f"/api/session/global/form/{global_form}")
    check(status == 404, "global cancel without location -> 404 (cwd location)")
    status, _, _, _ = h.client.request("DELETE", f"/api/session/global/form/{global_form}", {"location[directory]": WORKSPACE})
    check(status == 204, "global cancel with location[directory] -> 204")
    session_form = next(e["data"]["form"]["id"] for e in created if e["data"]["form"]["sessionID"] == session_id)
    status, _, _, _ = h.client.post(f"/api/session/{session_id}/form/{session_form}/reply", {"answer": {"name": "Ada"}})
    check(status == 204, "form reply -> 204 (logged so flows can assert it never happens)")


def test_sse_framing(h):
    section("SSE framing")
    reader = h.reader()
    expected = {}
    for line in (FIXTURES / "events" / "text.raw.txt").read_text().splitlines():
        if line.startswith("# ") and ":" in line and not line.startswith("# HTTP") and not line.startswith("# First"):
            name, _, value = line[2:].partition(":")
            expected[name.strip()] = value.strip()
    check(all(reader.headers.get(name) == value for name, value in expected.items()), f"SSE headers match fixture {expected}")
    time.sleep(1.3)
    frames = list(reader.frames)
    check(frames[0].startswith("data: ") and json.loads(frames[0][6:])["type"] == "server.connected", "first frame is server.connected")
    first = json.loads(frames[0][6:])
    check(set(first) == {"id", "type", "data"}, "server.connected has no created")
    check(frames[1] == ": heartbeat\n\n", "heartbeat comment right after connected (like the capture)")
    check(frames.count(": heartbeat\n\n") >= 2, "periodic heartbeats")
    h.client.request("PATCH", f"/api/session/{SES_OTHER}", body={"title": "Second session"})
    event, _ = reader.wait(lambda e: e["type"] == "session.renamed", 3)
    frames = list(reader.frames)
    data_frames = [f for f in frames if f.startswith("data: ")]
    check(all(re.fullmatch(r"data: \{.*\}\n\n", f, re.S) and "\n" not in f[:-2] for f in data_frames), "every data frame is one line + blank line")
    check(all(json.dumps(json.loads(f[6:]), separators=(",", ":")) == f[6:-2] for f in data_frames), "data frames are compact JSON")
    reader.close()
    return event


def test_event_schemas(h, reader):
    section("event envelope/data shapes")
    fixture_by_type = {}
    for path in sorted((FIXTURES / "events").glob("*.jsonl")):
        for line in path.read_text().splitlines():
            if line:
                event = json.loads(line)
                fixture_by_type.setdefault(event["type"], []).append(event)
    ours_by_type = {}
    for event in reader.snapshot():
        ours_by_type.setdefault(event["type"], []).append(event)
    envelope_keys = {"id", "created", "metadata", "type", "location", "data", "durable"}
    for event_type, events in sorted(ours_by_type.items()):
        samples = fixture_by_type.get(event_type)
        if samples is None:
            bad = [e for e in events if not set(e) <= envelope_keys]
            check(not bad, f"{event_type} (not in fixtures) has a valid envelope")
            continue
        schema = infer(samples)
        if event_type == "session.created":
            # v2-2.0.8 captures created sessions without `model`; OpenCode v2 includes `model`
            # on `session.created` whenever `POST /api/session` pins one.
            schema["keys"]["data"]["keys"].setdefault(
                "model",
                infer([{"id": "m", "providerID": "p"}, {"id": "m", "providerID": "p", "variant": "high"}]),
            )
        errors = []
        for event in events:
            validate(event, schema, event_type, errors)
        check(not errors, f"{event_type} x{len(events)} matches fixture shape" + (f": {errors[:4]}" if errors else ""))
    missing = sorted(set(fixture_by_type) - set(ours_by_type))
    check(not missing, f"every fixture event type was produced (missing: {missing})")


def test_faults(h):
    section("fault: drop SSE now")
    reader = h.reader()
    h.client.control(action="drop_sse")
    check(reader.closed.wait(3), "stream closed on drop_sse")
    check(not reader.clean_end, "drop is abrupt (no terminating chunk)")

    section("fault: drop SSE after N events")
    reader = h.reader()
    h.client.control(action="drop_sse", after=3)
    for index in range(5):
        h.client.request("PATCH", f"/api/session/{SES_OTHER}", body={"title": f"Drop {index}"})
    check(reader.closed.wait(3), "stream closed after N events")
    check(len([e for e in reader.snapshot() if e["type"] != "server.connected"]) == 3, "exactly N events delivered before the drop")
    h.client.control(action="drop_sse", after=2)
    reader = h.reader()
    for index in range(4):
        h.client.request("PATCH", f"/api/session/{SES_OTHER}", body={"title": f"Next {index}"})
    check(reader.closed.wait(3) and len(reader.snapshot()) == 3, "drop-after armed with no stream applies to the next stream")
    h.client.request("PATCH", f"/api/session/{SES_OTHER}", body={"title": "Second session"})

    section("fault: delay bootstrap")
    h.client.control(action="delay", routes=["server.info", "session.list"], ms=400)
    began = time.monotonic()
    h.client.get("/api/info")
    check(time.monotonic() - began >= 0.38, "info delayed")
    began = time.monotonic()
    h.client.get("/api/session", {"parentID": "null"})
    check(time.monotonic() - began >= 0.38, "session list delayed")
    h.client.control(action="delay", routes=["server.info", "session.list"], ms=0)
    began = time.monotonic()
    h.client.get("/api/info")
    check(time.monotonic() - began < 0.3, "delay cleared")

    section("fault: snapshot/event race")
    reader = h.reader()
    h.client.control(action="race", route="session.list", kind="rename")
    status, _, page, _ = h.client.get("/api/session", {"parentID": "null", "limit": 100})
    done = time.monotonic()
    with reader.lock:
        renamed = [(t, e) for t, e in reader.events if e["type"] == "session.renamed" and e["data"]["title"] == "Raced title"]
    check(renamed and renamed[0][0] < done, "session.renamed arrived before the list response completed")
    stale = next(item for item in page["data"] if item["id"] == SES_OTHER)
    check(stale["title"] != "Raced title", "the in-flight snapshot is stale")
    status, _, page, _ = h.client.get("/api/session", {"parentID": "null", "limit": 100})
    check(next(item for item in page["data"] if item["id"] == SES_OTHER)["title"] == "Raced title", "next snapshot has the new title")
    h.client.control(action="race", route="message.list", kind="append", sessionID=SES_OTHER)
    start = len(reader.snapshot())
    status, _, page, _ = h.client.get(f"/api/session/{SES_OTHER}/message", {"limit": 50})
    raced = [e for e in reader.snapshot()[start:] if e["type"] == "session.text.ended"]
    check(raced and raced[0]["data"]["assistantMessageID"] not in {e["id"] for e in page["data"]}, "message race: event before response, not in snapshot")
    status, _, page, _ = h.client.get(f"/api/session/{SES_OTHER}/message", {"limit": 50})
    check(raced and raced[0]["data"]["assistantMessageID"] in {e["id"] for e in page["data"]}, "message race: next page has it")
    h.client.control(action="race", route="session.list", kind="create")
    start = len(reader.snapshot())
    status, _, page, _ = h.client.get("/api/session", {"parentID": "null", "limit": 100})
    created = [e for e in reader.snapshot()[start:] if e["type"] == "session.created"]
    check(created and created[0]["data"]["sessionID"] not in {s["id"] for s in page["data"]}, "create race: session.created before a snapshot lacking it")
    reader.close()

    section("fault: 404 on reply routes")
    h.client.control(action="reply_404", enabled=True)
    status, _, body, _ = h.client.post(f"/api/session/{SES_MAIN}/permission/{BOOT_PERMISSION}/reply", {"decision": "once"})
    check(status == 404 and body.get("_tag") == "PermissionNotFoundError", "permission reply -> 404 while enabled")
    form_id = h.client.control(action="create_form", sessionID=SES_MAIN)["id"]
    status, _, body, _ = h.client.request("DELETE", f"/api/session/{SES_MAIN}/form/{form_id}")
    check(status == 404 and body.get("_tag") == "FormNotFoundError", "form cancel -> 404 while enabled")
    h.client.control(action="reply_404", enabled=False)
    status, _, _, _ = h.client.request("DELETE", f"/api/session/{SES_MAIN}/form/{form_id}")
    check(status == 204, "form cancel works after disabling")

    section("fault: models empty once")
    reader = h.reader()
    h.client.control(action="models_empty", count=1)
    status, _, models, _ = h.client.get("/api/model", {"location[directory]": WORKSPACE})
    check(models["data"] == [], "catalog empty once")
    event, _ = reader.wait(lambda e: e["type"] == "model.updated", 3)
    check(event is not None and event.get("location") == {"directory": WORKSPACE}, "model.updated follows the empty catalog")
    status, _, models, _ = h.client.get("/api/model", {"location[directory]": WORKSPACE})
    check(len(models["data"]) == 2, "catalog populated afterwards")
    reader.close()

    section("fault: server restart")
    status, _, info, _ = h.client.get("/api/info")
    pid = info["pid"]
    status, _, page, _ = h.client.get("/api/session", {"limit": 500})
    count = len(page["data"])
    reader = h.reader()
    h.client.control(action="ask_permission", sessionID=SES_OTHER)
    h.client.control(action="restart", down_ms=800)
    check(reader.closed.wait(3), "SSE dropped by restart")
    time.sleep(0.2)
    refused = False
    try:
        socket.create_connection(urlsplit(h.base)[1:2] and (urlsplit(h.base).hostname, urlsplit(h.base).port), timeout=1).close()
    except OSError:
        refused = True
    check(refused, "connections refused while down")
    deadline = time.monotonic() + 5
    info = None
    while time.monotonic() < deadline:
        try:
            status, _, info, _ = h.client.get("/api/info", timeout=1)
            if status == 200:
                break
        except OSError:
            time.sleep(0.1)
    check(info is not None and info["pid"] == pid + 1, "server back with a new pid")
    status, _, page, _ = h.client.get("/api/session", {"limit": 500})
    check(len(page["data"]) == count, "sessions persist across restart")
    status, _, listed, _ = h.client.get(f"/api/session/{SES_OTHER}/permission")
    check(listed["data"] == [], "in-memory pending permissions are gone after restart")
    reader = h.reader()
    check(reader.snapshot() and reader.snapshot()[0]["type"] == "server.connected", "new stream starts with server.connected")
    reader.close()


def test_boot_permission(h):
    section("boot permission")
    status, _, listed, _ = h.client.get("/api/permission/request", {"location[directory]": WORKSPACE})
    boot = [p for p in listed["data"] if p["id"] == BOOT_PERMISSION]
    check(boot and "save" not in boot[0], "--boot-permission seeds a request without save")
    status, _, _, _ = h.client.post(f"/api/session/{SES_MAIN}/permission/{BOOT_PERMISSION}/reply", {"decision": "reject"})
    check(status == 204, "boot permission reply -> 204")


def test_shells(h, reader):
    section("shell commands and execution events (background jobs)")
    location = {"location[directory]": WORKSPACE}
    status, _, listed, _ = h.client.get("/api/shell", location)
    check(status == 200 and listed == {"location": {"directory": WORKSPACE}, "data": []}, "shell list is empty by default")
    after = len(reader.snapshot())
    shell_id = h.client.control(action="create_shell", directory=WORKSPACE, command="sleep 30", sessionID=SES_MAIN, startedAgoMs=5000)["id"]
    created, _ = reader.wait(lambda e: e["type"] == "shell.created" and e["data"]["info"]["id"] == shell_id, 5, after)
    check(created is not None and created.get("location") == {"directory": WORKSPACE}, "shell.created carries the location")
    check(
        created is not None
        and created["data"]["info"]["metadata"] == {"sessionID": SES_MAIN}
        and created["data"]["info"]["status"] == "running"
        and created["data"]["info"]["time"]["started"] <= created["created"] - 4000,
        "shell.created info: running, metadata.sessionID, time.started",
    )
    check(created is not None and "durable" not in created, "shell.created is not durable")
    status, _, listed, _ = h.client.get("/api/shell", location)
    check(status == 200 and [item["id"] for item in listed["data"]] == [shell_id], "the running command is listed in its location")
    _, _, elsewhere, _ = h.client.get("/api/shell", {"location[directory]": CWD})
    check(elsewhere["data"] == [] and elsewhere["location"] == {"directory": CWD}, "another location lists nothing")
    samples = [event for event in fixture_events("permission") if event["type"] == "shell.created"]
    if created is not None:
        check_shape(created, samples, "shell.created")
    h.client.control(action="exit_shell", id=shell_id)
    exited, _ = reader.wait(lambda e: e["type"] == "shell.exited" and e["data"]["id"] == shell_id, 5, after)
    check(exited is not None and exited["data"] == {"id": shell_id, "status": "exited", "exit": 0}, "shell.exited {id, status, exit}")
    _, _, listed, _ = h.client.get("/api/shell", location)
    check(listed["data"] == [], "an exited command is no longer listed")
    status, _, created_value, _ = h.client.post(
        "/api/shell?" + urlencode(location), {"command": "sleep 60", "metadata": {"sessionID": SES_OTHER}}
    )
    check(status == 200 and created_value["data"]["metadata"] == {"sessionID": SES_OTHER}, "POST /api/shell creates a command")
    second = created_value["data"]["id"] if status == 200 else ""
    status, _, _, _ = h.client.request("DELETE", f"/api/shell/{second}", location)
    deleted, _ = reader.wait(lambda e: e["type"] == "shell.deleted" and e["data"] == {"id": second}, 5, after)
    check(status == 204 and deleted is not None, "DELETE /api/shell/{id} -> 204 + shell.deleted")
    h.client.control(action="set_running", sessionID=SES_CHILD, running=True)
    started, _ = reader.wait(lambda e: e["type"] == "session.execution.started" and e["data"]["sessionID"] == SES_CHILD, 5, after)
    check(started is not None and "location" not in started, "set_running emits session.execution.started (no location)")
    _, _, active, _ = h.client.get("/api/session/active")
    check(active["data"].get(SES_CHILD) == {"type": "running"}, "a running child session is active")
    h.client.control(action="set_running", sessionID=SES_CHILD, running=False)
    ended, _ = reader.wait(lambda e: e["type"] == "session.execution.succeeded" and e["data"]["sessionID"] == SES_CHILD, 5, after)
    _, _, active, _ = h.client.get("/api/session/active")
    check(ended is not None and SES_CHILD not in active["data"], "set_running false ends it")


def test_background_seed():
    section("--background-jobs seed")
    h = Harness(["--background-jobs"])
    try:
        _, _, active, _ = h.client.get("/api/session/active")
        check(active["data"] == {SES_BG_CHILD: {"type": "running"}}, "the seeded child session is running")
        _, _, child, _ = h.client.get(f"/api/session/{SES_BG_CHILD}")
        check(child["data"].get("parentID") == SES_MAIN and child["data"]["title"] == "Audit v1 call sites", "child info")
        _, _, workspace, _ = h.client.get("/api/shell", {"location[directory]": WORKSPACE})
        check(
            [(item["command"], item["metadata"].get("sessionID")) for item in workspace["data"]]
            == [("pnpm dev --port 5173", SES_BG_CHILD)],
            "the workspace runs the child's dev server",
        )
        _, _, other, _ = h.client.get("/api/shell", {"location[directory]": OTHER_DIR})
        check(
            [(item["command"], item["metadata"].get("sessionID")) for item in other["data"]]
            == [("cargo test --all-targets", SES_BG_OWNER)],
            "the other directory runs the owner's tests",
        )
        _, _, roots, _ = h.client.get("/api/session", {"parentID": "null", "limit": "200"})
        ids = [item["id"] for item in roots["data"]]
        check(SES_BG_OWNER in ids and SES_BG_CHILD not in ids, "the owner is a root session, the child is not")
        summary = h.client.control(action="state")["state"]
        check(len(summary["shells"]) == 2, "the control summary lists running shells")
    finally:
        h.stop()


def test_log(h):
    section("request log")
    records = h.log()
    http = [r for r in records if r["kind"] == "http"]
    check(all({"seq", "t", "method", "path", "route", "query", "status", "auth"} <= set(r) for r in http), "http records carry method/path/route/query/status/auth")
    prompts = [r for r in http if r["route"] == "session.prompt" and r["body"] and r["body"].get("files")]
    check(prompts and prompts[0]["body"]["files"][0].get("bytes") and prompts[0]["body"]["files"][0]["scheme"] == "data", "prompt log summarizes files")
    raw = open(h.log_file).read()
    check(PIXEL not in raw and h.password not in raw, "log holds no attachment bytes or password")
    check(any(r["kind"] == "sse.open" for r in records) and any(r["kind"] == "sse.close" for r in records), "SSE open/close recorded")
    check(any(r["kind"] == "restart" and r["phase"] == "up" for r in records), "restart recorded")
    check(any(r["route"] == "session.permission.reply" and r["body"]["decision"] == "once" for r in http), "reply decision recorded")
    status, _, _, text = h.client.request("GET", "/__log", auth=False)
    check(status == 200 and text.count(b"\n") >= len(records), "GET /__log serves the log")
    status, _, _, _ = h.client.get("/global/event")
    check(any(r.get("path") == "/global/event" and r["route"] is None and r["status"] == 404 for r in h.log()), "unknown (v1) routes are logged with route=null, 404")


def main():
    h = Harness()
    try:
        test_auth(h)
        test_bootstrap(h)
        test_messages(h)
        test_models(h)
        test_boot_permission(h)
        reader = h.reader()
        test_lifecycle(h, reader)
        test_scenarios(h, reader)
        test_forms(h, reader)
        test_shells(h, reader)
        test_event_schemas(h, reader)
        reader.close()
        test_sse_framing(h)
        test_faults(h)
        test_log(h)
    finally:
        h.stop()
    test_background_seed()
    print(f"\n{PASSES[0]} checks passed, {len(FAILURES)} failed")
    if FAILURES:
        for failure in FAILURES:
            print(f"  - {failure}")
        sys.exit(1)


if __name__ == "__main__":
    main()
