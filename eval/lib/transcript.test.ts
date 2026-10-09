import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { itemFromMessage, itemsFromEvents, wakes } from "./transcript.ts";

const wakeMessage = {
  role: "custom", customType: "pi-famulus-wake", content: "",
  details: { kind: "task", tasks: [], stillRunning: [] },
};
const wakeEvent = (seq: number) => ({ type: "message_end", seq, t: 60217, message: wakeMessage });

describe("wake delivery turn provenance", () => {
  it("invariant: absent turn_start and standalone message normalization leave delivery turn unknown", () => {
    const normalized = itemsFromEvents([wakeEvent(25), wakeEvent(27)]);
    assert.deepEqual(normalized, [
      ...itemFromMessage(wakeMessage, 25, 60217),
      ...itemFromMessage(wakeMessage, 27, 60217),
    ]);
    assert.ok(wakes(normalized).every((w) => !Object.hasOwn(w, "turnSeq")));
  });

  it("invariant: a finished turn or agent boundary cannot leak stale delivery metadata", () => {
    for (const boundary of ["turn_end", "agent_end", "agent_start", "agent_settled"]) {
      const items = itemsFromEvents([
        { type: "turn_start", seq: 0, t: 0 }, wakeEvent(1),
        { type: boundary, seq: 2, t: 60217 }, wakeEvent(3),
      ]);
      const [inside, outside] = wakes(items);
      assert.equal(inside.turnSeq, 0, "turn zero is valid evidence");
      assert.equal(Object.hasOwn(outside, "turnSeq"), false, boundary);
    }
  });

  it("regression: records shared and distinct explicit turn_start identities, not timestamps", () => {
    // Incident ordering: turn_start 23, quick wake 25, slow wake 27.
    // The second turn below deliberately has the same timestamp.
    const items = itemsFromEvents([
      { type: "turn_start", seq: 23, t: 60216 }, wakeEvent(25), wakeEvent(27),
      { type: "turn_end", seq: 29, t: 60217 },
      { type: "turn_start", seq: 31, t: 60217 }, wakeEvent(33),
    ]);
    assert.deepEqual(wakes(items).map((w) => ({ seq: w.seq, turnSeq: w.turnSeq })), [
      { seq: 25, turnSeq: 23 }, { seq: 27, turnSeq: 23 }, { seq: 33, turnSeq: 31 },
    ]);
  });
});
