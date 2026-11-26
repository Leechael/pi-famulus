#!/usr/bin/env python3
"""E2E scenarios for start_session / session_status / stop_session against real herdr + real pi.
Run through run.sh (which sets up and tears down). Prints PASS / FAIL / OBSERVED lines; exits 1 on any FAIL.
Optional args: scenario keys to run (the `steps` list in main(); see README.md)."""
import hashlib, json, os, socket, subprocess, sys, threading, time
import lib

MODEL = os.environ["PF_E2E_MODEL"]   # provider/model, set by env.sh
SCR = os.environ["RUNSCR"]           # fresh scratch dir for this run
RUN = os.environ["RUN"]
BIN, PFH = lib.BIN, lib.PFH
T0 = time.time()
PIDS = set()                          # every pi pid seen, checked at teardown
RESULTS = []
ANCHOR = None                         # idle shell pane, focused by default (stands in for the user's pane)
BYSTANDERS = {}                       # pane_id -> description, panes that must never change


def ts():
    return "%7.2f" % (time.time() - T0)


def log(*a):
    print(ts(), *a, flush=True)


def ev(label, obj):
    s = obj if isinstance(obj, str) else json.dumps(obj, sort_keys=True)
    log("   evidence %s: %s" % (label, s))


def verdict(kind, sc, expected, actual):
    RESULTS.append((kind, sc, expected, actual))
    log("%s [%s] expected: %s | actual: %s" % (kind, sc, expected, actual))


def check(sc, cond, expected, actual):
    verdict("PASS" if cond else "FAIL", sc, expected, actual)


def add_pid(pid):
    if pid:
        PIDS.add(int(pid))
        with open(os.path.join(RUN, "pids.txt"), "a") as f:
            f.write("%s\n" % pid)


def note_pid(payload):
    if isinstance(payload, dict) and payload.get("pid"):
        add_pid(payload["pid"])


def tool(name, args, timeout=120):
    err, payload, dt = lib.tool(name, args, timeout=timeout)
    note_pid(payload)
    return err, payload, dt


def status(handle):
    return tool("session_status", {"session": handle})[1]


def st_of(x):
    return x.get("state") if isinstance(x, dict) else x


def mkcwd(name):
    d = os.path.join(SCR, name)
    os.makedirs(d, exist_ok=True)
    return d


def focused():
    return [p["pane_id"] for p in lib.panes() if p.get("focused")]


def pinfo(pane_id):
    try:
        return lib.proc_info(pane_id)
    except Exception as e:
        return {"error": str(e)}


def fg_names(pane_id):
    return [x["name"] for x in pinfo(pane_id).get("foreground_processes", [])]


def pane_brief(p):
    if p is None:
        return None
    return {k: p.get(k) for k in ("pane_id", "focused", "agent", "agent_status", "agent_session", "revision")}


def snapshot(exclude=()):
    """Every pane but `exclude`: revision, agent, foreground group and pids (to prove nothing else changed)."""
    out = {}
    for p in lib.panes():
        if p["pane_id"] in exclude:
            continue
        i = pinfo(p["pane_id"])
        out[p["pane_id"]] = (p.get("revision"), i.get("shell_pid"), i.get("foreground_process_group_id"),
                             tuple(sorted(x["pid"] for x in i.get("foreground_processes", []))))
    return out


def snap_diff(a, b):
    keys = set(a) | set(b)
    return {k: (a.get(k), b.get(k)) for k in sorted(keys) if a.get(k) != b.get(k)}


def snapshot_nofocus(exclude=()):
    # revision also moves on focus changes, so compare the rest when focus was changed on purpose
    return {k: v[1:] for k, v in snapshot(exclude).items()}


def start(name, cwd, model=MODEL):
    return tool("start_session", {"cwd": cwd, "name": name, "model": model})


def wait_until(fn, timeout, interval=0.1):
    end = time.time() + timeout
    while time.time() < end:
        v = fn()
        if v:
            return v
        time.sleep(interval)
    return fn()


def read_pane(pane_id, lines=12):
    try:
        r = lib.herdr("pane.read", {"pane_id": pane_id, "source": "recent", "format": "text", "strip_ansi": True, "lines": lines})
        txt = r.get("read", {}).get("text", r)
        return "\n".join(l.rstrip() for l in str(txt).splitlines() if l.strip())
    except Exception as e:
        return "read error: %s" % e


def screen_hash(pane_id):
    try:
        r = lib.herdr("pane.read", {"pane_id": pane_id, "source": "visible", "format": "text", "strip_ansi": True})
        return hashlib.sha1(str(r.get("read", {}).get("text", "")).encode()).hexdigest()[:8]
    except Exception:
        return None


def session_entries(pane_or_path):
    path = pane_or_path
    if not path.startswith("/"):
        path = ((lib.pane(pane_or_path) or {}).get("agent_session") or {}).get("value", "")
    try:
        return path, [json.loads(l) for l in open(path) if l.strip()]
    except OSError:
        return path, []


STOPS = []   # (scenario, closed, wall_s, lag_s, lag_kind)


def stop_closed(err, res):
    """A stop of a live session succeeded: exited, its pane closed."""
    return (not err) and isinstance(res, dict) and res.get("pane") == "closed" and res.get("state") == "exited"


def record_stop(sc, err, res, dt, marks):
    closed = stop_closed(err, res)
    lag, kind = None, None
    if "pid_dead" in marks:
        if "agent_cleared" in marks:
            lag, kind = round(marks["agent_cleared"] - marks["pid_dead"], 3), "cleared"
        elif "pane_gone" in marks:
            lag, kind = round(marks["pane_gone"] - marks["pid_dead"], 3), "pane_closed_first(upper bound)"
    STOPS.append((sc, closed, round(dt, 2), lag, kind))
    log("   stop-record %s closed=%s wall=%.2fs herdr_agent_lag_after_pid_death=%s (%s)" % (sc, closed, dt, lag, kind))


def watch_stop(handle, pane_id, pid, interval=0.03, on_pid_dead=None):
    """stop_session(handle) while sampling the pane (presence, agent, status, screen), its foreground group and the pid.
    Returns (err, payload, dt, transitions, marks) where marks holds first times of observable events."""
    res = {}
    th = threading.Thread(target=lambda: res.update(r=tool("stop_session", {"session": handle}, timeout=120)))
    t = time.time()
    th.start()
    trans, last, marks = [], None, {}
    h0 = screen_hash(pane_id)
    while th.is_alive():
        p = lib.pane(pane_id)
        i = pinfo(pane_id) if p else {}
        h = screen_hash(pane_id) if p else None
        now = time.time() - t
        if p and h != h0 and "screen_changed" not in marks:
            marks["screen_changed"] = now
        if not lib.alive(pid) and "pid_dead" not in marks:
            marks["pid_dead"] = now
            if on_pid_dead:
                on_pid_dead()
                marks["hook_done"] = time.time() - t
        if p is not None and not p.get("agent") and "pid_dead" in marks and "agent_cleared" not in marks:
            marks["agent_cleared"] = now
        if p is None and "pane_gone" not in marks:
            marks["pane_gone"] = now
        if p and i.get("shell_pid") and i.get("shell_pid") == i.get("foreground_process_group_id") and "shell_has_terminal" not in marks:
            marks["shell_has_terminal"] = now
        cur = (bool(p), p and p.get("agent"), p and p.get("agent_status"), bool(p and p.get("agent_session")),
               lib.alive(pid), [x["name"] for x in i.get("foreground_processes", [])] if p else None,
               i.get("foreground_process_group_id") == i.get("shell_pid") if p else None)
        if cur != last:
            trans.append("%.2fs pane=%s agent=%s status=%s agent_session=%s pid_alive=%s fg=%s shell_has_tty=%s" % ((now,) + cur))
            last = cur
        time.sleep(interval)
    th.join()
    marks["returned"] = time.time() - t
    trans.append("%.2fs (returned) pane=%s pid_alive=%s" % (marks["returned"], lib.pane(pane_id) is not None, lib.alive(pid)))
    err, payload, dt = res["r"]
    return err, payload, dt, trans, {k: round(v, 2) for k, v in marks.items()}


def close_pane_if_ours(pane_id, why):
    """Close a pane this run created (all panes live in our private herdr session)."""
    if lib.pane(pane_id):
        try:
            lib.herdr("pane.close", {"pane_id": pane_id})
            log("   cleanup: closed our pane %s (%s)" % (pane_id, why))
        except Exception as e:
            log("   cleanup: pane.close %s failed: %s" % (pane_id, e))


def focus(pane_id):
    try:
        lib.herdr("pane.focus", {"pane_id": pane_id})
    except Exception as e:
        log("   pane.focus %s failed: %s" % (pane_id, e))


def rows_named(name):
    rows = lib.tool("list_sessions", {"query": name})[1]
    seq = rows.get("sessions", rows) if isinstance(rows, dict) else rows
    return [r for r in (seq or []) if isinstance(r, dict) and r.get("name") == name and str(r.get("id", "")).startswith("pi/")], rows


def prompt(sid, text, pane_id=None):
    r = subprocess.run([BIN, "--home", PFH, "intercom", "session", "prompt", sid, text], capture_output=True, text=True, timeout=60)
    out = "cli: rc=%s out=%s err=%s" % (r.returncode, r.stdout.strip()[:200], r.stderr.strip()[:200])
    if r.returncode != 0 and pane_id:
        lib.herdr("pane.send_text", {"pane_id": pane_id, "text": text})
        time.sleep(0.3)
        lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["enter"]})
        out += " -> typed into the pane via herdr pane.send_text + enter"
    return out


def assistant_texts(entries):
    out = []
    for e in entries:
        m = e.get("message") or {}
        if m.get("role") == "assistant":
            for c in m.get("content") or []:
                if isinstance(c, dict) and c.get("type") == "text" and c.get("text", "").strip():
                    out.append(c["text"].strip())
    return out


def stop_and_verify(sc, sid, name, pane_id, pid, focus_note):
    """A stop of a live started session: must succeed, close only its pane, kill its pid. Then repeat stops (E)."""
    others_before = snapshot(exclude=(pane_id,))
    ev("focused panes at stop (%s)" % focus_note, focused())
    ev("pane before", pane_brief(lib.pane(pane_id)))
    err, res, dt, trans, marks = watch_stop(sid, pane_id, pid)
    for line in trans:
        log("   stop-trace " + line)
    ev("stop_session result", [err, res])
    ev("timing marks (s from call)", marks)
    record_stop(sc, err, res, dt, marks)
    p = lib.pane(pane_id)
    if p:
        ev("pane after", [pane_brief(p), pinfo(pane_id), read_pane(pane_id, 6)])
    others_after = snapshot(exclude=(pane_id,))
    diff = snap_diff(others_before, others_after)
    ev("other panes changed", diff)
    check(sc, not err and isinstance(res, dict) and res.get("state") == "exited" and res.get("pane") == "closed"
          and not lib.alive(pid) and p is None,
          "success: state exited, pane closed, pid dead, a few seconds",
          "err=%s state=%s pane=%s pid_alive=%s pane_exists=%s wall %.2fs" % (
              err, res.get("state") if isinstance(res, dict) else res, res.get("pane") if isinstance(res, dict) else None,
              lib.alive(pid), p is not None, dt))
    check(sc + " [D: only that pane]", not diff, "every other pane's revision and processes unchanged", "changed=%s" % diff)
    # E: repeat stops by every handle, no herdr mutation
    before = snapshot()
    rep = {}
    for h in (sid, pane_id, name):
        e2, r2, d2 = tool("stop_session", {"session": h})
        rep[h] = (e2, r2)
        ev("repeat stop_session(%s)" % h, [e2, r2, "%.2fs" % d2])
    sts = {h: status(h) for h in (sid, pane_id, name)}
    ev("session_status after stop", sts)
    after = snapshot()
    check(sc + " [E: repeat stops]", all(not e and isinstance(r, dict) and r.get("state") == "exited" for e, r in rep.values())
          and all(st_of(v) == "exited" for v in sts.values()) and before == after,
          "repeat stop by id, pane id, name: success, exited, no herdr mutation",
          "%s status=%s herdr changed=%s" % ({h: (e, st_of(r)) for h, (e, r) in rep.items()},
                                              {h: st_of(v) for h, v in sts.items()}, snap_diff(before, after)))
    if p:
        close_pane_if_ours(pane_id, "%s stop left it open" % sc)
    return err, res, dt, marks


# --------------------------------------------------------------------------------------------
def s1():
    sc = "S1 tools/list + auth"
    st, init, _ = lib.initialize()
    ev("initialize", init.get("result", init))
    st, p, raw = lib.mcp_raw("tools/list")
    names = [t["name"] for t in p["result"]["tools"]]
    ev("tools", names)
    for t in p["result"]["tools"]:
        if t["name"] in ("start_session", "session_status", "stop_session"):
            ev(t["name"], {"description": t.get("description"), "inputSchema": t["inputSchema"]})
    check(sc, all(n in names for n in ("start_session", "session_status", "stop_session")),
          "three tools listed", "listed=%s" % [n for n in names if "session" in n])
    n0 = len(lib.panes())
    st1, _, raw1 = lib.mcp_raw("tools/list", token=False)
    st2, _, raw2 = lib.mcp_raw("tools/list", token="wrong-token")
    st3, _, raw3 = lib.mcp_raw("tools/call", {"name": "start_session", "arguments": {"cwd": SCR, "name": "noauth"}}, token=False)
    ev("no token tools/list", "%s %s" % (st1, raw1.strip()))
    ev("wrong token tools/list", "%s %s" % (st2, raw2.strip()))
    ev("no token tools/call start_session", "%s %s" % (st3, raw3.strip()))
    n1 = len(lib.panes())
    check(sc + " auth", st1 == 401 and st2 == 401 and st3 == 401 and n0 == n1,
          "401 without / with a wrong bearer, nothing started", "HTTP %s/%s/%s, panes %d->%d" % (st1, st2, st3, n0, n1))


def s2_3_4():
    """Start a session; poll pane.list and session_status(pane) while start_session runs."""
    cwd = mkcwd("s2")
    before = {p["pane_id"] for p in lib.panes()}
    res = {}
    t = time.time()
    th = threading.Thread(target=lambda: res.update(r=start("s2", cwd)))
    th.start()
    timeline, last, newpane, statuses, last_state = [], {}, None, [], None
    seen = {"agent": None, "agent_session": None, "pane": None, "returned": None}
    while time.time() - t < 20:
        now = time.time() - t
        for p in lib.panes():
            if p["pane_id"] in before:
                continue
            if newpane is None:
                newpane, seen["pane"] = p["pane_id"], now
            cur = (p.get("agent"), json.dumps(p.get("agent_session")), p.get("agent_status"))
            if last.get(p["pane_id"]) != cur:
                timeline.append("%.2fs pane %s agent=%s agent_status=%s agent_session=%s" % (now, p["pane_id"], cur[0], cur[2], cur[1]))
                last[p["pane_id"]] = cur
            if p.get("agent") and seen["agent"] is None:
                seen["agent"] = now
            if p.get("agent_session") and seen["agent_session"] is None:
                seen["agent_session"] = now
        if newpane and th.is_alive():
            e, s, _ = lib.tool("session_status", {"session": newpane})
            state = s.get("state") if isinstance(s, dict) else s
            if state != last_state:
                statuses.append("%.2fs session_status(pane)=%s" % (time.time() - t, json.dumps(s)))
                last_state = state
        if not th.is_alive() and seen["returned"] is None:
            seen["returned"] = time.time() - t
        if seen["returned"] is not None and seen["agent_session"] is not None and time.time() - t > seen["returned"] + 1:
            break
        time.sleep(0.05)
    th.join()
    err, payload, dt = res["r"]

    sc = "S2 start_session"
    ev("start_session result", [err, payload])
    ev("duration_s", "%.2f" % dt)
    if err or not isinstance(payload, dict):
        verdict("FAIL", sc, "session starts", payload)
        return None
    pid, pane_id = payload.get("pid"), payload.get("pane_id")
    pi_ = pinfo(pane_id)
    ev("pane.process_info", pi_)
    ev("ps", lib.ps(pid))
    cwdlink = os.readlink("/proc/%d/cwd" % pid) if pid and lib.alive(pid) else None
    fgpids = [x["pid"] for x in pi_.get("foreground_processes", [])]
    check(sc, payload.get("state") == "alive" and payload.get("id", "").startswith("pi/") and pid in fgpids and cwdlink == cwd,
          "returns id pi/<uuid>, pane_id, pid of the real pi (pane foreground, cwd), state alive",
          "state=%s id=%s pane=%s pid=%s pane_fg=%s cwd_ok=%s in %.2fs" % (payload.get("state"), payload.get("id"), pane_id, pid, fgpids, cwdlink == cwd, dt))

    sc = "S3 herdr agent identification"
    for line in timeline:
        log("   timeline " + line)
    sess = (lib.pane(pane_id) or {}).get("agent_session") or {}
    val = sess.get("value", "")
    uuid_part = os.path.splitext(os.path.basename(val))[0].rsplit("_", 1)[-1]
    native = ["pi/pi-" + uuid_part, "pi/" + uuid_part]
    ev("agent_session", sess)
    ev("session file exists on disk (no turn taken yet)", os.path.exists(val))
    fmt = lambda x: "%.2f" % x if x is not None else None
    verdict("OBSERVED", sc, "when agent / agent_session appear",
            "pane listed %ss, agent at %ss, agent_session at %ss (no turn), start_session returned %ss"
            % (fmt(seen["pane"]), fmt(seen["agent"]), fmt(seen["agent_session"]), fmt(seen["returned"])))
    check(sc, payload.get("id") in native, "intercom id is one of herdr's native ids", "native=%s id=%s" % (native, payload.get("id")))

    sc = "S4 session_status lifetime"
    for line in statuses:
        log("   status " + line)
    by = {h: status(h) for h in (payload.get("id"), pane_id, "s2")}
    for h, v in by.items():
        ev("session_status(%s)" % h, v)
    states = [l.split('"state": "')[1].split('"')[0] for l in statuses if '"state": "' in l]
    verdict("OBSERVED", sc, "during the call (before the record exists): no 'starting' expected",
            "seen by pane id during the call: %s" % ([l.split("=", 1)[1][:80] for l in statuses]))
    check(sc, payload.get("state") == "alive" and all(isinstance(x, dict) and x.get("state") == "alive" and x.get("pid") == pid for x in by.values()),
          "alive straight after start_session; alive with pid by id, pane id and name",
          "start state=%s; %s" % (payload.get("state"), {h: ((x.get("state"), x.get("pid")) if isinstance(x, dict) else x) for h, x in by.items()}))
    return payload


def s5(payload):
    sc = "S5 stop fresh never-prompted session (other pane focused)"
    if not payload:
        verdict("FAIL", sc, "a session from S2", "none")
        return
    focus(ANCHOR)
    stop_and_verify(sc, payload["id"], "s2", payload["pane_id"], payload["pid"], "anchor idle shell focused, a different pane")


def s5c():
    sc = "S5c pi reaction to ctrl+c / ctrl+d (keys sent by this script)"
    err, payload, dt = start("s5c", mkcwd("s5c"))
    if err:
        verdict("FAIL", sc, "session starts", payload)
        return
    pane_id, pid = payload["pane_id"], payload["pid"]
    lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+c"]})
    time.sleep(3)
    a1 = lib.alive(pid)
    t = time.time()
    lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+d"]})
    dead = wait_until(lambda: not lib.alive(pid), 10, 0.02)
    t_dead = time.time() - t
    verdict("OBSERVED", sc, "ctrl+c alone does not exit an idle pi; ctrl+d does",
            "alive 3s after one ctrl+c=%s; after ctrl+d pid dead=%s in %.2fs" % (a1, bool(dead), t_dead))
    e, r, _ = tool("stop_session", {"session": payload["id"]})
    ev("stop_session after self-exit", [e, r])
    close_pane_if_ours(pane_id, "S5c")


def s6():
    sc = "S6 stop after one turn (own pane focused)"
    err, payload, dt = start("s6", mkcwd("s6"))
    if err:
        verdict("FAIL", sc, "session starts", payload)
        return
    sid, pane_id, pid = payload["id"], payload["pane_id"], payload["pid"]
    ev("session prompt", prompt(sid, "Reply with only the word OK. Do not use tools.", pane_id))
    got = wait_until(lambda: assistant_texts(session_entries(pane_id)[1]), 90, 0.5)
    wait_until(lambda: (lib.pane(pane_id) or {}).get("agent_status") in ("idle", "done"), 30, 0.3)
    ev("assistant reply in session file", got)
    if not got:
        verdict("FAIL", sc, "one turn taken", "no assistant reply in 90s")
    focus(pane_id)
    time.sleep(0.3)
    stop_and_verify(sc, sid, "s6", pane_id, pid, "the session's own pane focused")
    focus(ANCHOR)


def sA():
    sc = "A stop during a running turn"
    d = mkcwd("sA")
    err, payload, dt = start("sa", d)
    if err:
        verdict("FAIL", sc, "session starts", payload)
        return
    sid, pane_id, pid = payload["id"], payload["pane_id"], payload["pid"]
    ev("session prompt", prompt(sid, "Use your bash tool, in the foreground (not in the background), to run exactly: python3 -c 'import time; time.sleep(45); print(1)'  then reply DONE.", pane_id))

    def sleepers():
        out = []
        for p in os.listdir("/proc"):
            if p.isdigit():
                try:
                    cmd = open("/proc/%s/cmdline" % p, "rb").read().replace(b"\0", b" ").decode().strip()
                    if cmd.startswith("python3") and "time.sleep(45)" in cmd and os.readlink("/proc/%s/cwd" % p) == d:
                        out.append(int(p))
                except OSError:
                    pass
        return out

    sl = wait_until(sleepers, 60, 0.5)
    p = lib.pane(pane_id) or {}
    ev("before stop: agent_status / sleep pids", [p.get("agent_status"), sl, [lib.ps(x) for x in sl]])
    if not sl:
        verdict("FAIL", sc, "the turn runs sleep 60", "no sleep 60 process within 60s; pane: %s" % read_pane(pane_id, 10))
    time.sleep(1)
    ev("agent_status right before stop", (lib.pane(pane_id) or {}).get("agent_status"))
    err, res, dt, trans, marks = watch_stop(sid, pane_id, pid)
    for line in trans:
        log("   stop-trace " + line)
    ev("stop_session result", [err, res])
    ev("timing marks (s from call)", marks)
    record_stop("A running turn", err, res, dt, marks)
    path, ents = session_entries(((lib.pane(pane_id) or {}).get("agent_session") or {}).get("value", "") or p.get("agent_session", {}).get("value", ""))
    tail = [json.dumps({k: e.get(k) for k in ("type",)} | {"role": (e.get("message") or {}).get("role"),
                                                           "stop": (e.get("message") or {}).get("stopReason"),
                                                           "err": (e.get("message") or {}).get("errorMessage")})
            for e in ents[-6:]]
    ev("session file tail (turn cancelled?)", tail)
    ev("worker process alive after stop", [(x, lib.alive(x)) for x in sl])
    pp = lib.pane(pane_id)
    if pp:
        ev("pane after", [pane_brief(pp), pinfo(pane_id), read_pane(pane_id, 8)])
    verdict("OBSERVED", sc, "ctrl+c cancels the turn, ctrl+d 3s later ends pi",
            "wall %.2fs; marks %s; aborted entry in session file=%s; sleep alive after=%s" % (
                dt, marks, any("abort" in json.dumps(e).lower() for e in ents[-6:]), [lib.alive(x) for x in sl]))
    check(sc, not err and isinstance(res, dict) and res.get("state") == "exited" and not lib.alive(pid) and pp is None,
          "stop succeeds: exited, pane closed, pid dead",
          "err=%s result=%s pid_alive=%s pane_exists=%s" % (err, res, lib.alive(pid), pp is not None))
    for x in sl:
        if lib.alive(x):
            log("   sleep 60 %d still alive after the stop: %s (left; it ends by itself within 60s)" % (x, lib.ps(x)))
    if pp:
        close_pane_if_ours(pane_id, "A")


def s7():
    sc = "S7 process_info of shell panes"
    w = lib.herdr("workspace.create", {"cwd": mkcwd("s7"), "label": "s7shell"})
    pane_id = w["root_pane"]["pane_id"]
    time.sleep(1.5)
    ev("focused", focused())
    idle = pinfo(pane_id)
    idle_tgt = lib.herdr("pane.process_info", {"target": pane_id})["process_info"]
    ev("idle shell, {pane_id}", idle)
    ev("idle shell, {target}", idle_tgt)
    wait_until(lambda: "❯" in read_pane(pane_id, 5), 10, 0.3)
    for attempt in range(3):
        lib.herdr("pane.send_text", {"pane_id": pane_id, "text": "sleep 300"})
        lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["enter"]})
        if wait_until(lambda: "sleep" in fg_names(pane_id), 5, 0.2):
            break
    sl = pinfo(pane_id)
    ev("sleep 300, {pane_id}", sl)
    verdict("OBSERVED", sc, "herdr facts",
            "idle shell fg=%s pgid=%s shell_pid=%s; sleep fg=%s pgid=%s; {target} answered for %s (asked %s)"
            % ([x["name"] for x in idle.get("foreground_processes", [])], idle.get("foreground_process_group_id"), idle.get("shell_pid"),
               [x["name"] for x in sl.get("foreground_processes", [])], sl.get("foreground_process_group_id"), idle_tgt.get("pane_id"), pane_id))
    check(sc + " (rule the daemon uses)", idle.get("shell_pid") == idle.get("foreground_process_group_id")
          and sl.get("shell_pid") != sl.get("foreground_process_group_id"),
          "idle: fg pgid == shell_pid; sleep: fg pgid != shell_pid",
          "idle %s==%s, sleep %s vs %s" % (idle.get("foreground_process_group_id"), idle.get("shell_pid"),
                                           sl.get("foreground_process_group_id"), sl.get("shell_pid")))
    lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["ctrl+c"]})
    time.sleep(0.5)
    close_pane_if_ours(pane_id, "S7")
    focus(ANCHOR)


def sB():
    sc = "B pane with a background job"
    d = mkcwd("sB")
    err, payload, _ = start("sb", d)
    sid, pane_id, pid = payload["id"], payload["pane_id"], payload["pid"]
    lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+d"]})
    wait_until(lambda: not lib.alive(pid) and not (lib.pane(pane_id) or {}).get("agent"), 10)
    lib.herdr("pane.send_text", {"pane_id": pane_id, "text": "sleep 300 &"})
    lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["enter"]})
    time.sleep(1.5)
    info = pinfo(pane_id)
    job = []
    for p in os.listdir("/proc"):
        if p.isdigit():
            try:
                if open("/proc/%s/cmdline" % p, "rb").read() == b"sleep\x00300\x00" and os.readlink("/proc/%s/cwd" % p) == d:
                    job.append(int(p))
            except OSError:
                pass
    ev("process_info, idle shell with a background job", info)
    ev("background job", [lib.ps(x) for x in job])
    verdict("OBSERVED", sc + " (herdr fact)", "what process_info reports for an idle shell with a bg job",
            "fg pgid=%s shell_pid=%s list=%s; job pids %s not listed=%s" % (
                info.get("foreground_process_group_id"), info.get("shell_pid"), [x["name"] for x in info.get("foreground_processes", [])],
                job, not any(x["pid"] in job for x in info.get("foreground_processes", []))))
    before = snapshot()
    rs = {}
    for h in (sid, pane_id, "sb"):
        e, r, dt = tool("stop_session", {"session": h})
        rs[h] = (e, r)
        ev("stop_session(%s)" % h, [e, r, "%.2fs" % dt])
    after = snapshot()
    check(sc, all(not e and isinstance(r, dict) and r.get("already_exited") and r.get("pane") == "left_open" for e, r in rs.values())
          and before == after and job and all(lib.alive(x) for x in job),
          "already exited, pane left open, nothing sent, the job survives",
          "%s herdr changed=%s job alive=%s" % ({h: (e, (r.get("pane") if isinstance(r, dict) else r)) for h, (e, r) in rs.items()},
                                                snap_diff(before, after), [lib.alive(x) for x in job]))
    close_pane_if_ours(pane_id, "B (also ends its job)")
    wait_until(lambda: not any(lib.alive(x) for x in job), 5)
    ev("job alive after we closed the pane", [(x, lib.alive(x)) for x in job])
    for x in job:
        if lib.alive(x):
            log("   job %d survives pane.close: %s" % (x, lib.ps(x)))
            with open("/proc/%d/cmdline" % x, "rb") as f:
                if f.read() == b"sleep\x00300\x00":
                    os.kill(x, 15)
                    log("   sent SIGTERM to our own sleep 300 pid %d" % x)


def s8():
    sc = "S8 pi exits by itself"
    lags, keep = [], None
    for i in range(3):
        err, payload, _ = start("s8-%d" % i, mkcwd("s8-%d" % i))
        if err:
            verdict("FAIL", sc, "session starts", payload)
            return
        pane_id, pid = payload["pane_id"], payload["pid"]
        t = time.time()
        lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+d"]})
        t_dead = t_agent = t_sess = None
        while time.time() - t < 15 and None in (t_dead, t_agent, t_sess):
            now = time.time() - t
            p = lib.pane(pane_id) or {}
            if t_dead is None and not lib.alive(pid):
                t_dead = now
            if t_agent is None and not p.get("agent"):
                t_agent = now
            if t_sess is None and not p.get("agent_session"):
                t_sess = now
            time.sleep(0.02)
        lags.append((t_dead, t_agent, t_sess))
        if i < 2:
            tool("stop_session", {"session": payload["id"]})
            close_pane_if_ours(pane_id, "S8 run %d" % i)
        else:
            keep = payload
    verdict("OBSERVED", sc, "how long herdr reports agent / agent_session after pi exits",
            "(pid_dead, agent_cleared, agent_session_cleared) s after ctrl+d: %s"
            % [tuple(round(x, 3) if x is not None else None for x in l) for l in lags])
    sid, pane_id = keep["id"], keep["pane_id"]
    st = {h: status(h) for h in (sid, pane_id, "s8-2")}
    for h, v in st.items():
        ev("session_status(%s)" % h, v)
    check(sc + " status", all(st_of(v) == "exited" for v in st.values()),
          "exited by id, pane id and name", "%s" % {h: st_of(v) for h, v in st.items()})
    before = snapshot()
    rs = {}
    for h in (sid, pane_id, "s8-2"):
        e, r, dt = tool("stop_session", {"session": h})
        rs[h] = (e, r)
        ev("stop_session(%s)" % h, [e, r, "%.2fs" % dt])
    after = snapshot()
    check(sc + " stop", all(not e and isinstance(r, dict) and r.get("pane") == "left_open" for e, r in rs.values()) and before == after,
          "success by id, pane id and name; pane left open; no herdr mutation",
          "%s herdr changed=%s" % ({h: (e, r.get("pane") if isinstance(r, dict) else r) for h, (e, r) in rs.items()}, snap_diff(before, after)))
    close_pane_if_ours(pane_id, "S8")


def s9():
    sc = "S9 pane taken over"
    err, payload, _ = start("s9a", mkcwd("s9a"))
    pane_id, pid = payload["pane_id"], payload["pid"]
    lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+d"]})
    wait_until(lambda: not lib.alive(pid) and not (lib.pane(pane_id) or {}).get("agent"), 10)
    lib.herdr("pane.send_text", {"pane_id": pane_id, "text": "sleep 300"})
    lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["enter"]})
    time.sleep(1.5)
    sl = [x["pid"] for x in pinfo(pane_id).get("foreground_processes", []) if x["name"] == "sleep"]
    for h in (payload["id"], pane_id, "s9a"):
        e, r, dt = tool("stop_session", {"session": h})
        ev("stop_session(%s)" % h, [e, r, "%.2fs" % dt])
    still = bool(sl) and all(lib.alive(x) for x in sl) and lib.pane(pane_id) is not None
    check(sc + " (a) sleep 300", still, "leaves it alone; sleep keeps running",
          "sleep pids %s alive=%s pane exists=%s" % (sl, [lib.alive(x) for x in sl], lib.pane(pane_id) is not None))
    lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["ctrl+c"]})
    time.sleep(0.5)
    close_pane_if_ours(pane_id, "S9a")
    err, payload, _ = start("s9b", mkcwd("s9b"))
    pane_id, pid = payload["pane_id"], payload["pid"]
    lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+d"]})
    wait_until(lambda: not lib.alive(pid) and not (lib.pane(pane_id) or {}).get("agent"), 10)
    lib.herdr("agent.start", {"name": "intruder", "kind": "pi", "pane_id": pane_id, "args": ["--name", "intruder", "--model", MODEL]})
    p = wait_until(lambda: (lib.pane(pane_id) or {}).get("agent_session") and lib.pane(pane_id), 15)
    other = [x["pid"] for x in pinfo(pane_id).get("foreground_processes", []) if x["name"] == "pi"]
    for x in other:
        add_pid(x)
    for h in (payload["id"], pane_id, "s9b"):
        e, r, dt = tool("stop_session", {"session": h})
        ev("stop_session(%s)" % h, [e, r, "%.2fs" % dt])
    ok = bool(other) and all(lib.alive(x) for x in other) and lib.pane(pane_id) is not None
    check(sc + " (b) different pi started in the pane after ours exited", ok, "leaves it alone; the other pi keeps running",
          "other pi %s alive=%s pane exists=%s" % (other, [lib.alive(x) for x in other], lib.pane(pane_id) is not None))
    rows, raw = rows_named("intruder")
    if rows:
        t = time.time()
        r = subprocess.run([BIN, "--home", PFH, "intercom", "session", "rm", rows[0]["id"]], capture_output=True, text=True, timeout=60)
        ev("session rm intruder", "rc=%s %s %s %.2fs" % (r.returncode, r.stdout.strip(), r.stderr.strip(), time.time() - t))
    wait_until(lambda: not any(lib.alive(x) for x in other), 5)
    ev("intruder alive after rm / pane", [lib.alive(x) for x in other], )
    close_pane_if_ours(pane_id, "S9b")


def s10():
    sc = "S10 kill the session's pid"
    d = mkcwd("s10")
    err, payload, _ = start("s10", d)
    pane_id = payload["pane_id"]
    pid = status(payload["id"])["pid"]
    fg = [x["pid"] for x in pinfo(pane_id).get("foreground_processes", [])]
    cwdl = os.readlink("/proc/%d/cwd" % pid)
    with open("/proc/%d/environ" % pid, "rb") as f:
        mine = ("PI_FAMULUS_HOME=" + PFH).encode() in f.read().split(b"\0")
    log("   about to kill: " + lib.ps(pid).replace("\n", " || "))
    ev("checks", {"pid_is_pane_foreground": pid in fg, "cwd": cwdl, "env_PI_FAMULUS_HOME_is_ours": mine})
    if not (pid in fg and cwdl == d and mine):
        verdict("FAIL", sc, "pid verified before kill", "verification failed; not killing")
        return
    os.kill(pid, 15)
    if not wait_until(lambda: not lib.alive(pid), 5, 0.05):
        log("   SIGTERM ignored for 5s; SIGKILL to the same pid: " + lib.ps(pid))
        os.kill(pid, 9)
        wait_until(lambda: not lib.alive(pid), 5, 0.05)
    s2 = wait_until(lambda: (lambda x: x if st_of(x) == "exited" else None)(status(payload["id"])), 10, 0.2) or status(payload["id"])
    ev("session_status after kill", s2)
    check(sc, st_of(s2) == "exited", "exited", "state=%s" % st_of(s2))
    e, r, dt = tool("stop_session", {"session": payload["id"]})
    ev("stop_session after kill", [e, r, "%.2fs" % dt])
    close_pane_if_ours(pane_id, "S10")


def s11():
    sc = "S11 session not created by start_session"
    d = mkcwd("s11")
    r = subprocess.run([BIN, "--home", PFH, "intercom", "session", "add", "--name", "s11", "--cwd", d, "--agent", "pi", "--", "--model", MODEL],
                       capture_output=True, text=True, timeout=60)
    ev("session add", "rc=%s out=%s err=%s" % (r.returncode, r.stdout.strip().replace("\n", " | "), r.stderr.strip()))
    rows, raw = wait_until(lambda: (lambda x: x if x[0] else None)(rows_named("s11")), 10, 0.5) or rows_named("s11")
    if not rows:
        verdict("FAIL", sc, "session registered", raw)
        return
    sid, pane_id = rows[0]["id"], rows[0].get("herdr_pane_id")
    st = status(sid)
    pid = st.get("pid")
    rev0 = (lib.pane(pane_id) or {}).get("revision")
    res = [tool("stop_session", {"session": h}) for h in (sid, pane_id, "s11")]
    for (e, r_, dt), h in zip(res, (sid, pane_id, "s11")):
        ev("stop_session(%s)" % h, [e, r_])
    rev1 = (lib.pane(pane_id) or {}).get("revision")
    check(sc + " refused", all(e for e, _, _ in res) and lib.alive(pid) and rev1 == rev0,
          "refused by id, pane, name; nothing sent", "errs=%s pid alive=%s pane rev %s->%s" % ([e for e, _, _ in res], lib.alive(pid), rev0, rev1))
    others = snapshot(exclude=(pane_id,))
    t = time.time()
    r = subprocess.run([BIN, "--home", PFH, "intercom", "session", "rm", sid], capture_output=True, text=True, timeout=90)
    dt = time.time() - t
    ev("session rm", "rc=%s out=%s err=%s %.2fs" % (r.returncode, r.stdout.strip(), r.stderr.strip(), dt))
    p = lib.pane(pane_id)
    if p:
        ev("pane after rm", [pane_brief(p), pinfo(pane_id)])
    diff = snap_diff(others, snapshot(exclude=(pane_id,)))
    RMS.append((r.returncode == 0 and not lib.alive(pid) and p is None and not diff, round(dt, 2)))
    check(sc + " session rm", r.returncode == 0 and not lib.alive(pid) and p is None and not diff,
          "rm <intercom id>: exit 0, pi stopped, pane closed, other panes untouched",
          "rc=%s pid alive=%s pane exists=%s others changed=%s in %.2fs" % (r.returncode, lib.alive(pid), p is not None, diff, dt))
    if p:
        close_pane_if_ours(pane_id, "S11 rm left it open")


def s12(phase):
    sc = "S12 refusals (%s)" % phase
    up = phase == "herdr up"
    n0 = len(lib.panes()) if up else None
    cases = [
        ("relative cwd", {"cwd": "scratch", "name": "r1"}),
        ("nonexistent cwd", {"cwd": "/nonexistent/e2e-xyz", "name": "r2"}),
        ("name with ;", {"cwd": SCR, "name": "a;b"}),
        ("name starting with -", {"cwd": SCR, "name": "-x"}),
        ("model with space", {"cwd": SCR, "name": "r5", "model": "deepseek flash"}),
    ]
    out = []
    for label, args in cases:
        e, r, dt = lib.tool("start_session", args)
        out.append((label, e, r))
        ev(label, [e, r, "%.3fs" % dt])
    # a validation refusal may cite "herdr's agent-name rule"; what must not appear is a herdr request outcome
    herdr_msg = [l for l, e, r in out if "not available" in str(r).lower() or "agent.start" in str(r) or "pane" in str(r).lower()]
    n1 = len(lib.panes()) if up else None
    if up:
        snap0 = snapshot()
        ws_before = sorted(w.get("workspace_id") for w in lib.herdr("workspace.list").get("workspaces", []))
        refused = []
        for label, nm in (("uppercase", "Upper"), ("space", "two words"), ("dot", "a.b"), ("33 chars", "n" * 33), ("leading -", "-x"),
                          ("leading digit", "1abc"), ("leading _", "_x")):
            e, r, dt = lib.tool("start_session", {"cwd": mkcwd("names"), "name": nm, "model": MODEL})
            refused.append((label, nm, e, r, dt))
            ev("name %s %r" % (label, nm), [e, r, "%.3fs" % dt])
        snap1 = snapshot()
        verdict("PASS" if all(e and "agent.start" not in str(r) for _, nm, e, r, _ in refused) and snap0 == snap1 else "FAIL",
                sc + " name rule refusals", "Upper, 'two words', a.b, 33 chars, -x, 1abc, _x refused by the daemon, no pane, no herdr change",
                "%s; herdr changed=%s" % ([(nm[:12], e, "rule" if "lowercase" in str(r) or "'-'" in str(r) else str(r)[:60]) for _, nm, e, r, _ in refused],
                                         snap_diff(snap0, snap1)))
        for label, nm, e, r, dt in []:
            verdict("OBSERVED", sc + " name " + label, "herdr requires a leading lowercase letter; does the daemon refuse first?",
                    "err=%s result=%r" % (e, r if not isinstance(r, dict) else {k: r.get(k) for k in ("id", "pane_id", "state")}))
            if not e and isinstance(r, dict):
                ee, rr, dd, tr, mk = watch_stop(r["id"], r["pane_id"], r.get("pid"))
                record_stop("S12 name probe %s" % nm, ee, rr, dd, mk)
                close_pane_if_ours(r["pane_id"], "S12 name probe")
        ws_mid = sorted(w.get("workspace_id") for w in lib.herdr("workspace.list").get("workspaces", []))
        ev("workspaces before / after refusals", [ws_before, ws_mid])
        for nm in ("a_b-1", "m" * 32):
            e, r, dt = tool("start_session", {"cwd": mkcwd("names-ok"), "name": nm, "model": MODEL})
            ev("accepted name %r" % nm, [e, r, "%.2fs" % dt])
            check(sc + " name accepted %r" % nm[:12], not e and st_of(r) == "alive", "accepted and started", "err=%s state=%s" % (e, st_of(r)))
            if not e and isinstance(r, dict):
                ee, rr, dd, tr, mk = watch_stop(nm, r["pane_id"], r.get("pid"))
                ev("stop by name %r" % nm, [ee, rr, "%.2fs" % dd])
                record_stop("S12 accepted name (fresh)", ee, rr, dd, mk)
                gone = lib.pane(r["pane_id"]) is None
                if not (stop_closed(ee, rr) and gone):
                    for line in tr:
                        log("   stop-trace " + line)
                check(sc + " stop by name %r" % nm[:12], stop_closed(ee, rr) and gone and not lib.alive(r.get("pid")),
                      "stop succeeds: exited, pane closed, pid dead",
                      "err=%s state=%s pane_exists=%s pid_alive=%s %.2fs" % (ee, st_of(rr), not gone, lib.alive(r.get("pid")), dd))
                close_pane_if_ours(r["pane_id"], "S12 accepted name")
        n1 = len(lib.panes())
    check(sc, all(e for _, e, _ in out) and not herdr_msg and n0 == n1,
          "all refused with a validation message, no pane created",
          "all_refused=%s herdr-mentioning=%s panes %s->%s" % (all(e for _, e, _ in out), herdr_msg, n0, n1))


def s12b():
    sc = "S12b ambiguous name"
    e0, a, _ = start("dup", mkcwd("dupA"))
    ev("first dup", [e0, a])
    lib.herdr("agent.send_keys", {"target": a["pane_id"], "keys": ["ctrl+d"]})
    wait_until(lambda: not lib.alive(a["pid"]) and not (lib.pane(a["pane_id"]) or {}).get("agent"), 10)
    e1_, b, _ = start("dup", mkcwd("dupB"))
    ev("second dup (first one exited)", [e1_, b])
    if e1_:
        verdict("FAIL", sc, "a second session can reuse the name", b)
        close_pane_if_ours(a["pane_id"], "S12b")
        return
    e1, r1, _ = tool("session_status", {"session": "dup"})
    e2, r2, _ = tool("stop_session", {"session": "dup"})
    ev("session_status(dup)", [e1, r1])
    ev("stop_session(dup)", [e2, r2])
    check(sc, e1 and e2 and "ambiguous" in str(r1) and "ambiguous" in str(r2) and lib.alive(b["pid"]),
          "an ambiguous name is an error that says so; nothing stopped",
          "status err=%s stop err=%s msgs=%r / %r; second alive=%s" % (e1, e2, r1, r2, lib.alive(b["pid"])))
    e, r, dt, trans, marks = watch_stop(b["id"], b["pane_id"], b["pid"])
    ev("stop second dup by id", [e, r, "%.2fs" % dt])
    record_stop("S12b dup (fresh)", e, r, dt, marks)
    gone = lib.pane(b["pane_id"]) is None
    if not (stop_closed(e, r) and gone):
        for line in trans:
            log("   stop-trace " + line)
    check(sc + " stop by id", stop_closed(e, r) and gone and not lib.alive(b["pid"]),
          "stop succeeds: exited, pane closed, pid dead",
          "err=%s state=%s pane_exists=%s pid_alive=%s %.2fs" % (e, st_of(r), not gone, lib.alive(b["pid"]), dt))
    close_pane_if_ours(b["pane_id"], "S12b")
    close_pane_if_ours(a["pane_id"], "S12b")


def s13():
    sc = "S13 client disconnect during start_session"
    d = mkcwd("s13")
    before = {p["pane_id"] for p in lib.panes()}
    host, port, tok = lib.mcp_endpoint()
    body = json.dumps({"jsonrpc": "2.0", "id": 99, "method": "tools/call",
                       "params": {"name": "start_session", "arguments": {"cwd": d, "name": "s13", "model": MODEL}}})
    req = ("POST /mcp HTTP/1.1\r\nHost: %s:%d\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n"
           "MCP-Protocol-Version: 2025-06-18\r\nAuthorization: Bearer %s\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s"
           ) % (host, port, tok, len(body), body)
    s = socket.create_connection((host, port))
    s.sendall(req.encode())
    time.sleep(1.0)
    s.close()
    log("   closed the HTTP connection 1s after sending the request")
    new = wait_until(lambda: [p["pane_id"] for p in lib.panes() if p["pane_id"] not in before], 10)
    if not new:
        verdict("FAIL", sc, "a pane was created", "none")
        return
    pane_id = new[0]
    wait_until(lambda: (lambda x: isinstance(x, dict) and x.get("state") == "alive" and x.get("id"))(lib.tool("session_status", {"session": pane_id})[1]), 20, 0.3)
    st = status(pane_id)
    st_name = status("s13")
    ev("session_status(pane), (name)", [st, st_name])
    pid = st.get("pid") if isinstance(st, dict) else None
    err, res, dt, trans, marks = watch_stop("s13", pane_id, pid)
    ev("stop_session(name)", [err, res, "%.2fs" % dt, marks])
    record_stop("S13 after disconnect", err, res, dt, marks)
    p = lib.pane(pane_id)
    check(sc, st_of(st) == "alive" and not err and st_of(res) == "exited" and not lib.alive(pid) and p is None,
          "visible to session_status and stoppable (pane closed)",
          "status=%s stop_err=%s result=%s pid alive=%s pane exists=%s" % (st_of(st), err, st_of(res), lib.alive(pid), p is not None))
    if p:
        close_pane_if_ours(pane_id, "S13")


def s15():
    sc = "S15 (optional) message round trip"
    ea, a, _ = start("s15a", mkcwd("s15a"))
    eb, b, _ = start("s15b", mkcwd("s15b"))
    if ea or eb:
        verdict("OBSERVED", sc, "two sessions", [a, b])
        return
    e, r, _ = tool("send", {"from": a["id"], "to": b["id"],
                            "message": "Reply to the sender of this message with exactly the word PONG, using your messaging tool. Nothing else."})
    ev("send", [e, r])
    sess_a = ((lib.pane(a["pane_id"]) or {}).get("agent_session") or {}).get("value", "")
    sess_b = ((lib.pane(b["pane_id"]) or {}).get("agent_session") or {}).get("value", "")

    def pong_in_a():
        for e_ in session_entries(sess_a)[1]:
            if e_.get("type") == "custom_message" and "PONG" in json.dumps(e_):
                return e_
        return None

    got = wait_until(pong_in_a, 120, 2)
    t_got = None
    if got:
        ev("entry in sender session file", json.dumps(got)[:400])
    b_calls = [json.dumps(c)[:200] for e_ in session_entries(sess_b)[1] for c in ((e_.get("message") or {}).get("content") or [])
               if isinstance(c, dict) and c.get("type") == "toolCall"]
    ev("receiver tool calls", b_calls)
    wait_until(lambda: (lib.pane(a["pane_id"]) or {}).get("agent_status") in ("idle", "done"), 60, 1)
    verdict("PASS" if got else "OBSERVED", sc, "the reply PONG from the receiver arrives in the sender session",
            "PONG cross-session message in the sender session file: %s" % bool(got))
    for x in (a, b):
        e, r, dt, trans, marks = watch_stop(x["id"], x["pane_id"], x["pid"])
        ev("stop %s" % x["id"], [e, r, "%.2fs" % dt, marks])
        record_stop("S15 " + x["name"], e, r, dt, marks)
        check(sc + " stop " + x["name"], not e and st_of(r) == "exited" and lib.pane(x["pane_id"]) is None,
              "stop succeeds, pane closed", "err=%s state=%s pane exists=%s %.2fs" % (e, st_of(r), lib.pane(x["pane_id"]) is not None, dt))
        close_pane_if_ours(x["pane_id"], "S15")


def s14():
    sc = "S14 herdr not running"
    pane_pids = set()
    for p in lib.panes():
        for x in pinfo(p["pane_id"]).get("foreground_processes", []):
            pane_pids.add(x["pid"])
    r = subprocess.run([os.environ["HERDR"], "--session", os.environ["HSESS"], "server", "stop"], capture_output=True, text=True, timeout=30)
    ev("herdr server stop", "rc=%s %s %s" % (r.returncode, r.stdout.strip(), r.stderr.strip()))
    time.sleep(2)
    d = mkcwd("s14")
    e, res, dt = lib.tool("start_session", {"cwd": d, "name": "s14", "model": MODEL})
    ev("start_session", [e, res, "%.2fs" % dt])
    time.sleep(2)
    strays = []
    for pid in os.listdir("/proc"):
        if pid.isdigit():
            try:
                if os.readlink("/proc/%s/cwd" % pid) == d:
                    strays.append(lib.ps(int(pid)))
            except OSError:
                pass
    msg = str(res).lower()
    check(sc, e and msg.count("herdr not available") + msg.count("herdr is not available") == 1 and not strays,
          "a clear error that says each thing once, no stray pane or process",
          "err=%s msg=%r strays=%s in %.2fs" % (e, res, strays, dt))
    s12("herdr down: validation must precede any herdr request")
    ev("pane processes still alive after herdr stop", [(x, lib.ps(x)) for x in sorted(pane_pids) if lib.alive(x)])


def sR():
    sc = "R repeated fresh-session stops"
    for i in range(10):
        nm = "rep%d" % i
        e, r, _ = start(nm, mkcwd(nm))
        if e:
            verdict("FAIL", sc, "session starts", r)
            continue
        if i % 2:
            focus(r["pane_id"])
        else:
            focus(ANCHOR)
        others = snapshot_nofocus(exclude=(r["pane_id"],))
        ee, rr, dd, tr, mk = watch_stop(r["id"], r["pane_id"], r["pid"])
        record_stop("R fresh #%d (%s focused)" % (i, "own" if i % 2 else "other"), ee, rr, dd, mk)
        diff = snap_diff(others, snapshot_nofocus(exclude=(r["pane_id"],)))
        p = lib.pane(r["pane_id"])
        alive = lib.alive(r["pid"])
        if ee or p is not None or alive or diff:
            for line in tr:
                log("   stop-trace " + line)
            ev("failed stop", [ee, rr, mk, diff])
        check("%s #%d (%s focused)" % (sc, i, "own" if i % 2 else "other"),
              stop_closed(ee, rr) and p is None and not alive and not diff,
              "stop succeeds: exited, pane closed, pid dead, other panes unchanged",
              "err=%s state=%s pane=%s pane_exists=%s pid_alive=%s others changed=%s wall %.2fs" % (
                  ee, st_of(rr), rr.get("pane") if isinstance(rr, dict) else rr, p is not None, alive, diff, dd))
        if p is not None:
            close_pane_if_ours(r["pane_id"], "R left open")
    focus(ANCHOR)


RMS = []


def sRM():
    sc = "RM repeated session rm"
    for i in range(5):
        nm = "rm%d" % i
        d = mkcwd(nm)
        r = subprocess.run([BIN, "--home", PFH, "intercom", "session", "add", "--name", nm, "--cwd", d, "--agent", "pi", "--", "--model", MODEL],
                           capture_output=True, text=True, timeout=60)
        kv = dict(l.split("\t", 1) for l in r.stdout.strip().splitlines() if "\t" in l)
        sid, pane_id = kv.get("session_id"), kv.get("pane_id")
        st = status(sid) if sid else {}
        pid = st.get("pid") if isinstance(st, dict) else None
        add_pid(pid)
        if not (sid and pid):
            verdict("FAIL", sc, "session add registers", "rc=%s %s %s" % (r.returncode, r.stdout, r.stderr))
            continue
        others = snapshot(exclude=(pane_id,))
        t = time.time()
        r = subprocess.run([BIN, "--home", PFH, "intercom", "session", "rm", sid], capture_output=True, text=True, timeout=90)
        dt = time.time() - t
        p = lib.pane(pane_id)
        diff = snap_diff(others, snapshot(exclude=(pane_id,)))
        ok = r.returncode == 0 and p is None and not lib.alive(pid) and not diff
        RMS.append((ok, round(dt, 2)))
        log("   rm-record #%d ok=%s rc=%s wall=%.2fs pane_exists=%s pid_alive=%s %s" % (i, ok, r.returncode, dt, p is not None, lib.alive(pid), r.stderr.strip()[:200]))
        check("%s #%d" % (sc, i), ok, "rm <intercom id>: exit 0, pi stopped, pane closed, other panes untouched",
              "rc=%s pane_exists=%s pid_alive=%s others changed=%s in %.2fs %s" % (
                  r.returncode, p is not None, lib.alive(pid), diff, dt, r.stderr.strip()[:200]))
        if p:
            close_pane_if_ours(pane_id, "RM left open")


WS = []


def sW():
    """The user's command (`cat`, which a ctrl+d would end) takes the pane's foreground as the agent dies.
    W1: right after the daemon's ctrl+d kills pi. W2: after the daemon's ctrl+c, before its ctrl+d
    (this script ends pi itself with ctrl+d at ~1s, then types cat)."""
    sc = "W takeover"
    for variant, runs in (("W1 after daemon ctrl+d", 4), ("W2 between ctrl+c and ctrl+d", 3)):
        for i in range(runs):
            nm = "w%s%d" % (variant[1], i)
            e, r, _ = start(nm, mkcwd(nm))
            if e:
                verdict("FAIL", sc, "session starts", r)
                continue
            pane_id, pid = r["pane_id"], r["pid"]
            took = {}

            def type_cat():
                lib.herdr("pane.send_text", {"pane_id": pane_id, "text": "cat"})
                lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["enter"]})
                took["typed"] = time.time()

            if variant.startswith("W2"):
                def early():
                    time.sleep(1.0)
                    lib.herdr("agent.send_keys", {"target": pane_id, "keys": ["ctrl+d"]})
                    wait_until(lambda: not lib.alive(pid), 3, 0.005)
                    type_cat()
                threading.Thread(target=early).start()
                hook = None
            else:
                hook = type_cat
            t0 = time.time()
            ee, rr, dd, tr, mk = watch_stop(r["id"], pane_id, pid, interval=0.01, on_pid_dead=hook)
            p = lib.pane(pane_id)
            cats = [x["pid"] for x in pinfo(pane_id).get("foreground_processes", []) if x["name"] == "cat"] if p else []
            since = (t0 + dd) - took["typed"] if "typed" in took else None
            msg = rr if isinstance(rr, str) else json.dumps(rr)
            ok = p is not None and bool(cats) and all(lib.alive(x) for x in cats) and "another agent" not in msg \
                and "something else" in msg and since is not None and since < 2.5
            WS.append((variant, ok, round(dd, 2), round(since, 2) if since is not None else None, bool(cats), msg[:160]))
            for line in tr:
                log("   stop-trace " + line)
            check("%s %s #%d" % (sc, variant, i), ok,
                  "pane left open, cat survives (no ctrl+d reached it), message says something else runs, returns <2.5s after takeover",
                  "wall %.2fs, %.2fs after cat typed; pane open=%s cat alive=%s; err=%s msg=%r" % (
                      dd, since if since is not None else -1, p is not None, [lib.alive(x) for x in cats], ee, msg[:200]))
            if p:
                lib.herdr("pane.send_keys", {"pane_id": pane_id, "keys": ["ctrl+c"]})
                time.sleep(0.3)
                close_pane_if_ours(pane_id, "W")


def main():
    global ANCHOR
    only = sys.argv[1:] or None
    w = lib.herdr("workspace.create", {"cwd": mkcwd("anchor"), "label": "anchor", "focus": True})
    ANCHOR = w["root_pane"]["pane_id"]
    # a bystander pane running a foreground job, and the anchor: both must never change in stop scenarios
    b = lib.herdr("workspace.create", {"cwd": mkcwd("bystander"), "label": "bystander"})
    BY = b["root_pane"]["pane_id"]
    time.sleep(1)
    lib.herdr("pane.send_text", {"pane_id": BY, "text": "sleep 1200"})
    lib.herdr("pane.send_keys", {"pane_id": BY, "keys": ["enter"]})
    time.sleep(1)
    focus(ANCHOR)
    log("anchor pane %s, bystander pane %s (fg %s), focused=%s" % (ANCHOR, BY, fg_names(BY), focused()))
    steps = [("s1", s1), ("s2", None), ("s5c", s5c), ("s6", s6), ("sA", sA), ("s7", s7), ("sB", sB), ("s8", s8), ("s9", s9),
             ("s10", s10), ("s11", s11), ("s12", lambda: s12("herdr up")), ("s12b", s12b), ("sR", sR), ("sRM", sRM), ("sW", sW), ("s13", s13), ("s15", s15), ("s14", s14)]
    for key, fn in steps:
        if only and key not in only:
            continue
        log("=" * 20 + " " + key)
        try:
            if key == "s2":
                payload = s2_3_4()
                log("=" * 20 + " s5")
                s5(payload)
            else:
                fn()
        except Exception as e:
            import traceback
            verdict("FAIL", key, "scenario runs", "exception %r" % e)
            traceback.print_exc(file=sys.stdout)
    # end the bystander's sleep (ours) before herdr goes away, if still up
    try:
        lib.herdr("pane.send_keys", {"pane_id": BY, "keys": ["ctrl+c"]})
    except Exception:
        pass
    print()
    print("SUMMARY")
    for kind, sc, exp, act in RESULTS:
        print("%-8s %s" % (kind, sc))
    print()
    print("STOPS of live sessions (scenario, closed, wall_s, herdr_agent_lag_s, lag_kind):")
    for x in STOPS:
        print("  ", x)
    closed = [x for x in STOPS if x[1]]
    walls = sorted(x[2] for x in STOPS)
    lags = sorted(x[3] for x in STOPS if x[3] is not None and x[4] == "cleared")
    ub = sorted(x[3] for x in STOPS if x[3] is not None and x[4] != "cleared")
    med = lambda v: v[len(v) // 2] if v else None
    print("closed/total = %d/%d; wall min/median/max = %s/%s/%s" % (len(closed), len(STOPS), walls[0] if walls else None, med(walls), walls[-1] if walls else None))
    print("herdr agent lag after pid death (observed clear, n=%d) min/median/max = %s/%s/%s; pane-closed-first upper bounds n=%d: %s"
          % (len(lags), lags[0] if lags else None, med(lags), lags[-1] if lags else None, len(ub), ub))
    print("session rm ok/total = %d/%d walls=%s" % (sum(1 for o, _ in RMS if o), len(RMS), [w for _, w in RMS]))
    print("W outcomes (variant, ok, wall_s, s_after_takeover, cat_alive, message):")
    for x in WS:
        print("  ", x)
    print("pi pids seen:", sorted(PIDS))
    counts = {k: sum(1 for r in RESULTS if r[0] == k) for k in ("PASS", "FAIL", "OBSERVED")}
    print("TOTAL PASS %(PASS)d FAIL %(FAIL)d OBSERVED %(OBSERVED)d" % counts)
    return 1 if counts["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
