"""Helpers for the e2e run: MCP over HTTP (legacy 2025-06-18) and the herdr socket API."""
import json, os, socket, subprocess, time, uuid, http.client

PFH = os.environ["PFH"]
BIN = os.environ["BIN"]
HSOCK = os.environ["HERDR_SOCKET_PATH"]
PROTO = "2025-06-18"


def intercom_status():
    out = subprocess.run([BIN, "--home", PFH, "intercom", "status"], capture_output=True, text=True, timeout=20).stdout
    d = {}
    for line in out.splitlines():
        k, _, v = line.partition("\t")
        d[k] = v
    return d


def mcp_endpoint():
    st = intercom_status()
    url = st["mcp_url"]
    token = open(st["mcp_token_file"]).read().strip()
    hostport = url.split("//", 1)[1].split("/", 1)[0]
    host, port = hostport.rsplit(":", 1)
    return host, int(port), token


_ID = [0]


def mcp_raw(method, params=None, token=True, timeout=60, notification=False):
    host, port, tok = mcp_endpoint()
    body = {"jsonrpc": "2.0", "method": method, "params": params or {}}
    if not notification:
        _ID[0] += 1
        body["id"] = _ID[0]
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
               "MCP-Protocol-Version": PROTO}
    if token:
        headers["Authorization"] = "Bearer " + (tok if token is True else token)
    c = http.client.HTTPConnection(host, port, timeout=timeout)
    c.request("POST", "/mcp", json.dumps(body), headers)
    r = c.getresponse()
    data = r.read().decode()
    c.close()
    parsed = None
    txt = data.strip()
    if txt.startswith("event:") or txt.startswith("data:"):
        for line in txt.splitlines():
            if line.startswith("data:"):
                txt = line[5:].strip()
    try:
        parsed = json.loads(txt) if txt else None
    except Exception:
        parsed = None
    return r.status, parsed, data


def initialize():
    return mcp_raw("initialize", {"protocolVersion": PROTO, "clientInfo": {"name": "e2e", "version": "1"}, "capabilities": {}})


def tool(name, args, timeout=90):
    """Returns (is_error, payload) where payload is the parsed JSON text content if possible."""
    t0 = time.time()
    status, parsed, raw = mcp_raw("tools/call", {"name": name, "arguments": args}, timeout=timeout)
    dt = time.time() - t0
    if parsed is None:
        return True, {"http_status": status, "raw": raw}, dt
    if "error" in parsed:
        return True, {"jsonrpc_error": parsed["error"]}, dt
    res = parsed.get("result", {})
    texts = [c.get("text", "") for c in res.get("content", []) if c.get("type") == "text"]
    text = "\n".join(texts)
    try:
        payload = json.loads(text)
    except Exception:
        payload = text
    if res.get("structuredContent") is not None and not isinstance(payload, dict):
        payload = res["structuredContent"]
    return bool(res.get("isError")), payload, dt


def herdr(method, params=None, timeout=10):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(HSOCK)
    rid = str(uuid.uuid4())
    s.sendall((json.dumps({"id": rid, "method": method, "params": params or {}}) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(1 << 20)
        if not chunk:
            break
        buf += chunk
    s.close()
    v = json.loads(buf)
    if v.get("error"):
        raise RuntimeError("herdr %s: %s" % (method, v["error"]))
    return v["result"]


def panes():
    return herdr("pane.list")["panes"]


def pane(pid):
    for p in panes():
        if p.get("pane_id") == pid:
            return p
    return None


def proc_info(pid):
    # herdr 0.9.5 takes `pane_id` here; `target` is ignored (answers for the focused pane).
    return herdr("pane.process_info", {"pane_id": pid})["process_info"]


def ps(pid):
    if not pid:
        return "(no pid)"
    r = subprocess.run(["ps", "-o", "pid,ppid,stat,etime,args", "-p", str(pid)], capture_output=True, text=True)
    return r.stdout.strip()


def alive(pid):
    if not pid:
        return False
    try:
        with open("/proc/%d/stat" % pid) as f:
            st = f.read().rsplit(")", 1)[1].split()[0]
        return st != "Z"
    except FileNotFoundError:
        return False
