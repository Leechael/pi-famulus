import {
  mkdirSync,
  mkdtempSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { BUILTIN_AGENTS } from "../../src/agents/builtins";
import {
  DEFAULT_AGENT_TOOLS,
  parseAgentMarkdown,
  resolveAgent,
} from "../../src/agents/definition";
import { createAgentLoader, loadAgentDefinitions } from "../../src/agents/loader";
import { VALID_THINKING_LEVELS } from "../../src/thinking-levels";

const FULL_MD = `---
name: scout
description: Finds things in the codebase
tools: [read, grep, find]
model: anthropic:claude-haiku-4-5
thinking: high
---

You are a scout agent.

Report what you find.
`;

const MINIMAL_MD = `---
name: scout
description: Finds things
---
`;

function agentMd(name: string, description: string, extra = ""): string {
  const extraLines = extra === "" ? "" : `${extra}\n`;
  return `---\nname: ${name}\ndescription: ${description}\n${extraLines}---\n\nBody of ${name}.\n`;
}

describe("parseAgentMarkdown", () => {
  it("parses all fields including array syntax, model, thinking, and trimmed body", () => {
    const def = parseAgentMarkdown(FULL_MD, "user", "/tmp/scout.md");
    expect(def).toEqual({
      name: "scout",
      description: "Finds things in the codebase",
      tools: ["read", "grep", "find"],
      model: "anthropic:claude-haiku-4-5",
      thinking: "high",
      systemPrompt: "You are a scout agent.\n\nReport what you find.",
      source: "user",
      path: "/tmp/scout.md",
    });
  });

  it("applies defaults: tools default, model/thinking absent, empty body → empty prompt", () => {
    const def = parseAgentMarkdown(MINIMAL_MD, "project");
    expect(def.tools).toEqual([...DEFAULT_AGENT_TOOLS]);
    expect(def.model).toBeUndefined();
    expect(def.thinking).toBeUndefined();
    expect(def.systemPrompt).toBe("");
    expect(def.path).toBeUndefined();
    expect(def.source).toBe("project");
  });

  it("parses quoted values, empty arrays, and strips inline comments", () => {
    const def = parseAgentMarkdown(
      `---\nname: 'quoted-name'\ndescription: "quoted desc"\ntools: []   # no tools at all\n---\nbody`,
      "user",
    );
    expect(def.name).toBe("quoted-name");
    expect(def.description).toBe("quoted desc");
    expect(def.tools).toEqual([]);
    expect(def.systemPrompt).toBe("body");
  });

  it("parses every thinking level", () => {
    for (const level of VALID_THINKING_LEVELS) {
      const def = parseAgentMarkdown(agentMd("a", "d", `thinking: ${level}`), "user");
      expect(def.thinking).toBe(level);
    }
  });

  it.each([
    ["uppercase", "Scout"],
    ["leading digit", "1scout"],
    ["underscore", "my_agent"],
    ["empty", ""],
  ])("rejects invalid name (%s)", (_label, name) => {
    expect(() => parseAgentMarkdown(agentMd(name, "d"), "user", "/tmp/x.md")).toThrow(
      /\/tmp\/x\.md.*name/i,
    );
  });

  it("rejects a missing description", () => {
    expect(() =>
      parseAgentMarkdown(`---\nname: scout\n---\n`, "user", "/tmp/x.md"),
    ).toThrow(/\/tmp\/x\.md.*description/);
  });

  it("rejects an empty description", () => {
    expect(() =>
      parseAgentMarkdown(`---\nname: scout\ndescription: \n---\n`, "user", "/tmp/x.md"),
    ).toThrow(/description.*non-empty/);
  });

  it("rejects an unterminated frontmatter block", () => {
    expect(() =>
      parseAgentMarkdown(`---\nname: scout\ndescription: d\nbody without close`, "user", "/tmp/x.md"),
    ).toThrow(/\/tmp\/x\.md.*unterminated/);
  });

  it("rejects content without a frontmatter block", () => {
    expect(() => parseAgentMarkdown(`no frontmatter here`, "user", "/tmp/x.md")).toThrow(
      /missing frontmatter/,
    );
  });

  it("rejects an invalid thinking level", () => {
    expect(() =>
      parseAgentMarkdown(agentMd("scout", "d", "thinking: turbo"), "user", "/tmp/x.md"),
    ).toThrow(/thinking.*turbo/);
  });

  it("accepts bare comma-separated tools lists", () => {
    const def = parseAgentMarkdown(
      agentMd("scout", "d", "tools: read, grep, find, ls, bash"),
      "user",
      "/tmp/x.md",
    );
    expect(def.tools).toEqual(["read", "grep", "find", "ls", "bash"]);
  });

  it("accepts a single bare tool name", () => {
    const def = parseAgentMarkdown(agentMd("scout", "d", "tools: read"), "user", "/tmp/x.md");
    expect(def.tools).toEqual(["read"]);
  });

  it("rejects half-open tools brackets", () => {
    expect(() =>
      parseAgentMarkdown(agentMd("scout", "d", "tools: [read, grep"), "user", "/tmp/x.md"),
    ).toThrow(/tools.*(array|comma)/);
  });
});

describe("builtins", () => {
  it("ships at least explorer and worker with the contracted tool sets", () => {
    const names = BUILTIN_AGENTS.map((d) => d.name);
    expect(names).toContain("explorer");
    expect(names).toContain("worker");
    const explorer = BUILTIN_AGENTS.find((d) => d.name === "explorer")!;
    const worker = BUILTIN_AGENTS.find((d) => d.name === "worker")!;
    expect(explorer.tools).toEqual(["read", "grep", "find", "ls", "bash"]);
    expect(worker.tools).toEqual(["read", "bash", "edit", "write"]);
    expect(explorer.source).toBe("builtin");
    expect(explorer.path).toBeUndefined();
  });
});

describe("loadAgentDefinitions", () => {
  let root: string;
  let userDir: string;
  let projectDir: string;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "pi-famulus-agents-"));
    userDir = join(root, "user");
    projectDir = join(root, "project");
    mkdirSync(userDir, { recursive: true });
    mkdirSync(projectDir, { recursive: true });
  });

  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
  });

  function write(dir: string, rel: string, content: string): string {
    const path = join(dir, rel);
    mkdirSync(join(path, ".."), { recursive: true });
    writeFileSync(path, content);
    return path;
  }

  it("returns only builtins when both dirs are empty", () => {
    const report = loadAgentDefinitions({ userDir, projectDir });
    expect(report.errors).toEqual([]);
    expect(report.definitions.map((d) => d.name).sort()).toEqual(
      BUILTIN_AGENTS.map((d) => d.name).sort(),
    );
  });

  it("treats missing directories as an empty set, not an error", () => {
    const report = loadAgentDefinitions({
      userDir: join(root, "nope-user"),
      projectDir: join(root, "nope-project"),
    });
    expect(report.errors).toEqual([]);
    expect(report.definitions.length).toBe(BUILTIN_AGENTS.length);
  });

  it("user definition overrides a builtin of the same name", () => {
    write(userDir, "explorer.md", agentMd("explorer", "user override", "tools: [read]"));
    const report = loadAgentDefinitions({ userDir, projectDir });
    const explorer = report.definitions.find((d) => d.name === "explorer")!;
    expect(explorer.description).toBe("user override");
    expect(explorer.tools).toEqual(["read"]);
    expect(explorer.source).toBe("user");
    expect(explorer.path).toBe(join(userDir, "explorer.md"));
  });

  it("project definition overrides a user definition of the same name", () => {
    write(userDir, "scout.md", agentMd("scout", "user version"));
    write(projectDir, "nested/deep/scout.md", agentMd("scout", "project version"));
    const report = loadAgentDefinitions({ userDir, projectDir });
    const scouts = report.definitions.filter((d) => d.name === "scout");
    expect(scouts).toHaveLength(1);
    expect(scouts[0].description).toBe("project version");
    expect(scouts[0].source).toBe("project");
  });

  it("merges differently-named definitions across all three tiers", () => {
    write(userDir, "alpha.md", agentMd("alpha", "from user"));
    write(projectDir, "beta.md", agentMd("beta", "from project"));
    const report = loadAgentDefinitions({ userDir, projectDir });
    const names = report.definitions.map((d) => d.name);
    expect(names).toEqual(expect.arrayContaining(["explorer", "worker", "alpha", "beta"]));
  });

  it("recurses into nested directories", () => {
    write(userDir, "a/b/c/deep.md", agentMd("deep", "nested file"));
    const report = loadAgentDefinitions({ userDir, projectDir });
    expect(report.definitions.map((d) => d.name)).toContain("deep");
  });

  it("collects parse errors without aborting the load", () => {
    const badPath = write(userDir, "broken.md", `---\nname: Broken\n---\n`);
    write(userDir, "good.md", agentMd("good", "loads fine"));
    write(projectDir, "also-good.md", agentMd("also-good", "loads fine too"));
    const report = loadAgentDefinitions({ userDir, projectDir });
    expect(report.errors).toHaveLength(1);
    expect(report.errors[0].path).toBe(badPath);
    expect(report.errors[0].error).toMatch(/name/);
    const names = report.definitions.map((d) => d.name);
    expect(names).toEqual(expect.arrayContaining(["good", "also-good", "worker"]));
  });
});

describe("createAgentLoader (mtime cache)", () => {
  let root: string;
  let userDir: string;
  let projectDir: string;

  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "pi-famulus-agents-cache-"));
    userDir = join(root, "user");
    projectDir = join(root, "project");
    mkdirSync(userDir, { recursive: true });
    mkdirSync(projectDir, { recursive: true });
  });

  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
  });

  it("returns the identical report object when nothing changed", () => {
    writeFileSync(join(userDir, "a.md"), agentMd("alpha", "v1"));
    const loader = createAgentLoader({ userDir, projectDir });
    const first = loader.reload();
    const second = loader.reload();
    expect(second).toBe(first);
  });

  it("re-parses after a file's mtime changes", () => {
    const path = join(userDir, "a.md");
    writeFileSync(path, agentMd("alpha", "v1"));
    const loader = createAgentLoader({ userDir, projectDir });
    const first = loader.reload();
    expect(first.definitions.find((d) => d.name === "alpha")!.description).toBe("v1");

    writeFileSync(path, agentMd("alpha", "v2"));
    // Force a distinct mtime so the fingerprint changes regardless of fs granularity.
    const future = new Date("2100-01-01T00:00:00.000Z");
    utimesSync(path, future, future);

    const second = loader.reload();
    expect(second).not.toBe(first);
    expect(second.definitions.find((d) => d.name === "alpha")!.description).toBe("v2");
  });

  it("re-parses when a file is added or removed", () => {
    const loader = createAgentLoader({ userDir, projectDir });
    const first = loader.reload();

    const path = join(userDir, "new.md");
    writeFileSync(path, agentMd("newbie", "added"));
    const second = loader.reload();
    expect(second).not.toBe(first);
    expect(second.definitions.map((d) => d.name)).toContain("newbie");

    rmSync(path);
    const third = loader.reload();
    expect(third).not.toBe(second);
    expect(third.definitions.map((d) => d.name)).not.toContain("newbie");
  });
});

describe("resolveAgent", () => {
  const defs = loadAgentDefinitions({
    userDir: join(tmpdir(), "pi-famulus-resolve-nope-user"),
    projectDir: join(tmpdir(), "pi-famulus-resolve-nope-project"),
  }).definitions;

  it("returns the worker definition when name is undefined", () => {
    expect(resolveAgent(defs, undefined).name).toBe("worker");
  });

  it("returns the named definition", () => {
    expect(resolveAgent(defs, "explorer").name).toBe("explorer");
  });

  it("throws for unknown names and lists all available names", () => {
    expect(() => resolveAgent(defs, "nope")).toThrowError(
      /Unknown agent "nope"\. Available agents: .*explorer.*worker/,
    );
  });

  it("throws when the default worker is missing", () => {
    expect(() => resolveAgent([], undefined)).toThrow(/Default agent "worker"/);
  });
});
