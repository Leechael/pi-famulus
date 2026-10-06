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
function matchesOutline(p: Record<string, unknown>, ready: boolean): boolean {
  const ref = trimmed(p.ref), scope = trimmed(p.scopeRef);
  if (ref && !["@e1", "@e2", "@e3"].includes(ref)) return false;
  if (scope && scope !== "@e1") return false;
  const label = ready ? UI_LABEL : UI_PREPARING;
  const nodes = ref === "@e1" ? [{ text: UI_WINDOW, roles: ["window", "axwindow"] }]
    : ref ? [{ text: label, roles: ["statictext", "axstatictext"] }]
    : [{ text: UI_WINDOW, roles: ["window", "axwindow"] }, { text: label, roles: ["statictext", "axstatictext"] }];
  if (!nodes.some((node) => (!p.text || node.text.toLowerCase().includes(trimmed(p.text).toLowerCase())) &&
    (!p.role || node.roles.includes(trimmed(p.role).toLowerCase())))) return false;
  if (p.value && (!ready || trimmed(p.value).toLowerCase() !== UI_LABEL.toLowerCase())) return false;
  return true;
}
/** Conditions are checked against the observed preparing state and its ready successor. */
export function matchesFixtureCondition(p: Record<string, unknown>): boolean {
  if (conditionError(p)) return false;
  // A predicate on either returned outline is fixture-grounded, even when its
  // legitimate wait times out or is already satisfied on the observed state.
  return matchesOutline(p, false) || matchesOutline(p, true);
}
function fixtureConditionFound(p: Record<string, unknown>): boolean {
  if (p.until !== "absent") return matchesOutline(p, false) || matchesOutline(p, true);
  const initiallyPresent = matchesOutline(p, false);
  const successorPresent = matchesOutline(p, true);
  // Already absent known nodes succeed against observation; a present node must
  // actually disappear in the ready successor. Stable nodes time out normally.
  return (!initiallyPresent && successorPresent) || (initiallyPresent && !successorPresent);
}

/** The positive control must wait for readiness, not a preexisting broad role. */
export function requestsReadyCondition(p: Record<string, unknown>): boolean {
  return p.until !== "absent" && matchesFixtureCondition(p) && p.ref !== "@e1" &&
    (trimmed(p.text).toLowerCase().includes("ready") || trimmed(p.value).toLowerCase() === UI_LABEL.toLowerCase());
}

/** No UI, browser, network, native helper, production actions, or wait timers. */
export function createStubUiTools(enabled: boolean, token: string) {
  let state = 0;
  const states = new Set<string>();
  const nextState = () => { const id = `eval-ui-${++state}`; states.add(id); return id; };
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
      if (!states.has(params.stateId)) throw new Error(`State '${params.stateId}' is unavailable or was evicted. Observe the root again.`);
      const error = conditionError(params);
      if (error) throw new Error(error);
      const scope = params.ref ?? params.scopeRef;
      if (scope && !["@e1", "@e2", "@e3"].includes(scope)) throw new Error(`Condition scope ref '${scope}' is unavailable in this state.`);
      const validTimeout = params.timeoutMs === undefined || (typeof params.timeoutMs === "number" && Number.isFinite(params.timeoutMs) && params.timeoutMs >= 100 && params.timeoutMs <= 60000);
      if (!validTimeout) throw new Error("timeoutMs must be between 100 and 60000.");
      const found = fixtureConditionFound(params);
      const transitioned = params.until === "absent" && matchesOutline(params, false) && !matchesOutline(params, true);
      const ready = transitioned || requestsReadyCondition(params);
      const successor = nextState();
      return {
        content: [{ type: "text" as const, text: `${found ? (transitioned ? "Condition disappeared in successor state." : "Condition is already absent in the observed state.") : `Timed out after ${params.timeoutMs ?? 10000}ms waiting for condition.`}\n${outline(ready)}\nUse stateId ${successor} for subsequent actions and queries.` }],
        details: { tool: "wait_for", stateId: successor, baseStateId: params.stateId, view: "diff", found, timedOut: !found, text: params.text, role: params.role, value: params.value, scopeRef: scope, renderedOutline: outline(ready), fixture: FIXTURE_VERSION },
      };
    },
  });
  return [observe, wait] as const;
}
