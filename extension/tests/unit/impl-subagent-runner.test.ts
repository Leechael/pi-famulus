import { beforeEach, describe, expect, it } from "vitest";
import { ManualClock } from "../../src/clock";
import { InProcessRunner } from "../../src/subagent/runner";
import type { ChildRunRequest } from "../../src/subagent/types";
import { FakeChildSession, SessionFactory, tick, WORKER_AGENT } from "./subagent-fakes";

function makeReq(overrides: Partial<ChildRunRequest> = {}): ChildRunRequest {
  return {
    childId: "ch_test0001",
    runId: "run_test0001",
    name: "worker-1",
    prompt: "do the thing",
    agent: WORKER_AGENT,
    timeoutMs: 60_000,
    depth: 1,
    ...overrides,
  };
}

describe("InProcessRunner", () => {
  for (const stopReason of ["error", "aborted"] as const) {
    it(`settles provider stopReason=${stopReason} as failed`, async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      factory.configure = (session) => {
        session.lastAssistantFailure = { stopReason, errorMessage: "529 overloaded_error" };
      };
      const runner = new InProcessRunner({ createSession: factory.fn });
      const handle = await runner.start(makeReq());
      factory.sessions[0].complete();
      const result = await handle.result;
      expect(result.status).toBe("failed");
      expect(result.error).toBe("529 overloaded_error");
      expect(handle.status()).toBe("failed");
    });
  }

  it("completes with the last assistant text", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = "all done";
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    const result = await handle.result;
    expect(result.status).toBe("completed");
    expect(result.text).toBe("all done");
    expect(result.attempts).toBeUndefined(); // omitted on a clean first pass
    expect(result.durationMs).toBeGreaterThanOrEqual(0);
    expect(factory.sessions[0].prompts).toEqual(["do the thing"]);
    expect(handle.status()).toBe("completed");
  });

  it("attributes queued, assistant-message, and tool wall time with a fake clock", async () => {
    const clock = new ManualClock(0);
    const factory = new SessionFactory();
    factory.autoComplete = null;
    let grant!: (release: (terminal?: boolean) => void) => void;
    const admission = new Promise<(terminal?: boolean) => void>((resolve) => { grant = resolve; });
    const runner = new InProcessRunner({
      createSession: factory.fn,
      clock,
      stallMs: 0,
      acquire: async () => admission,
    });
    const starting = runner.start(makeReq());
    await tick();
    clock.advanceBy(500);
    grant(() => {});
    const handle = await starting;
    const session = factory.sessions[0];

    session.emitEvent({ type: "message_start", role: "assistant" });
    clock.advanceBy(1_200);
    session.emitEvent({ type: "message_end", role: "assistant" });
    const tool = session.runTool();
    clock.advanceBy(300);
    tool.end();
    session.emitEvent({ type: "message_start", role: "assistant" });
    clock.advanceBy(500);
    session.emitEvent({ type: "message_end", role: "assistant" });
    session.complete("done");
    await handle.result;

    expect(handle.wallUsage()).toEqual({
      llmMs: 1_700,
      toolMs: 300,
      queueMs: 500,
      otherMs: 0,
      approximate: false,
    });
  });

  it("tells the child its resolved model on the first prompt", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = "ok";
    factory.configure = (session) => {
      session.resolvedModel = "openai/gpt-5.6-sol";
    };
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    await handle.result;
    expect(handle.resolvedModel()).toBe("openai/gpt-5.6-sol");
    expect(factory.sessions[0].prompts[0]).toBe(
      "You are running as model openai/gpt-5.6-sol.\n\ndo the thing",
    );
  });

  it("does not re-announce the model on resume prompts", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = "first";
    factory.configure = (session) => {
      session.resolvedModel = "openai/gpt-5.6-sol";
    };
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    await handle.result;
    factory.sessions[0].autoComplete = "second";
    await handle.resume("keep going");
    await handle.result;
    expect(factory.sessions[0].prompts).toEqual([
      "You are running as model openai/gpt-5.6-sol.\n\ndo the thing",
      "keep going",
    ]);
  });

  it('maps empty output to "(no output)"', async () => {
    const factory = new SessionFactory();
    factory.autoComplete = "";
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    const result = await handle.result;
    expect(result.status).toBe("completed");
    expect(result.text).toBe("(no output)");
  });

  it("fails the child when the prompt throws", async () => {
    const factory = new SessionFactory();
    factory.configure = (s) => {
      s.promptError = new Error("no API key");
    };
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    const result = await handle.result;
    expect(result.status).toBe("failed");
    expect(result.error).toContain("no API key");
  });

  it("fails the child when session creation throws", async () => {
    const factory = new SessionFactory();
    factory.createError = new Error("pi package unavailable");
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    const result = await handle.result;
    expect(result.status).toBe("failed");
    expect(result.error).toContain("pi package unavailable");
  });

  it("steer/followUp deliver to a running session and throw once terminal", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null; // manual completion
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    expect(handle.status()).toBe("running");
    await handle.steer("focus");
    await handle.followUp("and then");
    expect(factory.sessions[0].steers).toEqual(["focus"]);
    expect(factory.sessions[0].followUps).toEqual(["and then"]);
    factory.sessions[0].complete("finished");
    await handle.result;
    await expect(handle.steer("too late")).rejects.toThrow(/not running/);
    await expect(handle.followUp("too late")).rejects.toThrow(/not running/);
  });

  it("interrupt aborts and resolves interrupted", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    await handle.interrupt();
    const result = await handle.result;
    expect(result.status).toBe("interrupted");
    expect(factory.sessions[0].aborts).toBe(1);
    expect(handle.status()).toBe("interrupted");
    // Second interrupt is a no-op.
    await handle.interrupt();
    expect(factory.sessions[0].aborts).toBe(1);
  });

  it("resume re-prompts the same session and exposes a new result promise", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = "first";
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    const first = await handle.result;
    expect(first.text).toBe("first");
    expect(factory.sessions).toHaveLength(1);

    factory.sessions[0].autoComplete = null;
    await handle.resume("keep going");
    expect(handle.status()).toBe("running");
    expect(factory.sessions).toHaveLength(1); // same session object
    expect(factory.sessions[0].prompts).toEqual(["do the thing", "keep going"]);

    const secondPromise = handle.result;
    factory.sessions[0].complete("second");
    const second = await secondPromise;
    expect(second.status).toBe("completed");
    expect(second.text).toBe("second");
    // The first generation's promise stays resolved with the first result.
    await expect(Promise.resolve(first)).resolves.toMatchObject({ text: "first" });
  });

  it("resume on a running child throws", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const runner = new InProcessRunner({ createSession: factory.fn });
    const handle = await runner.start(makeReq());
    await expect(handle.resume("nope")).rejects.toThrow(/still running/);
    factory.sessions[0].complete();
    await handle.result;
  });

  it("invokes the acquire hook per generation and releases on settle", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = "one";
    let acquired = 0;
    let released = 0;
    const runner = new InProcessRunner({
      createSession: factory.fn,
      acquire: async () => {
        acquired++;
        return () => {
          released++;
        };
      },
    });
    const handle = await runner.start(makeReq());
    await handle.result;
    expect(acquired).toBe(1);
    expect(released).toBe(1);

    factory.sessions[0].autoComplete = "two";
    await handle.resume("again");
    await handle.result;
    expect(acquired).toBe(2);
    expect(released).toBe(2);
  });

  it("keeps the machine permit across an interrupted resumable turn", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const releases: boolean[] = [];
    const runner = new InProcessRunner({
      createSession: factory.fn,
      acquire: async () => (terminal = true) => { releases.push(terminal); },
    });
    const handle = await runner.start(makeReq());
    await handle.interrupt();
    expect(releases).toEqual([false]);
    factory.sessions[0].autoComplete = "resumed";
    await handle.resume("continue");
    expect((await handle.result).status).toBe("completed");
    expect(releases).toEqual([false, true]);
  });

  it("keeps the permit when admission resolves after a resumable interruption", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const releases: boolean[] = [];
    let resolveAdmission!: (release: (terminal?: boolean) => void) => void;
    let acquired = 0;
    const runner = new InProcessRunner({
      createSession: factory.fn,
      acquire: async () => {
        acquired++;
        if (acquired === 1) return (terminal = true) => { releases.push(terminal); };
        return new Promise((resolve) => { resolveAdmission = resolve; });
      },
    });
    const handle = await runner.start(makeReq());
    await handle.interrupt();
    await handle.resume("continue");
    await tick();
    expect(acquired).toBe(2);
    await handle.interrupt();
    resolveAdmission((terminal = true) => { releases.push(terminal); });
    await tick();
    expect(releases).toEqual([false, false, false]);
    handle.dispose();
    expect(releases).toEqual([false, false, false, true]);
  });

  it("releases an interrupted child's retained machine permit on disposal", async () => {
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const releases: boolean[] = [];
    const runner = new InProcessRunner({
      createSession: factory.fn,
      acquire: async () => (terminal = true) => { releases.push(terminal); },
    });
    const handle = await runner.start(makeReq());
    await handle.interrupt();
    expect(releases).toEqual([false]);
    handle.dispose();
    expect(releases).toEqual([false, true]);
  });

  it("cancels the child when admission rejects", async () => {
    const factory = new SessionFactory();
    const runner = new InProcessRunner({
      createSession: factory.fn,
      acquire: async () => {
        throw new Error("cancelled (fail_fast)");
      },
    });
    const handle = await runner.start(makeReq());
    const result = await handle.result;
    expect(result.status).toBe("interrupted");
    expect(result.error).toContain("fail_fast");
    expect(factory.sessions).toHaveLength(0); // no session was created
  });

  describe("timers (ManualClock)", () => {
    let clock: ManualClock;
    beforeEach(() => {
      clock = new ManualClock();
    });

    // timeoutMs is a soft deadline now (impl-subagent-soft-deadline.test.ts);
    // the abort-and-settle contract below belongs to the opt-in hardTimeoutMs.
    it("hard ceiling (hardTimeoutMs) aborts and resolves interrupted with error=timeout", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({ createSession: factory.fn, clock, hardTimeoutMs: 1000 });
      const handle = await runner.start(makeReq());
      expect(handle.status()).toBe("running");
      clock.advanceBy(1000);
      const result = await handle.result;
      expect(result.status).toBe("interrupted");
      expect(result.error).toBe("timeout");
      expect(factory.sessions[0].aborts).toBe(1);
    });

    it("stall watchdog aborts after stallMs without events", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({ createSession: factory.fn, stallMs: 500, clock, stallRetries: 0 });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500);
      const result = await handle.result;
      expect(result.status).toBe("failed");
      expect(result.error).toBe("stalled");
      expect(factory.sessions[0].aborts).toBe(1);
    });

    it("does not stall while a tool is executing", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({ createSession: factory.fn, stallMs: 500, clock, stallRetries: 0 });
      const handle = await runner.start(makeReq());
      const emit = (factory.sessions[0] as unknown as { emit: (e: { type: string }) => void }).emit.bind(
        factory.sessions[0],
      );
      emit({ type: "tool_execution_start" });
      clock.advanceBy(2_000);
      expect(handle.status()).toBe("running");
      emit({ type: "tool_execution_end" });
      clock.advanceBy(500);
      expect(handle.status()).toBe("failed");
    });

    it("stalls generation 2 after a hard ceiling that landed mid-tool", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 50,
        clock,
        stallRetries: 0,
        hardTimeoutMs: 100,
      });
      const handle = await runner.start(makeReq());
      const emit = (factory.sessions[0] as unknown as { emit: (e: { type: string }) => void }).emit.bind(
        factory.sessions[0],
      );
      emit({ type: "tool_execution_start" });
      clock.advanceBy(100);
      expect(handle.status()).toBe("interrupted");
      emit({ type: "tool_execution_end" });
      await handle.resume("again");
      await tick(); // the ceiling's abort drains, then the turn starts
      emit({ type: "tool_execution_end" });
      clock.advanceBy(50);
      expect(handle.status()).toBe("failed");
      expect((await handle.result).error).toBe("stalled");
    });

    it("pauses the stall watchdog while a supervisor decision is pending", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({ createSession: factory.fn, stallMs: 500, clock, stallRetries: 0 });
      const handle = await runner.start(makeReq());
      const stallControl = handle as typeof handle & { pauseStall(): void; resumeStall(): void };
      stallControl.pauseStall();
      clock.advanceBy(2_000);
      expect(handle.status()).toBe("running");
      stallControl.resumeStall();
      clock.advanceBy(499);
      expect(handle.status()).toBe("running");
      clock.advanceBy(1);
      expect((await handle.result).error).toBe("stalled");
    });

    it("session events reset the stall watchdog", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({ createSession: factory.fn, stallMs: 500, clock, stallRetries: 0 });
      const handle = await runner.start(makeReq());
      const session = factory.sessions[0];
      clock.advanceBy(400);
      session.event(); // resets the watchdog at t=400
      clock.advanceBy(400); // t=800, 400 since last event
      expect(handle.status()).toBe("running");
      clock.advanceBy(100); // t=900, 500 since last event
      const result = await handle.result;
      expect(result.status).toBe("failed");
      expect(result.error).toBe("stalled");
    });

    it("events after settle do not re-arm the watchdog", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = "quick";
      const runner = new InProcessRunner({ createSession: factory.fn, stallMs: 500, clock });
      const handle = await runner.start(makeReq());
      await handle.result;
      factory.sessions[0].event();
      clock.advanceBy(1000);
      expect(handle.status()).toBe("completed"); // unchanged
    });

    it("auto-resumes on stall: retries on the same session and completes", async () => {
      // The dogfood incident: the model stream stalled silently mid-response
      // (one message out, then nothing). Expect: abort + auto-resume with a
      // continuation prompt on the SAME session, then a normal completion.
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const stalls: Array<{ childId: string; attempt: number }> = [];
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
        onStall: (childId, attempt) => stalls.push({ childId, attempt }),
      });
      const handle = await runner.start(makeReq());
      const session = factory.sessions[0];
      clock.advanceBy(400);
      session.event(); // text lands at t=400; then the stream goes silent
      clock.advanceBy(500); // t=900: stall detected
      expect(stalls).toEqual([{ childId: "ch_test0001", attempt: 1 }]);
      expect(session.aborts).toBe(1);
      // The aborted generation settles later; it must NOT complete the run.
      await tick();
      expect(handle.status()).toBe("running");
      expect(session.prompts).toHaveLength(1);
      // Retry fires after the delay.
      clock.advanceBy(100); // t=1000
      await tick();
      expect(session.prompts).toHaveLength(2);
      expect(session.prompts[1]).toContain("stalled with no activity");
      expect(factory.sessions).toHaveLength(1); // same session, no re-admission
      session.complete("all done");
      const result = await handle.result;
      expect(result.status).toBe("completed");
      expect(result.text).toBe("all done");
      expect(result.attempts).toBe(2);
    });

    it("settles failed (stalled) only after the retry budget is spent", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const stalls: number[] = [];
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        stallRetries: 1,
        clock,
        onStall: (_childId, attempt) => stalls.push(attempt),
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall 1 at t=500 -> retry
      expect(handle.status()).toBe("running");
      clock.advanceBy(100); // retry generation starts at t=600
      await tick();
      clock.advanceBy(500); // stall 2 at t=1100 -> budget spent
      const result = await handle.result;
      expect(result.status).toBe("failed");
      expect(result.error).toBe("stalled");
      expect(result.attempts).toBe(2);
      expect(stalls).toEqual([1, 2]);
      expect(factory.sessions[0].aborts).toBe(2);
    });

    it("interrupt during the retry delay cancels the retry", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stalled, retry pending
      expect(factory.sessions[0].aborts).toBe(1);
      await handle.interrupt();
      expect((await handle.result).status).toBe("interrupted");
      clock.advanceBy(1000); // retry timer would fire now
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(1); // no retry prompt
    });

    it("a stall retry keeps the admission slot (no re-acquire, one release)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      let acquired = 0;
      let released = 0;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
        acquire: async () => {
          acquired++;
          return () => {
            released++;
          };
        },
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500);
      clock.advanceBy(100);
      await tick();
      factory.sessions[0].complete("done");
      const result = await handle.result;
      expect(result.status).toBe("completed");
      expect(acquired).toBe(1);
      expect(released).toBe(1);
    });

    it("does not let an abandoned acquire overwrite a resumed generation's queue wait", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      let acquisitions = 0;
      let resolveOldAcquire!: (release: () => void) => void;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        clock,
        acquire: async () => {
          acquisitions++;
          if (acquisitions === 2) {
            return new Promise<() => void>((resolve) => { resolveOldAcquire = resolve; });
          }
          if (acquisitions === 3) await clock.sleep(100);
          return () => {};
        },
      });
      const handle = await runner.start(makeReq());
      await handle.interrupt();

      await handle.resume("queued old generation");
      await tick();
      expect(acquisitions).toBe(2);
      clock.advanceBy(500);
      await handle.interrupt();

      await handle.resume("current generation");
      const currentResult = handle.result;
      await tick();
      expect(acquisitions).toBe(3);
      clock.advanceBy(100);
      await tick();
      clock.advanceBy(100);
      resolveOldAcquire(() => {});
      await tick();

      factory.sessions[0].complete("done");
      const result = await currentResult;
      expect(result.status).toBe("completed");
      expect(result.queueMs).toBe(100);
    });

    it("preserves the turn's admission wait across a stall retry", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        stallRetries: 1,
        clock,
        acquire: async () => {
          await clock.sleep(250);
          return () => {};
        },
      });
      const starting = runner.start(makeReq());
      await tick();
      clock.advanceBy(250);
      const handle = await starting;
      clock.advanceBy(500); // first generation stalls
      clock.advanceBy(100); // retry reuses the held slot
      await tick();
      factory.sessions[0].complete("recovered");
      const result = await handle.result;
      expect(result.status).toBe("completed");
      expect(result.queueMs).toBe(250);
    });

    it("a completed stall retry re-arms the watchdog for the new generation", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall 1 -> retry
      clock.advanceBy(100);
      await tick();
      factory.sessions[0].event(); // retry generation is alive at t=1100
      clock.advanceBy(400);
      expect(handle.status()).toBe("running");
      clock.advanceBy(100); // 500 silent in the retry generation
      const result = await handle.result;
      expect(result.status).toBe("failed");
      expect(result.error).toBe("stalled");
      expect(result.attempts).toBe(2);
    });

    it("retry waits for the abort before re-prompting (B1)", async () => {
      // Real pi rejects prompt() while the aborted run is still active
      // ("Agent is already processing"); the fake mirrors that. The abort
      // here unwinds asynchronously, so a retry that prompts without
      // awaiting the abort settles failed with the wrong error.
      const factory = new SessionFactory();
      factory.autoComplete = null;
      factory.configure = (session) => {
        session.abortGateOpen = false;
      };
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall detected, abort pending behind the gate
      expect(factory.sessions[0].aborts).toBe(1);
      clock.advanceBy(100); // retry timer fires; abort still not resolved
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(1); // no premature prompt
      factory.sessions[0].openAbortGate();
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(2); // retry re-prompts
      factory.sessions[0].complete("recovered");
      const result = await handle.result;
      expect(result.status).toBe("completed");
      expect(result.text).toBe("recovered");
      expect(result.stalls).toBe(1);
    });

    it("an abort the hung stream never answers settles failed (stalled), no retry (B1)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      factory.configure = (session) => {
        session.hungAbort = true;
      };
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall; abort() never resolves
      clock.advanceBy(100); // retry timer fires, bounded abort wait starts
      expect(handle.status()).toBe("running");
      clock.advanceBy(500); // bound (stallMs) expires: session is dead
      await tick();
      const result = await handle.result;
      expect(result.status).toBe("failed");
      expect(result.error).toBe("stalled");
      expect(result.stalls).toBe(1);
      expect(factory.sessions[0].prompts).toHaveLength(1); // no retry prompt
    });

    // S1 now holds for the hard ceiling; the soft-deadline twin (a wake, and
    // the retry still runs) is in impl-subagent-soft-deadline.test.ts.
    it("a hard ceiling landing during the retry delay still fires (S1)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 5_000,
        clock,
        hardTimeoutMs: 800,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall at t=500; retry would start t=5500
      clock.advanceBy(300); // t=800: the turn budget is spent mid-delay
      const result = await handle.result;
      expect(result.status).toBe("interrupted");
      expect(result.error).toBe("timeout");
      clock.advanceBy(10_000);
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(1); // retry suppressed
    });

    it("a stall retry arms only the remaining hard ceiling (S1)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
        hardTimeoutMs: 2000,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall at t=500
      clock.advanceBy(100); // retry generation at t=600; budget left: 1400
      await tick();
      // Keep the retry generation alive (its own stall watchdog is 500ms).
      for (let i = 0; i < 3; i++) {
        clock.advanceBy(400);
        factory.sessions[0].event();
      } // t=1800, last event at 1800
      clock.advanceBy(199); // t=1999
      expect(handle.status()).toBe("running");
      clock.advanceBy(1); // t=2000: turn budget spent
      const result = await handle.result;
      expect(result.status).toBe("interrupted");
      expect(result.error).toBe("timeout");
    });

    it("dispose during the retry delay cancels the retry (S5)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stalled, retry pending
      handle.dispose();
      expect((await handle.result).error).toBe("disposed");
      clock.advanceBy(1000);
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(1); // no retry prompt
    });

    it("user resume() resets the stall budget (S3)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // turn 1 stalls at t=500
      clock.advanceBy(100); // retry at t=600
      await tick();
      factory.sessions[0].complete("first");
      const first = await handle.result;
      expect(first.stalls).toBe(1);

      await handle.resume("again"); // turn 2 gets a fresh stall budget
      clock.advanceBy(500); // turn 2 stalls: attempt 1 of the new budget
      expect(handle.status()).toBe("running"); // a retry is scheduled, not terminal
      clock.advanceBy(100);
      await tick();
      factory.sessions[0].complete("second");
      const second = await handle.result;
      expect(second.status).toBe("completed");
      expect(second.text).toBe("second");
      expect(second.attempts).toBe(4);
      expect(second.stalls).toBe(1); // per-user-turn count
    });

    it("steer/followUp during the restart window are rejected (N1)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stalled, retry pending
      await expect(handle.steer("focus")).rejects.toThrow(/restarting after a stall/);
      await expect(handle.followUp("more")).rejects.toThrow(/restarting after a stall/);
      clock.advanceBy(100);
      await tick();
      factory.sessions[0].complete("done");
      expect((await handle.result).status).toBe("completed");
    });

    it("late events from the aborted generation do not break the retry (S5)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetryDelayMs: 100,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall detected
      const emit = (factory.sessions[0] as unknown as { emit: (e: { type: string }) => void }).emit.bind(
        factory.sessions[0],
      );
      emit({ type: "agent_end" }); // late unwind event from the dead generation
      emit({ type: "message_end" });
      clock.advanceBy(100);
      await tick();
      factory.sessions[0].complete("done");
      const result = await handle.result;
      expect(result.status).toBe("completed");
      expect(result.stalls).toBe(1);
    });

    it("queue wait does not consume the hard ceiling (R1)", async () => {
      // runStartedAt is set before admission; the timeout budget must not
      // start until the admission slot and the session exist. Regression:
      // a child queued longer than timeoutMs was settled timeout before its
      // first prompt.
      const factory = new SessionFactory();
      factory.autoComplete = null;
      let releaseAdmission: (() => void) | null = null;
      const runner = new InProcessRunner({
        createSession: factory.fn,
        clock,
        hardTimeoutMs: 1000,
        acquire: () =>
          new Promise<() => void>((resolve) => {
            releaseAdmission = () => resolve(() => {});
          }),
      });
      const startPromise = runner.start(makeReq());
      await tick(); // reach the admission wait
      clock.advanceBy(1001); // queued past the full budget
      releaseAdmission!();
      const handle = await startPromise;
      // The full budget starts after admission: the child gets a real run.
      expect(factory.sessions[0].prompts).toHaveLength(1);
      clock.advanceBy(999);
      expect(handle.status()).toBe("running");
      clock.advanceBy(1);
      const result = await handle.result;
      expect(result.status).toBe("interrupted");
      expect(result.error).toBe("timeout");
    });

    it("resume after a terminal stall waits for the in-flight abort", async () => {
      // Terminal stall (budget 0) settles while its abort is still
      // unwinding. A resume issued in that window must not re-prompt until
      // the abort finishes — real pi rejects prompt() while the aborted run
      // is still active.
      const factory = new SessionFactory();
      factory.autoComplete = null;
      factory.configure = (session) => {
        session.abortGateOpen = false;
      };
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetries: 0,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall → failed(stalled), abort pending at the gate
      const first = await handle.result;
      expect(first.error).toBe("stalled");

      await handle.resume("try again"); // accepted; the drain runs in the background
      await tick();
      expect(handle.status()).toBe("pending");
      expect(factory.sessions[0].prompts).toHaveLength(1); // waits for the abort
      factory.sessions[0].openAbortGate();
      await tick();
      expect(factory.sessions[0].prompts).toHaveLength(2);
      factory.sessions[0].complete("recovered");
      const second = await handle.result;
      expect(second.status).toBe("completed");
      expect(second.text).toBe("recovered");
    });

    it("a resumed turn settles failed when the abort never completes (hung session)", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      factory.configure = (session) => {
        session.hungAbort = true;
      };
      const runner = new InProcessRunner({
        createSession: factory.fn,
        stallMs: 500,
        stallRetries: 0,
        clock,
      });
      const handle = await runner.start(makeReq());
      clock.advanceBy(500); // stall → failed(stalled), abort never resolves
      expect((await handle.result).error).toBe("stalled");
      // The caller is not held by the drain: resume() resolves at once, and
      // the dead session is reported through the turn's result.
      await handle.resume("try again");
      expect(handle.status()).toBe("pending");
      clock.advanceBy(500); // bound (stallMs) expires
      const second = await handle.result;
      expect(second).toMatchObject({ status: "failed", text: "" });
      expect(second.error).toMatch(/did not go idle/);
      expect(factory.sessions[0].prompts).toHaveLength(1);
      expect(clock.pendingTimers).toBe(0);
    });

    it("lastEventAt tracks session events", async () => {
      const factory = new SessionFactory();
      factory.autoComplete = null;
      const runner = new InProcessRunner({ createSession: factory.fn, clock });
      const handle = await runner.start(makeReq());
      const atStart = handle.lastEventAt();
      clock.advanceBy(2000);
      factory.sessions[0].event();
      expect(handle.lastEventAt()).toBeGreaterThan(atStart);
      factory.sessions[0].complete();
      await handle.result;
    });
  });

  it("dispose releases the session, closes generation timers, and never hangs result waiters", async () => {
    const clock = new ManualClock();
    const factory = new SessionFactory();
    factory.autoComplete = null;
    const runner = new InProcessRunner({ createSession: factory.fn, clock, stallMs: 50 });
    const handle = await runner.start(makeReq({ timeoutMs: 100 }));
    handle.dispose();
    const result = await handle.result;
    clock.advanceBy(1000);
    expect(result.status).toBe("interrupted");
    expect(result.error).toBe("disposed");
    expect(handle.status()).toBe("interrupted");
    expect(factory.sessions[0].disposed).toBe(true);
    expect(factory.sessions[0].aborts).toBe(0);
    await tick();
  });
});
