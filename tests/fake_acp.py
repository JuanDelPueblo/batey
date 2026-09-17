"""Deterministic ACP peer for transport/lifecycle tests; no model or network calls."""
import json
import os
import pathlib
import sys
import time
import uuid
import socket
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

root = pathlib.Path(sys.argv[1])
mode = sys.argv[2] if len(sys.argv) >= 3 else "load"

# Terminal authentication runs this same program with the arguments the agent
# advertised, so the interactive branch lives here rather than in a second
# script. The backend appends the advertised arguments after the base ones.
if len(sys.argv) > 3 and sys.argv[3] == "terminal-auth":
    import signal

    out = pathlib.Path(sys.argv[4])
    out.mkdir(parents=True, exist_ok=True)
    (out / "invocation.json").write_text(json.dumps({
        "argv": sys.argv,
        "cwd": os.getcwd(),
        "env": dict(os.environ),
        "isatty": sys.stdin.isatty(),
    }))

    def report_size(*_):
        size = os.get_terminal_size(sys.stdin.fileno())
        print(f"size:{size.columns}x{size.lines}", flush=True)

    signal.signal(signal.SIGWINCH, report_size)
    # A descendant that only dies with the whole process tree.
    import subprocess
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(600)"])
    (out / "child.pid").write_text(str(child.pid))
    print("ready", flush=True)
    while True:
        line = sys.stdin.readline()
        if not line:
            sys.exit(7)
        command = line.strip()
        if command == "ok":
            # The login stored credentials. Later initializes read this file
            # and advertise the post-login state, so a test can prove that a
            # read after a success is fresh.
            (out / "authenticated").write_text("1")
            # A last line just before the exit, so a test can prove the
            # client still receives it.
            print("login-complete", flush=True)
            sys.exit(0)
        if command == "fail":
            sys.exit(3)
        if command == "size":
            report_size()
        if command == "flood":
            # More output than the retained scrollback, to prove the bound.
            for index in range(4000):
                print(f"flood-{index:06d}-" + "x" * 64, flush=True)
        print(f"echo:{command}", flush=True)

can_load = mode != "no-load"
rich_capabilities = mode not in ("no-rich", "null-rich", "object-rich")
reject_config = mode == "reject-config"
slow_startup = mode == "slow-startup" or mode == "auth-slow"
# Snapshot the received process environment beside the session file, so tests
# can prove exactly which variables one agent process observed.
dump_env = mode == "dump-env"
# Deterministic transient failure: succeeds at the transport level but omits
# the authoritative `configOptions`, so the backend must treat it as a retryable
# connection/start failure rather than a saved-config rejection.
transient_config = mode == "transient-config"
# Authentication modes. `auth` advertises one agent method, one terminal
# method, and one method type this client cannot know. `auth-no-logout` drops
# the logout capability. `auth-required` answers session/new with the stable
# `auth_required` error code. `auth-legacy-opencode` and
# `auth-legacy-copilot` advertise the deployed `_meta["terminal-auth"]`
# bridge instead of a stable terminal type. `auth-codex-url` runs an
# `agent` method that emits a URL elicitation while `authenticate` runs.
# `auth-antigravity` runs an `agent` method that waits for an elicitation
# answer, so a client without a cancellable flow would hang. The browser
# variants exercise the compatibility URL capture and loopback relay.
auth_mode = mode.startswith("auth")
supports_logout = mode in ("auth", "auth-required")
requires_auth = mode == "auth-required"
current = None
pending_prompt = None
model = "small"
current_mode = "ask"


def send(obj):
    print(json.dumps({"jsonrpc": "2.0", **obj}), flush=True)


def reply(id, result):
    send({"id": id, "result": result})


def options():
    return [{"id": "model", "name": "Model", "type": "select", "currentValue": model,
             "options": [{"value": "small", "name": "Small"}, {"value": "large", "name": "Large"}]},
            {"id": "web_search", "name": "Web search", "type": "boolean", "currentValue": False,
             "description": "Let the agent read pages from the web."}]


def modes():
    return {"currentModeId": current_mode,
            "availableModes": [{"id": "ask", "name": "Ask"}, {"id": "act", "name": "Act"}]}


def update(update_kind, **fields):
    send({"method": "session/update", "params": {"sessionId": current, "update": {"sessionUpdate": update_kind, **fields}}})


def auth_methods():
    if mode == "auth-legacy-opencode":
        return [
            {"id": "opencode-login", "name": "Log in with OpenCode",
             "description": "Run `opencode auth login` in the terminal",
             "_meta": {"terminal-auth": {"command": str(root / "opencode-stub"),
                                        "args": ["auth", "login"],
                                        "label": "OpenCode Login"}}},
        ]
    if mode == "auth-legacy-opencode-relative":
        # A Registry-installed OpenCode advertises a bare command name. Batey
        # may resolve it inside that same agent's validated install directory.
        return [
            {"id": "opencode-login", "name": "Log in with OpenCode",
             "description": "Run `opencode auth login` in the terminal",
             "_meta": {"terminal-auth": {"command": "opencode",
                                        "args": ["auth", "login"],
                                        "label": "OpenCode Login"}}},
        ]
    if mode == "auth-legacy-copilot":
        return [
            {"id": "copilot-login", "name": "Log in with Copilot CLI",
             "description": "Run `copilot login` in the terminal",
             "_meta": {"terminal-auth": {"command": str(root / "copilot-stub"),
                                        "args": ["login"],
                                        "label": "Copilot Login"}}},
        ]
    if mode == "auth-codex-url":
        return [
            {"id": "codex-oauth", "name": "Sign in with Codex",
             "description": "Device-code flow through a URL step"},
        ]
    if mode == "auth-antigravity":
        return [
            {"id": "antigravity-interactive", "name": "Interactive sign-in",
             "description": "Complete the interactive step"},
        ]
    if mode in ("auth-antigravity-browser", "auth-antigravity-stderr", "auth-antigravity-deny", "auth-antigravity-malicious"):
        return [{"id": "oauth-personal", "name": "Sign in with Google",
                 "description": "Browser sign-in through the ACP agent"}]
    methods = [
        {"id": "api-key", "name": "API key", "description": "Paste an API key"},
        {"id": "api-key-broken", "name": "Broken API key", "type": "agent"},
        {"id": "tui", "name": "Terminal login", "type": "terminal",
         "args": ["terminal-auth", str(root)],
         "env": {"BATEY_TEST_METHOD_ENV": "from-method", "BATEY_TEST_SHARED_ENV": "from-method"}},
        {"id": "future", "name": "Future scheme", "type": "browser-popup"},
    ]
    if (root / "authenticated").exists():
        # The terminal login stored credentials, so the agent stops
        # advertising the terminal method. A test proves that a read after a
        # terminal success observes this change and not a stale snapshot.
        return [method for method in methods if method["id"] != "tui"]
    return methods


def record(name, payload):
    """Appends one observed request, so a test can prove what was sent."""
    path = root / name
    seen = json.loads(path.read_text()) if path.exists() else []
    seen.append(payload)
    path.write_text(json.dumps(seen))


def capture_browser_url(url):
    """Emulate an upstream BROWSER launch without opening a real browser."""
    address = os.environ.get("BATEY_AUTH_BROWSER_CAPTURE_ADDR")
    token = os.environ.get("BATEY_AUTH_BROWSER_CAPTURE_TOKEN")
    if not address or not token:
        return False
    host, port = address.rsplit(":", 1)
    with socket.create_connection((host, int(port)), timeout=2) as stream:
        stream.sendall((token + "\n" + url).encode())
    return True


def loopback_auth_url(error=False, host="127.0.0.1"):
    state = "fake-oauth-state"
    class CallbackHandler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.server.callback_path = self.path
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"callback received")
        def log_message(self, *_args):
            pass
    server = HTTPServer((host, 0), CallbackHandler)
    server.oauth_error = error
    redirect = f"http://{host}:{server.server_port}/oauth/callback"
    url = "https://accounts.example.test/o/oauth2/auth?" + urllib.parse.urlencode({
        "client_id": "fake",
        "redirect_uri": redirect,
        "state": state,
        "response_type": "code",
    })
    return server, url, state


for line in sys.stdin:
    msg = json.loads(line)
    method, p, id = msg.get("method"), msg.get("params", {}), msg.get("id")
    if method == "initialize":
        if slow_startup:
            time.sleep(1.0)
        agent_capabilities = {"loadSession": can_load,
                              "sessionCapabilities": {"list": {}, "close": {}, "delete": {}}}
        # Stable ACP v1 prompt capabilities are top-level boolean fields;
        # sessionCapabilities is a separate lifecycle surface.
        if rich_capabilities:
            agent_capabilities["promptCapabilities"] = {
                "image": True, "audio": True, "embeddedContext": True}
        elif mode == "null-rich":
            agent_capabilities["promptCapabilities"] = None
        elif mode == "object-rich":
            # Deliberately malformed for typed-deserialization regression
            # coverage: objects must not be mistaken for true booleans.
            agent_capabilities["promptCapabilities"] = {
                "image": {}, "audio": {}, "embeddedContext": {}}
        if supports_logout:
            agent_capabilities["auth"] = {"logout": {}}
        if auth_mode:
            record("initialize.json", p.get("clientCapabilities", {}))
            # Authentication helpers print credentials to stderr. This marker
            # stands in for such a line, so a test can prove what the
            # authentication path logs and what an ordinary chat agent logs.
            print(f"BATEY_TEST_STDERR_SECRET cwd={os.getcwd()}", file=sys.stderr, flush=True)
            # The environment this authentication process received, so a test
            # can prove per-agent secret isolation on the probe path too.
            (root / "probe-env.json").write_text(json.dumps(dict(os.environ)))
        reply(id, {"protocolVersion": 1, "agentCapabilities": agent_capabilities,
                   "agentInfo": {"name": "fake-acp", "version": "1.0.0"},
                   "authMethods": auth_methods() if auth_mode else []})
    elif method == "authenticate":
        record("authenticate.json", p)
        if p.get("methodId") == "api-key-broken":
            send({"id": id, "error": {"code": -32603, "message": "Key rejected"}})
        elif mode == "auth-codex-url" and p.get("methodId") == "codex-oauth":
            # Request-scoped URL elicitation during `authenticate`. The client
            # must surface the URL, never pre-fetch it, and answer
            # accept/decline/cancel. The fake waits for that answer.
            elic_req_id = 9001
            send({"id": elic_req_id, "method": "elicitation/create", "params": {
                "mode": "url",
                "message": "Open the device page and enter the code.",
                "url": "https://example.invalid/device?code=ABCD-1234",
                "elicitationId": "device-1",
                "requestId": id}})
            action = None
            while True:
                line2 = sys.stdin.readline()
                if not line2:
                    sys.exit(0)
                try:
                    msg2 = json.loads(line2)
                except Exception:
                    continue
                if msg2.get("id") == elic_req_id and ("result" in msg2 or "error" in msg2):
                    result = msg2.get("result", {})
                    action = result.get("action", "cancel")
                    if isinstance(action, dict):
                        action = action.get("action", "cancel")
                    break
                # A protocol cancel for the authenticate request aborts the flow.
                if msg2.get("method") == "$/cancel_request":
                    send({"id": id, "error": {"code": -32800, "message": "Request cancelled"}})
                    action = "cancelled"
                    break
            if action == "accept":
                send({"method": "elicitation/complete", "params": {"elicitationId": "device-1"}})
                reply(id, {})
            elif action == "cancelled":
                pass
            elif action == "decline":
                send({"id": id, "error": {"code": -32603, "message": "User declined"}})
            else:
                send({"id": id, "error": {"code": -32800, "message": "Request cancelled"}})
        elif mode == "auth-antigravity" and p.get("methodId") == "antigravity-interactive":
            elic_req_id = 9002
            send({"id": elic_req_id, "method": "elicitation/create", "params": {
                "mode": "form",
                "message": "Complete the interactive sign-in step.",
                "requestedSchema": {"type": "object", "properties": {}, "required": []},
                "requestId": id}})
            action = None
            while True:
                line2 = sys.stdin.readline()
                if not line2:
                    sys.exit(0)
                try:
                    msg2 = json.loads(line2)
                except Exception:
                    continue
                if msg2.get("id") == elic_req_id and ("result" in msg2 or "error" in msg2):
                    result = msg2.get("result", {})
                    action = result.get("action", "cancel")
                    if isinstance(action, dict):
                        action = action.get("action", "cancel")
                    break
                if msg2.get("method") == "$/cancel_request":
                    send({"id": id, "error": {"code": -32800, "message": "Request cancelled"}})
                    action = "cancelled"
                    break
            if action == "accept":
                reply(id, {})
            elif action == "cancelled":
                pass
            elif action == "decline":
                send({"id": id, "error": {"code": -32603, "message": "User declined"}})
            else:
                send({"id": id, "error": {"code": -32800, "message": "Request cancelled"}})
        elif mode in ("auth-antigravity-browser", "auth-antigravity-stderr", "auth-antigravity-deny") and p.get("methodId") == "oauth-personal":
            server, auth_url, state = loopback_auth_url(error=mode == "auth-antigravity-deny")
            if mode == "auth-antigravity-browser":
                if not capture_browser_url(auth_url):
                    print(auth_url, file=sys.stderr, flush=True)
            else:
                print("Open the following link to authenticate the ACP server: " + auth_url, file=sys.stderr, flush=True)
            server.handle_request()
            if server.oauth_error:
                send({"id": id, "error": {"code": -32000, "message": "OAuth access denied"}})
            else:
                reply(id, {})
            server.server_close()
        elif mode == "auth-antigravity-malicious" and p.get("methodId") == "oauth-personal":
            malicious = "https://accounts.example.test/o/oauth2/auth?redirect_uri=http%3A%2F%2Fevil.example.test%3A1%2Fadmin&state=fake-oauth-state"
            print("Open the following link to authenticate the ACP server: " + malicious, file=sys.stderr, flush=True)
            time.sleep(600)
        else:
            reply(id, {})
    elif method == "logout":
        record("logout.json", p)
        reply(id, {})
    elif method == "session/new" and requires_auth:
        record("session-new.json", p)
        send({"id": id, "error": {"code": -32000, "message": "Log in first, friend"}})
    elif method == "session/new":
        if slow_startup:
            time.sleep(1.0)
        current = str(uuid.uuid4())
        (root / current).write_text("0")
        reply(id, {"sessionId": current, "configOptions": options(), "modes": modes()})
        if dump_env:
            (root / f"{current}.env.json").write_text(json.dumps(dict(os.environ)))
    elif method == "session/load":
        if slow_startup:
            time.sleep(1.0)
        current = p["sessionId"]
        if not (root / current).exists():
            send({"id": id, "error": {"code": -32001, "message": "Missing history"}})
        else:
            # Replay dynamic snapshots as live agents do during load. The
            # client must retain them in memory without duplicating durable
            # history: message chunks stay suppressed and config snapshots
            # must not add historical config events.
            update("available_commands_update", availableCommands=[
                {"name": "plan", "description": "Make a plan", "input": {"hint": "goal"}},
                {"name": "review", "description": "Review changes"}])
            update("current_mode_update", currentModeId=current_mode)
            update("usage_update", used=100, size=2000, cost={"amount": 0.5, "currency": "USD"})
            update("config_option_update", configOptions=options())
            update("agent_message_chunk", content={"type": "text", "text": "REPLAY"})
            reply(id, {"configOptions": options(), "modes": modes()})
    elif method == "session/set_config_option":
        if reject_config:
            send({"id": id, "error": {"code": -32002, "message": "Rejected saved option"}})
        elif transient_config:
            # No `configOptions`: the backend reports a transient reapply failure.
            reply(id, {"unexpected": True})
        else:
            # Boolean options carry `type: boolean`; select options are bare ids.
            if isinstance(p.get("value"), str):
                model = p["value"]
            reply(id, {"configOptions": options()})
    elif method == "session/list":
        reply(id, {"sessions": [{"sessionId": f.name, "cwd": str(root)} for f in root.iterdir() if f.is_file()]})
    elif method == "session/close":
        reply(id, {})
    elif method == "session/delete":
        target = p.get("sessionId")
        try:
            (root / target).unlink(missing_ok=True)
        except Exception:
            pass
        reply(id, {})
    elif method == "session/set_mode":
        current_mode = p.get("modeId", current_mode)
        reply(id, {})
    elif method == "$/cancel_request":
        # Protocol-level cancellation is advisory; ignore per spec.
        pass
    elif method == "session/cancel":
        if pending_prompt is not None:
            reply(pending_prompt, {"stopReason": "cancelled"})
            pending_prompt = None
    elif method == "session/prompt":
        text = "\n".join(block.get("text", "") for block in p["prompt"] if block.get("type") == "text")
        count = int((root / current).read_text()) + 1
        (root / current).write_text(str(count))
        if text == "wait":
            pending_prompt = id
        elif text == "rpc-error":
            send({"id": id, "error": {"code": -32003, "message": "Prompt failed"}})
        elif text == "permission":
            pending_prompt = id
            send({"id": "permission-1", "method": "session/request_permission", "params": {
                "sessionId": current, "toolCall": {"toolCallId": "tool-1", "title": "Write file", "kind": "edit"},
                "options": [{"optionId": "yes", "name": "Approve once", "kind": "allow_once"},
                            {"optionId": "always", "name": "Always approve", "kind": "allow_always"},
                            {"optionId": "no", "name": "Reject once", "kind": "reject_once"},
                            {"optionId": "never", "name": "Always reject", "kind": "reject_always"}]}})
        elif text.startswith("title:"):
            new_title = text[6:]
            update("session_info_update", title=new_title)
            update("agent_message_chunk", content={"type": "text", "text": "title-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "title-empty":
            update("session_info_update", title="")
            update("agent_message_chunk", content={"type": "text", "text": "empty-title-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "title-oversized":
            update("session_info_update", title="x" * 500)
            update("agent_message_chunk", content={"type": "text", "text": "oversized-title-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "commands":
            update("available_commands_update", availableCommands=[
                {"name": "plan", "description": "Make a plan", "input": {"hint": "goal"}},
                {"name": "review", "description": "Review changes"}])
            update("agent_message_chunk", content={"type": "text", "text": "commands-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "modes":
            update("current_mode_update", currentModeId="act")
            update("agent_message_chunk", content={"type": "text", "text": "mode-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "usage":
            update("usage_update", used=100, size=2000, cost={"amount": 0.5, "currency": "USD"})
            update("agent_message_chunk", content={"type": "text", "text": "usage-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "message-id":
            update("agent_message_chunk", content={"type": "text", "text": "part-1 "}, messageId="m1")
            update("agent_message_chunk", content={"type": "text", "text": "part-2"}, messageId="m1")
            update("agent_message_chunk", content={"type": "text", "text": "next"}, messageId="m2")
            reply(id, {"stopReason": "end_turn"})
        elif text == "rich-output":
            update("agent_message_chunk", content={"type": "text", "text": "before"}, messageId="rich-1")
            update("agent_message_chunk", content={"type": "resource_link", "name": "Batey", "uri": "https://example.test/batey"}, messageId="rich-1")
            update("agent_thought_chunk", content={"type": "resource", "resource": {"uri": "attachment://note.txt", "mimeType": "text/plain", "text": "private note"}}, messageId="thought-1")
            reply(id, {"stopReason": "end_turn"})
        elif text == "user-chunk":
            # Agent-reflected user chunk must not duplicate local history.
            update("user_message_chunk", content={"type": "text", "text": text})
            update("agent_message_chunk", content={"type": "text", "text": "user-chunk-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text.startswith("identity:"):
            # Echo the client-generated user message identity from `_meta`
            # so the round trip can be correlated. Other prompts omit the
            # echo entirely, exercising agents that ignore the extension.
            observed = p.get("_meta", {}).get("batey", {}).get("userMessageId")
            update("agent_message_chunk", content={"type": "text", "text": f"identity:{observed}"})
            if observed is None:
                reply(id, {"stopReason": "end_turn"})
            else:
                reply(id, {"stopReason": "end_turn",
                           "_meta": {"batey": {"userMessageId": observed}}})
        elif text == "tool-loc":
            update("tool_call", toolCallId="t1", title="Edit", kind="edit",
                   locations=[{"path": "/tmp/a.rs", "line": 3}])
            update("agent_message_chunk", content={"type": "text", "text": "loc-sent"})
            reply(id, {"stopReason": "end_turn"})
        elif text == "tool-rich":
            update("tool_call", toolCallId="rich-tool", title="Inspect", kind="read",
                   content=[
                       {"type": "content", "content": {"type": "text", "text": "summary"}},
                       {"type": "content", "content": {"type": "resource_link", "name": "Batey", "uri": "https://example.test/tool"}},
                       {"type": "content", "content": {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"}},
                   ])
            reply(id, {"stopReason": "end_turn"})
        else:
            update("agent_message_chunk", content={"type": "text", "text": f"{current}:{count}:{model}"})
            reply(id, {"stopReason": "end_turn"})
    elif id == "permission-1" and pending_prompt is not None:
        choice = msg.get("result", {}).get("outcome", {}).get("optionId", "cancelled")
        update("agent_message_chunk", content={"type": "text", "text": choice})
        reply(pending_prompt, {"stopReason": "end_turn"})
        pending_prompt = None
