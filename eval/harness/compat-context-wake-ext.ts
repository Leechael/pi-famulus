/** Test-only SDK extension that injects a synthetic wake without a manager/process. */
import { Type } from "typebox";
import { defineTool, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { FAMULUS_WAKE_CUSTOM_TYPE, FAMULUS_WAKE_LEAD_IN } from "../lib/wake-adapter.ts";
import { FIXTURE_VERSION } from "../ablation/fixtures/computer-use-0.5.1.ts";

export default function compatibilityWakeFixture(pi: ExtensionAPI): void {
  if (process.env.PI_FAMULUS_COMPAT_FIXTURE !== FIXTURE_VERSION) return;
  pi.registerTool(defineTool({
    name: "emit_compat_wake",
    label: "Emit Synthetic Compatibility Wake",
    description: "Test-only faux SDK wake delivery; no manager or subprocess.",
    parameters: Type.Object({}),
    async execute() {
      await pi.sendMessage({
        customType: FAMULUS_WAKE_CUSTOM_TYPE,
        content: `${FAMULUS_WAKE_LEAD_IN}\n\n<pi-famulus-wake kind="monitor" id="compat-fixture" description="Synthetic compatibility fixture"><event>SDK faux continuation</event></pi-famulus-wake>`,
        details: { kind: "monitor", id: "compat-fixture", status: "event", event: "SDK faux continuation" },
        display: true,
      }, { triggerTurn: true, deliverAs: "followUp" });
      return { content: [{ type: "text" as const, text: "Queued one synthetic SDK wake." }], details: {} };
    },
  }));
}
