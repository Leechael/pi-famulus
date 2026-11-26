import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { installWaitGuard, waitShape } from "../../src/wait-guard";
import { loadConfig } from "../../src/config";

// 2026-11-07 log-monitor session (deepseek-flash): 76 bash waits and producers
// started with `&`, though the monitor prompt said not to. Commands below are
// taken from that session.

describe("waitShape", () => {
  it.each([
    ["cd ~/.local/share/probe-detect && sleep 45; wc -l /tmp/v3-*.jsonl 2>/dev/null", "sleep 45"],
    ["cd ~/.local/share/probe-detect && sleep 55; date -u +%FT%TZ; ps -eo args | grep -c '[a]nsible nodes'", "sleep 55"],
    ["cd x && while [ \"$(grep -c '::' nft-canary2-after.txt 2>/dev/null)\" = \"0\" ]; do sleep 5; done; echo ready", "a loop that sleeps"],
    ["sleep 30", "sleep 30"],
    // agent-wait-ready eval, 2026-11-08: a for loop slipped past a while/until-only check.
    ["for i in $(seq 1 200); do [ -f .ready ] && break; sleep 0.25; done; node smoke.cjs", "a loop that sleeps"],
  ])("waits: %j", (command, what) => {
    expect(waitShape(command)).toEqual({ kind: "wait", what });
  });

  it.each([
    ["cd x && cp fleet-watch.sh fleet-watch-run.sh && (./fleet-watch-run.sh > fleet/run.out 2>&1 &) ; sleep 3; echo started", "&"],
    ["(./run-onnode.sh prod5 > a.txt 2>&1; echo done) & (cd b && ./run-onnode.sh prod9 > b.txt)", "&"],
    ["nohup ./watch.sh > out", "nohup"],
    ["npm run dev &", "&"],
  ])("detaches: %j", (command, what) => {
    expect(waitShape(command)).toEqual({ kind: "detach", what });
  });

  it.each([
    "pkill -f fleet-watch-run.sh; sleep 1; pkill -f 'tail -F -n 0 /var/log/haproxy.log'; sleep 2; echo left",
    "cd a && ./x.sh > out 2>&1 && echo ok",
    "./x.sh &> out",
    "timeout 120 ansible prod5 -m shell -a 'for p in $(pgrep x); do kill $p & done; sleep 30'",
    "curl -s 'https://example.com/api?a=1&b=2' | jq .",
    "python3 - <<'PY'\nimport time\nx = a & b\ntime.sleep(60)\nPY\necho done",
    "echo \"run it with & later\" # then sleep 60",
    "grep -c '[s]leep 99' log.txt",
    "for h in a b; do ssh $h systemctl restart app; done; sleep 2; curl -s localhost:8080/health",
  ])("leaves work alone: %j", (command) => {
    expect(waitShape(command)).toBeNull();
  });
});

type Handler = (event: any) => unknown;
function setup(enabled = true) {
  const handlers: Handler[] = [];
  const blocks: unknown[] = [];
  installWaitGuard({ on: (name: string, h: Handler) => name === "tool_call" && handlers.push(h) } as never, {
    enabled,
    onBlock: (info) => blocks.push(info),
  });
  const call = async (toolName: string, input: Record<string, unknown>) => {
    let out: unknown;
    for (const h of handlers) out = (await h({ toolName, input, toolCallId: "c1" })) ?? out;
    return out as { block?: boolean; terminate?: boolean; reason?: string } | undefined;
  };
  return { call, blocks };
}

describe("installWaitGuard", () => {
  it("blocks a bash wait without ending the run and names the monitor alternative", async () => {
    const g = setup();
    const r = await g.call("bash", { command: "sleep 45; ls" });
    expect(r?.block).toBe(true);
    expect(r?.terminate).toBeUndefined();
    expect(r?.reason).toContain("sleep 45");
    expect(r?.reason).toContain("persistent:true");
    expect(g.blocks).toEqual([{ kind: "wait", what: "sleep 45" }]);
  });

  it("blocks a detached launch with the foreground alternative", async () => {
    const r = await setup().call("bash", { command: "(./run.sh > out 2>&1 &)" });
    expect(r?.block).toBe(true);
    expect(r?.reason).toContain("monitor command");
    expect(r?.reason).toContain("foreground");
  });

  it("does not touch other tools or ordinary bash", async () => {
    const g = setup();
    expect(await g.call("monitor", { command: "while true; do sleep 60; check; done", persistent: true })).toBeUndefined();
    expect(await g.call("bash", { command: "ls -la" })).toBeUndefined();
  });

  it("is off when disabled, and on by default in config", async () => {
    expect(await setup(false).call("bash", { command: "sleep 45" })).toBeUndefined();
    const home = mkdtempSync(join(tmpdir(), "pf-wait-guard-"));
    try {
      expect(loadConfig(home).waitGuard).toBe(true);
      writeFileSync(join(home, "config.json"), JSON.stringify({ waitGuard: false }));
      expect(loadConfig(home).waitGuard).toBe(false);
    } finally {
      rmSync(home, { recursive: true, force: true });
    }
  });
});
