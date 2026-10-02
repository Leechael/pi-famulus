/**
 * Optional LLM judge (--judge <model>) for fuzzy criteria only. Uses pi's own
 * auth via `pi -p` with no tools; returns true/false, or null when the
 * verdict cannot be parsed. Programmatic graders stay authoritative.
 */
import { spawn } from "node:child_process";
import { piCommand } from "../lib/paths.ts";

export async function judge(model: string, question: string, excerpt: string): Promise<boolean | null> {
  const prompt =
    `${question}\n\nAnswer with exactly one word: YES or NO.\n\n<transcript>\n${excerpt.slice(-12_000)}\n</transcript>`;
  const out = await new Promise<string>((resolve) => {
    const [bin, args] = piCommand(["-p", "-ne", "-ns", "-np", "-nc", "-nt", "--no-session", "--offline", "--model", model, prompt]);
    const p = spawn(bin, args, {
      stdio: ["ignore", "pipe", "ignore"],
    });
    let text = "";
    p.stdout.setEncoding("utf8");
    p.stdout.on("data", (d: string) => (text += d));
    const timer = setTimeout(() => p.kill("SIGKILL"), 120_000);
    p.on("close", () => {
      clearTimeout(timer);
      resolve(text);
    });
  });
  const m = /\b(YES|NO)\b/i.exec(out.trim().split("\n").at(-1) ?? "");
  return m ? m[1].toUpperCase() === "YES" : null;
}
