/**
 * After the parent process dies, resume by child_id must reconstruct
 * the run from the disk record and reattach the warm hosted child.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, describe, expect, it, vi } from "vitest";
import { writeAgentChildRecord } from "../../src/subagent/agent-records";
import { hostedFakeState, resetHostedFakes, startHostedExtension, textOf } from "./hosted-fakes";

vi.mock("../../src/manager-client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../src/manager-client")>();
  const { FakeHostedManager } = await import("./hosted-fakes");
  return { ...actual, ManagerClient: FakeHostedManager };
});

const home = mkdtempSync(join(tmpdir(), "pi-famulus-resume-id-"));
process.env.PI_FAMULUS_HOME = home;
process.env.PI_FAMULUS_PI = JSON.stringify(["pi"]);
afterAll(() => rmSync(home, { recursive: true, force: true }));
afterEach(() => resetHostedFakes());

const SESSION = "sess-resume-id";
const CHILD = "ch_left_warm";
const RUN = "run_left_warm";

describe("resume a hosted child by child_id after the parent dies", () => {
  it("reattaches the warm child and sends the resume message", async () => {
    writeAgentChildRecord(home, {
      v: 1,
      kind: "agent",
      child_id: CHILD,
      run_id: RUN,
      session_id: SESSION,
      name: "worker",
      agent: "worker",
      status: "running",
      started_at: Date.now() - 60_000,
      prompt_head: "finish the review",
    });
    hostedFakeState().lookups.set(CHILD, {
      child_id: CHILD,
      residency: "warm",
      activity: "idle",
      parent_session: SESSION,
    });

    const ext = await startHostedExtension({ sessionId: SESSION, cwd: home });
    try {
      let text: string;
      try {
        const executed = await ext.run({ action: "resume", child_id: CHILD, message: "continue the review" });
        text = textOf(executed);
      } catch (error) {
        text = `refused: ${error instanceof Error ? error.message : String(error)}`;
      }
      expect({
        refused: text.startsWith("refused"),
        namesChild: text.includes(CHILD) || ext.manager.spawns.some((spawn) => spawn.childId === CHILD),
        prompted: ext.manager.inputs.some(
          (input) => input.childId === CHILD && input.text.includes("continue the review"),
        ),
      }).toEqual({
        refused: false,
        namesChild: true,
        prompted: true,
      });
    } finally {
      await ext.shutdown();
    }
  });
});
