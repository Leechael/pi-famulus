/**
 * The local bash fallback used to keep every output chunk, Buffer.concat them
 * on exit, and writeFileSync the whole string: at 250MB of output that blocked
 * the event loop for ~5.6s and grew RSS to ~2GB. BoundedOutput keeps only a
 * bounded tail in memory and streams the full output to a temp file.
 */
import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { BoundedOutput } from "../../src/bounded-output";

describe("BoundedOutput", () => {
  it("returns small output whole, without a spill file", async () => {
    const o = new BoundedOutput({ maxBytes: 100, maxLines: 10, tailBytes: 1000 });
    o.append(Buffer.from("a\nb\n"));
    const r = await o.finish();
    expect(r).toMatchObject({ text: "a\nb\n", headDropped: false, totalBytes: 4, totalLines: 3 });
    expect(r.fullOutputPath).toBeUndefined();
  });

  it("bounds memory to the tail and spills the full output to a file", async () => {
    const o = new BoundedOutput({ maxBytes: 50, maxLines: 1000, tailBytes: 64 });
    let full = "";
    for (let i = 0; i < 500; i++) {
      const line = `line ${i}\n`;
      full += line;
      o.append(Buffer.from(line));
    }
    const r = await o.finish();
    expect(r.headDropped).toBe(true);
    expect(Buffer.byteLength(r.text)).toBeLessThanOrEqual(64);
    expect(full.endsWith(r.text)).toBe(true);
    expect(r.totalBytes).toBe(Buffer.byteLength(full));
    expect(r.totalLines).toBe(full.split("\n").length);
    expect(readFileSync(r.fullOutputPath!, "utf8")).toBe(full);
  });

  it("spills on line count even under the byte limit", async () => {
    const o = new BoundedOutput({ maxBytes: 1e6, maxLines: 3, tailBytes: 1e6 });
    o.append(Buffer.from("1\n2\n3\n4\n"));
    const r = await o.finish();
    expect(readFileSync(r.fullOutputPath!, "utf8")).toBe("1\n2\n3\n4\n");
  });

  it("never starts the tail mid UTF-8 character", async () => {
    const o = new BoundedOutput({ maxBytes: 10, maxLines: 100, tailBytes: 7 });
    o.append(Buffer.from("ééééééééé"));
    const r = await o.finish();
    expect(r.text).not.toContain("\uFFFD");
    expect(/^é+$/.test(r.text)).toBe(true);
  });
});
