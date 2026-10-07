/**
 * Frozen model-facing tools from @injaneity/pi-computer-use@0.5.1.
 * Source: extensions/computer-use.ts (stateId, conditionProperties,
 * observeTool, waitForTool). Uses the same typebox@1.1.38 schema library;
 * descriptions/snippets/guidelines/JSON schema are unchanged.
 * NEVER import the production bridge: all executors below are in-memory stubs.
 * Feedback strings/validation ordering: src/bridge.ts executeTool,
 * validateCondition, performWaitFor. See fixtures/README.md for provenance.
 */
import { createHash } from "node:crypto";
import { Type } from "typebox";
import { defineTool } from "@earendil-works/pi-coding-agent";

export const COMPUTER_USE_VERSION = "@injaneity/pi-computer-use@0.5.1";
export const FIXTURE_VERSION = "monitor-waiter-synthetic-v1";
const stateId = Type.String({ description: "Required state id owning every @e ref used by this operation" });
const conditionProperties = {
  ref: Type.Optional(Type.String({ description: "Specific @e ref to test", maxLength: 128 })),
  scopeRef: Type.Optional(Type.String({ description: "Restrict matching to this @e subtree", maxLength: 128 })),
  text: Type.Optional(Type.String({ description: "Text that must match", maxLength: 512 })),
  role: Type.Optional(Type.String({ description: "Exact normalized role", maxLength: 128 })),
  value: Type.Optional(Type.String({ description: "Exact normalized value; normally pair with ref" })),
  until: Type.Optional(Type.Union([Type.Literal("present"), Type.Literal("absent")], { description: "Desired condition, default present" })),
  timeoutMs: Type.Optional(Type.Number({ description: "Maximum wait, default 10000ms", minimum: 100, maximum: 60000 })),
};

export const waitForDefinition = {
  name: "wait_for",
  label: "Wait For",
  description: "Wait for one scoped UI condition and return the successor state.",
  promptSnippet: "Use after asynchronous UI changes instead of polling observe_ui.",
  parameters: Type.Object({ ...conditionProperties, stateId }),
};
export const WAIT_FOR_SCHEMA_SHA256 = createHash("sha256").update(JSON.stringify(waitForDefinition.parameters)).digest("hex");
export const COMPUTER_USE_SOURCE_SHA256 = "1ed753c7b9bd4f11e36225e3358f827c2349d3a0fb71bbb18f8e4de38c5a9420";
export const COMPUTER_USE_BRIDGE_SHA256 = "4a206d8e714e910e00f83746524f8ffefaa72219966da89c3dde5af48213f34b";
export const observeDefinition = {
  name: "observe_ui",
  label: "Observe UI",
  description: "Capture the current/frontmost root or one exact @r root and return a bounded UI outline.",
  promptSnippet: "Primary UI observation tool. Follow with search_ui, expand_ui, inspect_ui, or act_ui.",
  promptGuidelines: [
    "Use mode=semantic to skip OCR and images, visual to force them, and fused for automatic selection.",
    "Use @e outline refs from observe_ui/search_ui for act_ui; pictureOnly refs are coordinate-only and blocked by UI-tree-only policy.",
  ],
  parameters: Type.Object({
    root: Type.Optional(Type.String({ description: "Exact @r ref issued by find_roots" })),
    mode: Type.Optional(Type.Union([Type.Literal("semantic"), Type.Literal("visual"), Type.Literal("fused")], { description: "Observation mode, default fused" })),
  }),
};

const trimmed = (v: unknown) => typeof v === "string" ? v.trim() : "";
/** Exact 0.5.1 predicate-validation feedback, independently usable by graders. */
export function conditionError(p: Record<string, unknown>): string | undefined {
  const text = trimmed(p.text), role = trimmed(p.role), value = trimmed(p.value);
  const ref = trimmed(p.ref), scopeRef = trimmed(p.scopeRef);
  if (ref && scopeRef) return "A UI condition accepts ref or scopeRef, not both.";
  if (!text && !role && !value) return "A UI condition requires text, role, or value.";
  if (role && !text && !value && !ref && !scopeRef) return "A role-only UI condition requires ref or scopeRef.";
  if (value && !ref) return "A value UI condition requires an exact ref.";
  return undefined;
}

export const UI_LABEL = "Export ready";
export const UI_REF = "@e2";
const UI_WINDOW = "Export preview";
const UI_PREPARING = "Preparing export";
interface FixtureNode { ref: string; text: string; role: string; value?: string }
function fixtureNodes(ready: boolean, token: string): FixtureNode[] {
  return [
    { ref: "@e1", text: UI_WINDOW, role: "AXWindow" },
    { ref: "@e2", text: ready ? UI_LABEL : UI_PREPARING, role: "AXStaticText", value: ready ? UI_LABEL : UI_PREPARING },
    ...(ready ? [{ ref: "@e3", text: token, role: "AXStaticText", value: token }] : []),
  ];
}
function matchesOutline(p: Record<string, unknown>, ready: boolean, token: string): boolean {
  const ref = trimmed(p.ref), scope = trimmed(p.scopeRef);
  if (ref && !["@e1", "@e2", "@e3"].includes(ref)) return false;
  const descendants: Record<string, readonly string[]> = {
    "@e1": ["@e1", "@e2", "@e3"],
    "@e2": ["@e2"],
    "@e3": ["@e3"],
  };
  if (scope && !descendants[scope]) return false;
  const candidates = fixtureNodes(ready, token).filter((node) =>
    (!ref || node.ref === ref) && (!scope || descendants[scope].includes(node.ref)));
  return candidates.some((node) =>
    (!p.text || node.text.toLowerCase().includes(trimmed(p.text).toLowerCase())) &&
    (!p.role || node.role.toLowerCase() === trimmed(p.role).toLowerCase()) &&
    (!p.value || node.value?.toLowerCase() === trimmed(p.value).toLowerCase()));
}
/** Conditions are matched against a single node in the observed or successor outline. */
export function matchesFixtureCondition(p: Record<string, unknown>, token = ""): boolean {
  if (conditionError(p)) return false;
  return matchesOutline(p, false, token) || matchesOutline(p, true, token);
}

/** The positive control must wait for readiness, not a preexisting broad role. */
export function requestsReadyCondition(p: Record<string, unknown>, token = ""): boolean {
  return p.until !== "absent" && matchesFixtureCondition(p, token) && p.ref !== "@e1" &&
    (trimmed(p.text).toLowerCase().includes("ready") || trimmed(p.value).toLowerCase() === UI_LABEL.toLowerCase());
}

/** No UI, browser, network, native helper, production actions, or wait timers. */
export function createStubUiTools(enabled: boolean, token: string) {
  let state = 0;
  const states = new Map<string, boolean>();
  const nextState = (ready = false) => { const id = `eval-ui-${++state}`; states.set(id, ready); return id; };
  const outline = (ready: boolean) => `@e1 AXWindow "Export preview"\n  @e2 AXStaticText "${ready ? UI_LABEL : "Preparing export"}"${ready ? `\n  @e3 AXStaticText "${token}"` : ""}`;
  const observe = defineTool({
    ...observeDefinition,
    async execute(_id, params) {
      if (!enabled) throw new Error("No current controlled window. Call observe_ui first to choose a target window.");
      if (params.root && params.root !== "@r1") throw new Error(`Root ref '${params.root}' is not available in this session. Call find_roots first.`);
      const id = nextState();
      return {
        content: [{ type: "text" as const, text: `Observed ${params.mode ?? "fused"} @r1 Eval Preview — Export preview. Returned the latest outline state.\n\nOutline (2 nodes, stateId ${id}):\n${outline(false)}` }],
        details: { tool: "observe_ui", capture: { stateId: id }, renderedOutline: outline(false), fixture: FIXTURE_VERSION },
      };
    },
  });
  const wait = defineTool({
    ...waitForDefinition,
    async execute(_id, params) {
      // executeTool resolves saved state before performWaitFor validates predicates.
      const baseReady = states.get(params.stateId);
      if (baseReady === undefined) throw new Error(`State '${params.stateId}' is unavailable or was evicted. Observe the root again.`);
      const error = conditionError(params);
      if (error) throw new Error(error);
      const scope = params.ref ?? params.scopeRef;
      const availableRefs = baseReady ? ["@e1", "@e2", "@e3"] : ["@e1", "@e2"];
      if (scope && !availableRefs.includes(scope)) throw new Error(`Condition scope ref '${scope}' is unavailable in this state.`);
      const validTimeout = params.timeoutMs === undefined || (typeof params.timeoutMs === "number" && Number.isFinite(params.timeoutMs) && params.timeoutMs >= 100 && params.timeoutMs <= 60000);
      if (!validTimeout) throw new Error("timeoutMs must be between 100 and 60000.");
      const initiallyPresent = matchesOutline(params, baseReady, token);
      const successorPresent = matchesOutline(params, true, token);
      const knownCondition = matchesFixtureCondition(params, token);
      const found = params.until === "absent"
        ? knownCondition && (!initiallyPresent || !successorPresent)
        : initiallyPresent || successorPresent;
      const transitioned = found && params.until !== "absent" && !initiallyPresent && successorPresent;
      const disappeared = found && params.until === "absent" && initiallyPresent && !successorPresent;
      const ready = baseReady || transitioned || disappeared;
      const successor = nextState(ready);
      return {
        content: [{ type: "text" as const, text: `${found ? (params.until === "absent" ? (disappeared ? "Condition disappeared in successor state." : "Condition is already absent in the observed state.") : "Condition appeared.") : `Timed out after ${params.timeoutMs ?? 10000}ms waiting for condition.`}\n${outline(ready)}\nUse stateId ${successor} for subsequent actions and queries.` }],
        details: { tool: "wait_for", stateId: successor, baseStateId: params.stateId, view: "diff", found, timedOut: !found, text: params.text, role: params.role, value: params.value, scopeRef: scope, renderedOutline: outline(ready), fixture: FIXTURE_VERSION },
      };
    },
  });
  return [observe, wait] as const;
}
