# Prompts

Every text a model sees from this extension lives here, one file per owner: a tool (`tools/`, child-session tools under `tools/child/`), the system-prompt guidelines, the builtin agents, and the fixed texts inside wakes. [INDEX.md](INDEX.md) lists every prompt id, its kind, and its ablatable segments.

pi loads the extension's TypeScript directly, so these files are compiled into `src/prompts.generated.ts` (committed). After editing, run:

```bash
npm run prompts
```

A test fails if the generated module or the index is stale.

## Format

```markdown
# description            ← prompt id <file path>.description, e.g. tools.bash.description
Text up to the next "# " heading; leading and trailing blank lines are dropped.

# rules (list)           ← a string[]: one "- " item per entry
- First rule.

# result: backgrounded   ← "result: backgrounded" → id tools.bash.result.backgrounded
Command "{{command}}" …  ← {{name}} placeholders, filled in code with fill(id, vars)
```

- `<!--seg:segment.id-->…<!--/seg-->` marks a span the eval can remove (`eval/ablation/manifest.json` refers to it by id). The markers are not part of the text.
- No line may end in whitespace: editors strip it and the text would change silently. Spaces between pieces that code joins belong in the code.
- Changing a prompt changes what models see: rerun the eval (see `eval/README.md`).
