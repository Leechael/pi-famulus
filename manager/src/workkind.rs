//! What kind of work a shell command does, guessed from its text: the
//! `work_kind` of `ls --json`, `show` and `stats --by kind`.
//!
//! Commands written by agents are rarely one program. They are `cd X &&
//! export PATH=… && pdm run python -c '<check>' && pdm run test > log 2>&1;
//! rc=$?; tail -n 100 log; exit $rc`, often wrapped in `bash -o pipefail -c
//! '…'`, sometimes with a `python - <<'PY'` heredoc. So the command is cut
//! into simple commands at the top level (`;` `&&` `||` `|` `&` newline,
//! outside quotes, `$(…)` and heredoc bodies), `sh/bash -c '…'` is
//! classified recursively, setup noise (assignments, `cd`, `export`,
//! `test`, `printf`, …) is dropped, wrappers (`pdm run`, `uv run`, `npx`,
//! `xargs`, `env`, `timeout N`, `python -m`, a path before the program
//! name) are peeled, and the heaviest remaining simple command decides:
//! test-suite > test > build > lint/type > other > git > read/search.
//! "Heaviest", not "last": the `tail` that prints a suite's log must not
//! turn the suite into a read.
//!
//! A test runner with no target is a whole suite (`pdm run test -n 4`,
//! `cargo test`, `npm test`); a path, a node id, `-k`/`-m`/`--lf`, a `$`
//! expansion standing in for targets, or targets fed by `xargs` make it
//! targeted (`test`), in any spelling (`-k auth`, `-k=auth`, `-kauth`,
//! `--test=name`), through any number of wrappers after the `xargs`.
//! A runner's own option values (`npm --prefix web`, `uv run --directory
//! backend`, `make -C dir`) are skipped, never taken as the program.
//!
//! Two deliberate calls (owner decision on #35): a script whose name
//! contains `compile` (`pdm run py-compile`) is `build`, though it only
//! checks syntax; `awk` is `read/search`, though it can compute, because
//! agents use it to slice logs and listings.
//!
//! Computed when read, never stored: a better rule applies to old records
//! too, and nothing on the wire depends on it.
//!
//! deferred: per-project override rules (e.g. `.pi/famulus-kinds.json`
//! mapping a project's own scripts like `pdm run ci-fast` to a kind) |
//! impact: a project script whose name says nothing (`pdm run go`,
//! `./run.sh`) lands in `other`, so `stats --by kind` under-reports that
//! project's test or build cost; no record or behaviour is wrong |
//! trigger: a real run where `other` holds a large share of CPU or wall
//! time that a person can attribute to a known script.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkKind {
    ReadSearch,
    Git,
    Other,
    LintType,
    Build,
    Test,
    TestSuite,
}

impl WorkKind {
    pub fn label(self) -> &'static str {
        match self {
            WorkKind::TestSuite => "test-suite",
            WorkKind::Test => "test",
            WorkKind::Build => "build",
            WorkKind::LintType => "lint/type",
            WorkKind::Other => "other",
            WorkKind::Git => "git",
            WorkKind::ReadSearch => "read/search",
        }
    }
}

/// The kind of the heaviest simple command in `command`.
pub fn classify(command: &str) -> WorkKind {
    classify_depth(command, 0).unwrap_or(WorkKind::Other)
}

/// `None` when every simple command is setup noise.
fn classify_depth(command: &str, depth: usize) -> Option<WorkKind> {
    split_simple_commands(command)
        .iter()
        .filter_map(|words| simple_kind(words, depth))
        .max()
}

// ---------------------------------------------------------------------------
// Lexing: top-level simple commands as unquoted words
// ---------------------------------------------------------------------------

/// Cut `src` into simple commands, each a list of words with quotes
/// removed. Separators count only at the top level: inside quotes, `$(…)`,
/// backticks and `${…}` they are part of the word. Redirections and their
/// targets are dropped; heredoc bodies are skipped.
// The final end_command! flush resets lexer state nothing reads afterwards.
#[allow(unused_assignments)]
fn split_simple_commands(src: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    // Heredoc delimiters seen on the current line, skipped at its end.
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut drop_next_word = false;
    let mut pending_heredoc: Option<bool> = None; // Some(strip_tabs)
    let mut i = 0;

    macro_rules! end_word {
        () => {
            if in_word {
                let w = std::mem::take(&mut word);
                in_word = false;
                if let Some(strip) = pending_heredoc.take() {
                    heredocs.push((w, strip));
                } else if drop_next_word {
                    drop_next_word = false;
                } else {
                    words.push(w);
                }
            }
        };
    }
    macro_rules! end_command {
        () => {
            end_word!();
            if !words.is_empty() {
                out.push(std::mem::take(&mut words));
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                if chars[i + 1] != '\n' {
                    word.push(chars[i + 1]);
                    in_word = true;
                }
                i += 2;
                continue;
            }
            '\'' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
                continue;
            }
            '"' => {
                in_word = true;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() && matches!(chars[i + 1], '"' | '\\' | '$' | '`') {
                        i += 1;
                    }
                    word.push(chars[i]);
                    i += 1;
                }
                i += 1;
                continue;
            }
            '$' if i + 1 < chars.len() && matches!(chars[i + 1], '(' | '{') => {
                // Keep `$(…)` / `${…}` inside the word, balanced.
                let (open, close) = if chars[i + 1] == '(' { ('(', ')') } else { ('{', '}') };
                in_word = true;
                word.push('$');
                word.push(open);
                i += 2;
                let mut depth = 1;
                while i < chars.len() && depth > 0 {
                    let ch = chars[i];
                    if ch == '\'' {
                        // Quoted text inside a substitution: copy through.
                        word.push(ch);
                        i += 1;
                        while i < chars.len() && chars[i] != '\'' {
                            word.push(chars[i]);
                            i += 1;
                        }
                    } else if ch == open {
                        depth += 1;
                    } else if ch == close {
                        depth -= 1;
                    }
                    if i < chars.len() {
                        word.push(chars[i]);
                    }
                    i += 1;
                }
                continue;
            }
            '`' => {
                in_word = true;
                word.push('`');
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    word.push(chars[i]);
                    i += 1;
                }
                word.push('`');
                i += 1;
                continue;
            }
            '#' if !in_word => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '\n' => {
                end_command!();
                i += 1;
                // Skip the bodies of heredocs opened on this line.
                for (delim, strip) in std::mem::take(&mut heredocs) {
                    loop {
                        let start = i;
                        while i < chars.len() && chars[i] != '\n' {
                            i += 1;
                        }
                        let line: String = chars[start..i].iter().collect();
                        i = (i + 1).min(chars.len());
                        let line = if strip { line.trim_start_matches('\t') } else { line.as_str() };
                        if line == delim || i >= chars.len() {
                            break;
                        }
                    }
                }
                continue;
            }
            ';' | '|' | '&' => {
                // `&>` / `&>>` redirect both streams: a redirection.
                if c == '&' && chars.get(i + 1) == Some(&'>') {
                    end_word!();
                    i += 2;
                    if chars.get(i) == Some(&'>') {
                        i += 1;
                    }
                    drop_next_word = true;
                    continue;
                }
                end_command!();
                i += 1;
                while i < chars.len() && matches!(chars[i], ';' | '|' | '&') {
                    i += 1;
                }
                continue;
            }
            '(' | ')' if !in_word => {
                end_command!();
                i += 1;
                continue;
            }
            '<' | '>' => {
                // A digit word right before (`2>`) is the fd, not a word.
                if in_word && word.chars().all(|d| d.is_ascii_digit()) {
                    word.clear();
                    in_word = false;
                } else {
                    end_word!();
                }
                if c == '<' && chars.get(i + 1) == Some(&'<') {
                    if chars.get(i + 2) == Some(&'<') {
                        i += 3; // here-string: drop the word
                        drop_next_word = true;
                        continue;
                    }
                    i += 2;
                    let strip = chars.get(i) == Some(&'-');
                    if strip {
                        i += 1;
                    }
                    pending_heredoc = Some(strip);
                    continue;
                }
                i += 1;
                while i < chars.len() && matches!(chars[i], '>' | '<' | '|') {
                    i += 1;
                }
                if chars.get(i) == Some(&'&') {
                    // `2>&1`, `>&2`: the target is an fd.
                    i += 1;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '-') {
                        i += 1;
                    }
                    continue;
                }
                drop_next_word = true;
                continue;
            }
            c if c.is_whitespace() => {
                end_word!();
                i += 1;
                continue;
            }
            _ => {
                word.push(c);
                in_word = true;
                i += 1;
            }
        }
    }
    end_command!();
    out
}

// ---------------------------------------------------------------------------
// One simple command
// ---------------------------------------------------------------------------

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

fn base(w: &str) -> &str {
    Path::new(w).file_name().and_then(|s| s.to_str()).unwrap_or(w)
}

/// Shell keywords and setup commands that do no work of their own.
const NOISE: &[&str] = &[
    "cd", "pushd", "popd", "export", "unset", "set", "shopt", "test", "[", "[[", "]]", "]", "true", "false", ":",
    "printf", "echo", "exit", "return", "sleep", "mkdir", "rm", "rmdir", "cp", "mv", "touch", "ln", "chmod", "chown",
    "tee", "date", "wait", "trap", "source", ".", "local", "declare", "typeset", "read", "shift", "let", "mapfile",
    "readarray", "ulimit", "umask", "alias", "kill", "pkill", "for", "case", "esac", "in", "}", "fi",
    "done", "eval", "break", "continue", "select", "function",
];
/// Words that open a compound command; the command follows them.
const LEADERS: &[&str] = &["if", "then", "else", "elif", "do", "while", "until", "!", "time", "{"];

const READ: &[&str] = &[
    "rg", "grep", "egrep", "fgrep", "ag", "ack", "cat", "bat", "less", "more", "head", "tail", "sed", "awk", "gawk",
    "find", "fd", "ls", "tree", "wc", "sort", "uniq", "cut", "tr", "diff", "cmp", "stat", "file", "du", "df", "readlink",
    "realpath", "basename", "dirname", "pwd", "jq", "yq", "xxd", "od", "hexdump", "nl", "column", "comm", "which",
    "type", "whereis", "md5sum", "sha256sum", "shasum", "paste", "fold", "rev", "strings",
];
const LINT: &[&str] = &[
    "ruff", "mypy", "pyright", "basedpyright", "eslint", "prettier", "black", "isort", "flake8", "pylint", "biome",
    "oxlint", "shellcheck", "shfmt", "prek", "pre-commit", "rustfmt", "golangci-lint", "gofmt", "actionlint",
    "hadolint", "markdownlint", "codespell", "typos", "stylelint", "ty",
];
const BUILD: &[&str] = &[
    "gcc", "g++", "clang", "clang++", "cc", "c++", "rustc", "cmake", "ninja", "meson", "gradle", "javac", "swiftc",
    "webpack", "vite", "esbuild", "rollup", "tsup",
];
const TEST: &[&str] = &[
    "pytest", "py.test", "unittest", "vitest", "jest", "mocha", "ava", "tox", "nox", "rspec", "ctest", "phpunit",
    "behave",
];
/// `pdm run X` style runners: X is a script name or a program.
const SCRIPT_RUNNERS: &[&str] = &["pdm", "uv", "poetry", "pipenv", "hatch", "rye", "npm", "pnpm", "yarn", "bun", "just", "task", "make"];

fn simple_kind(words: &[String], depth: usize) -> Option<WorkKind> {
    simple_kind_fed(words, depth, false)
}

/// `fed_by_xargs`: an `xargs` further out supplies arguments on stdin, so
/// a test runner reached through more wrappers (`xargs uv run python -m
/// pytest`) is still a targeted run.
fn simple_kind_fed(words: &[String], depth: usize, fed_by_xargs: bool) -> Option<WorkKind> {
    let mut w: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut fed_by_xargs = fed_by_xargs;
    loop {
        // Assignments and compound-command leaders before the program.
        while let Some(first) = w.first() {
            if is_assignment(first) || LEADERS.contains(first) {
                w.remove(0);
            } else {
                break;
            }
        }
        let Some(&prog) = w.first() else { return None };
        let prog = base(prog);
        let rest = &w[1..];
        match prog {
            "env" => {
                w = rest.iter().copied().skip_while(|a| a.starts_with('-') || is_assignment(a)).collect();
            }
            "nohup" | "exec" | "command" | "builtin" | "sudo" | "stdbuf" => {
                w = rest.iter().copied().skip_while(|a| a.starts_with('-')).collect();
            }
            "nice" | "ionice" => w = skip_flags_with_values(rest, &["-n", "-c"]),
            "timeout" | "gtimeout" => {
                let r = skip_flags_with_values(rest, &["-s", "-k", "--signal", "--kill-after"]);
                w = r.into_iter().skip(1).collect(); // the duration
            }
            "xargs" => {
                fed_by_xargs = true;
                w = skip_flags_with_values(rest, &["-I", "-n", "-P", "-L", "-d", "-a", "-s", "-E", "--max-procs", "--delimiter"]);
            }
            "npx" | "bunx" | "pnpx" => w = rest.iter().copied().skip_while(|a| a.starts_with('-')).collect(),
            "sh" | "bash" | "zsh" | "dash" | "ksh" => {
                // `bash [-o opt] [-e] -c '<script>'`: classify the script.
                let mut i = 0;
                while i < rest.len() {
                    let a = rest[i];
                    if a == "-o" || a == "+o" {
                        i += 2;
                        continue;
                    }
                    if a.starts_with('-') && a.contains('c') && !a.starts_with("--") {
                        return match rest.get(i + 1) {
                            Some(script) if depth < 8 => classify_depth(script, depth + 1),
                            _ => Some(WorkKind::Other),
                        };
                    }
                    if !a.starts_with('-') && !a.starts_with('+') {
                        break;
                    }
                    i += 1;
                }
                return Some(WorkKind::Other); // a script file
            }
            p if p.starts_with("python") && rest.first() == Some(&"-m") && rest.len() > 1 => {
                w = rest[1..].to_vec();
            }
            _ => return program_kind(prog, rest, fed_by_xargs),
        }
    }
}

/// Drop leading flags (and the value of those listed in `with_value`).
fn skip_flags_with_values<'a>(args: &[&'a str], with_value: &[&str]) -> Vec<&'a str> {
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        if with_value.contains(&args[i]) {
            i += 1;
        }
        i += 1;
    }
    args[i.min(args.len())..].to_vec()
}

fn program_kind(prog: &str, args: &[&str], fed_by_xargs: bool) -> Option<WorkKind> {
    if NOISE.contains(&prog) {
        return None;
    }
    if TEST.contains(&prog) {
        return Some(test_kind(args, fed_by_xargs));
    }
    if SCRIPT_RUNNERS.contains(&prog) {
        return Some(runner_kind(prog, args, fed_by_xargs));
    }
    if READ.contains(&prog) {
        return Some(WorkKind::ReadSearch);
    }
    if LINT.contains(&prog) {
        return Some(WorkKind::LintType);
    }
    if BUILD.contains(&prog) {
        return Some(WorkKind::Build);
    }
    let sub = args.iter().copied().find(|a| !a.starts_with('-'));
    Some(match prog {
        "git" | "gh" | "tig" => WorkKind::Git,
        "cargo" => match sub {
            Some("test" | "nextest") => cargo_test_kind(args),
            Some("clippy" | "check" | "fmt") => WorkKind::LintType,
            Some("build" | "install" | "doc" | "fetch" | "b") => WorkKind::Build,
            _ => WorkKind::Other,
        },
        "go" => match sub {
            Some("test") => {
                let targeted = args.iter().any(|a| *a == "-run")
                    || args.iter().skip(1).any(|a| !a.starts_with('-') && *a != "./..." && looks_like_target(a));
                if targeted { WorkKind::Test } else { WorkKind::TestSuite }
            }
            Some("vet") => WorkKind::LintType,
            Some("build" | "install" | "mod") => WorkKind::Build,
            _ => WorkKind::Other,
        },
        "tsc" => {
            if args.iter().any(|a| *a == "--noEmit") {
                WorkKind::LintType
            } else {
                WorkKind::Build
            }
        }
        "pip" | "pip3" => match sub {
            Some("install" | "download" | "wheel") => WorkKind::Build,
            _ => WorkKind::Other,
        },
        "docker" | "podman" => match sub {
            Some("build" | "buildx") => WorkKind::Build,
            _ => WorkKind::Other,
        },
        "node" | "deno" if args.iter().any(|a| *a == "--test" || *a == "test") => test_kind(args, fed_by_xargs),
        "mvn" | "mvnw" => {
            if args.iter().any(|a| *a == "test" || *a == "verify") {
                WorkKind::TestSuite
            } else {
                WorkKind::Build
            }
        }
        _ => wrapped_kind(prog, args, fed_by_xargs).unwrap_or(WorkKind::Other),
    })
}

/// An unknown program whose arguments start a known heavy one: a shell
/// function or script used as a wrapper (`run_check fmt pdm run
/// fmt-check`, `retry 3 cargo test`). Interpreters are excluded: their
/// arguments are code or scripts, not commands.
fn wrapped_kind(prog: &str, args: &[&str], fed_by_xargs: bool) -> Option<WorkKind> {
    if ["python", "node", "ruby", "perl", "deno", "bun"].iter().any(|p| prog.starts_with(p)) {
        return None;
    }
    let heavy = |a: &str| {
        TEST.contains(&a) || SCRIPT_RUNNERS.contains(&a) || LINT.contains(&a) || BUILD.contains(&a) || matches!(a, "cargo" | "go" | "tsc")
    };
    let at = args.iter().position(|a| heavy(base(a)))?;
    let words: Vec<String> = args[at..].iter().map(|s| s.to_string()).collect();
    simple_kind_fed(&words, 0, fed_by_xargs).filter(|k| *k > WorkKind::Other)
}

/// `pdm run X`, `npm run X`, `make X`, …: X is a script/target name or a
/// program to run.
fn runner_kind(runner: &str, args: &[&str], fed_by_xargs: bool) -> WorkKind {
    // Runner's own options before the subcommand (`npm --prefix web run`,
    // `make -C backend test`), values included.
    let args = skip_runner_opts(runner, args);
    let sub = args.first().copied();
    let Some(sub) = sub else {
        return if runner == "make" || runner == "just" { WorkKind::Build } else { WorkKind::Other };
    };
    let after: Vec<&str> = args[1..].to_vec();
    match (runner, sub) {
        ("npm" | "pnpm" | "yarn" | "bun", "test" | "t") => return test_kind(&after, fed_by_xargs),
        ("npm" | "pnpm" | "yarn" | "bun" | "pdm" | "poetry" | "pipenv", "ci" | "install" | "i" | "add" | "sync" | "lock" | "update") => {
            return WorkKind::Build
        }
        ("uv", "sync" | "lock" | "pip" | "add" | "build") | ("pdm" | "poetry" | "hatch" | "rye", "build") => return WorkKind::Build,
        ("npm" | "pnpm" | "yarn" | "bun", "exec" | "dlx" | "x") => {
            let rest: Vec<String> = after.iter().map(|s| s.to_string()).collect();
            return simple_kind_fed(&rest, 0, fed_by_xargs).unwrap_or(WorkKind::Other);
        }
        _ => {}
    }
    // `run X` (or `make X`, `just X`, `pnpm X`): X names a script or a program.
    let (name, script_args): (&str, Vec<&str>) = if sub == "run" {
        // `uv run --directory backend pytest`: `run`'s options, values included.
        let a = skip_runner_opts(runner, &after);
        match a.first() {
            Some(n) => (*n, a[1..].to_vec()),
            None => return WorkKind::Other,
        }
    } else {
        (sub, after)
    };
    if let Some(k) = script_name_kind(name, &script_args, fed_by_xargs) {
        return k;
    }
    // Not a recognisable script name: perhaps a program (`pdm run pytest`
    // matched above, `uv run ruff`, `pdm run python -m mypy`).
    let mut words = vec![name.to_string()];
    words.extend(script_args.iter().map(|s| s.to_string()));
    match simple_kind_fed(&words, 0, fed_by_xargs) {
        Some(k) => k,
        None if runner == "make" || runner == "just" => WorkKind::Build,
        None => WorkKind::Other,
    }
}

/// Options of a script runner that take a separate value, before its
/// subcommand or after `run`. The `--opt=value` and attached short forms are
/// one word and need no entry.
fn runner_value_opts(runner: &str) -> &'static [&'static str] {
    match runner {
        "npm" => &["--prefix", "-w", "--workspace", "--userconfig", "--cache", "--registry", "--loglevel"],
        "pnpm" => &["-C", "--dir", "--filter", "-F", "--workspace-dir", "--reporter", "--loglevel"],
        "yarn" => &["--cwd", "--cache-folder", "--modules-folder"],
        "bun" => &["--cwd", "-c", "--config", "--filter"],
        "uv" => &[
            "--directory", "--project", "--python", "-p", "--with", "--with-requirements", "--with-editable",
            "--package", "--env-file", "--extra", "--group", "--only-group", "--no-group", "--index", "--index-url",
            "--cache-dir", "--config-file",
        ],
        "pdm" => &["-p", "--project", "-c", "--config"],
        "poetry" => &["-C", "--directory", "-P", "--project"],
        "pipenv" => &["--python"],
        "hatch" => &["-e", "--env", "-p", "--project"],
        "rye" => &["--pyproject"],
        "make" => &["-C", "--directory", "-f", "--file", "--makefile", "-j", "--jobs", "-l", "--load-average", "-o", "-W"],
        "just" => &["-f", "--justfile", "-d", "--working-directory"],
        "task" => &["-d", "--dir", "-t", "--taskfile"],
        _ => &[],
    }
}

/// Drop a runner's leading options and the values of those that take one.
fn skip_runner_opts<'a>(runner: &str, args: &[&'a str]) -> Vec<&'a str> {
    let with_value = runner_value_opts(runner);
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        let a = args[i];
        // make's `-j`/`-l` take an optional number: `make -j test` has none.
        let optional = runner == "make" && matches!(a, "-j" | "--jobs" | "-l" | "--load-average");
        let has_value = with_value.contains(&a)
            && (!optional || args.get(i + 1).is_some_and(|n| n.chars().all(|c| c.is_ascii_digit() || c == '.')));
        i += if has_value { 2 } else { 1 };
    }
    args[i.min(args.len())..].to_vec()
}

/// A script or make target by its name (`test`, `type-check`,
/// `fmt-check`, `py-compile`, `lint:fix`).
fn script_name_kind(name: &str, args: &[&str], fed_by_xargs: bool) -> Option<WorkKind> {
    let n = name.to_ascii_lowercase();
    let parts: Vec<&str> = n.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |k: &str| parts.contains(&k);
    if has("test") {
        return Some(test_kind(args, fed_by_xargs));
    }
    let lint_words = ["format", "type", "types", "mypy", "ruff", "clippy", "prek", "precommit", "style", "check"];
    if n.contains("lint") || n.contains("typecheck") || n.contains("fmt") || lint_words.iter().any(|k| has(k)) {
        return Some(WorkKind::LintType);
    }
    if n.contains("compile") || ["build", "bundle", "dist", "install", "all"].iter().any(|k| has(k)) {
        return Some(WorkKind::Build);
    }
    None
}

/// A path, node id, glob or file argument (not a flag's bare value like
/// `10` in `-n 10`).
fn looks_like_target(a: &str) -> bool {
    a.contains('/') || a.contains("::") || a.ends_with(".py") || a.ends_with(".ts") || a.ends_with(".js") || a.starts_with("test")
}

/// Pytest-style flags whose value is a path that is not a target.
const PATH_VALUE_FLAGS: &[&str] = &["--ignore", "--deselect", "--rootdir", "--basetemp", "-c", "--confcutdir", "--junitxml", "--cov", "--cov-report", "-p", "-o", "--log-file"];

/// A test-selection flag in any spelling: `-k expr`, `-k=expr`, `-kexpr`,
/// `--testNamePattern=x`.
fn selects_tests(a: &str) -> bool {
    let key = a.split_once('=').map_or(a, |(k, _)| k);
    if matches!(key, "-k" | "-m" | "--lf" | "--last-failed" | "-t" | "--testNamePattern" | "--grep" | "-g" | "--run") {
        return true;
    }
    // Attached short form: pytest `-kauth`, `-mslow`; jest/vitest `-tname`.
    !a.starts_with("--") && a.len() > 2 && ["-k", "-m", "-t", "-g"].iter().any(|f| a.starts_with(f))
}

/// A test runner run: the whole suite, or a targeted subset.
fn test_kind(args: &[&str], fed_by_xargs: bool) -> WorkKind {
    if fed_by_xargs {
        return WorkKind::Test; // targets arrive on stdin
    }
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if selects_tests(a) {
            return WorkKind::Test;
        }
        if PATH_VALUE_FLAGS.contains(&a) {
            i += 2;
            continue;
        }
        if !a.starts_with('-') && (looks_like_target(a) || a.starts_with('$')) {
            return WorkKind::Test;
        }
        i += 1;
    }
    WorkKind::TestSuite
}

/// `cargo test [opts] [FILTER] [-- args]`: a filter or a named test target
/// narrows it.
fn cargo_test_kind(args: &[&str]) -> WorkKind {
    let mut it = args.iter().copied().skip_while(|a| *a != "test" && *a != "nextest").skip(1);
    if args.contains(&"nextest") {
        it.next(); // `run`
    }
    let mut prev_takes_value = false;
    for a in it {
        if a == "--" {
            break;
        }
        if prev_takes_value {
            prev_takes_value = false;
            continue;
        }
        // `--test=observability` selects like `--test observability`.
        let key = a.split_once('=').map_or(a, |(k, _)| k);
        if matches!(key, "--test" | "--bin" | "--example" | "--bench") {
            return WorkKind::Test;
        }
        if matches!(a, "-p" | "--package" | "--features" | "-F" | "--target" | "--profile" | "-j" | "--jobs" | "--manifest-path" | "--target-dir") {
            prev_takes_value = true;
            continue;
        }
        if !a.starts_with('-') {
            return WorkKind::Test;
        }
    }
    WorkKind::TestSuite
}

#[cfg(test)]
mod tests {
    use super::WorkKind::*;
    use super::*;

    fn k(cmd: &str) -> WorkKind {
        classify(cmd)
    }

    #[test]
    fn single_programs() {
        assert_eq!(k("rg -n foo src"), ReadSearch);
        assert_eq!(k("git status --short"), Git);
        assert_eq!(k("gh issue view 2256"), Git);
        assert_eq!(k("ruff check ."), LintType);
        assert_eq!(k("cargo clippy --all-targets"), LintType);
        assert_eq!(k("npx tsc --noEmit"), LintType);
        assert_eq!(k("cargo build --release"), Build);
        assert_eq!(k("npm ci"), Build);
        assert_eq!(k("python -c 'print(1)'"), Other);
        assert_eq!(k("ps -eo pid,args"), Other);
        assert_eq!(k(""), Other);
        assert_eq!(k("cd /tmp && export X=1"), Other, "only noise");
    }

    #[test]
    fn test_suites_versus_targeted_runs() {
        assert_eq!(k("pytest"), TestSuite);
        assert_eq!(k("python -m unittest"), TestSuite);
        assert_eq!(k("python -m unittest tests.test_foo"), Test);
        assert_eq!(k("pytest -q -n 10"), TestSuite);
        assert_eq!(k("pdm run test -n 4"), TestSuite);
        assert_eq!(k("npm test"), TestSuite);
        assert_eq!(k("npx vitest run"), TestSuite);
        assert_eq!(k("cargo test"), TestSuite);
        assert_eq!(k("cargo test -p pi-famulus --features test-clock"), TestSuite);
        assert_eq!(k("go test ./..."), TestSuite);
        assert_eq!(k("pytest tests/api/test_x.py"), Test);
        assert_eq!(k("pytest tests/a.py::test_b -q"), Test);
        assert_eq!(k("pdm run pytest -q -k redeem"), Test);
        assert_eq!(k("python -m pytest -x tests/unit"), Test);
        assert_eq!(k(".venv/bin/python -m pytest --ignore tests/slow -q"), TestSuite);
        assert_eq!(k("cargo test status_lines"), Test);
        assert_eq!(k("cargo test --test observability"), Test);
        assert_eq!(k("npx vitest run tests/foo.test.ts"), Test);
        assert_eq!(k("xargs pdm run pytest -q -n 10 < /tmp/failed.txt"), Test, "targets on stdin");
        assert_eq!(k(r#"pdm run pytest -q -n 10 "${nodeids[@]}""#), Test, "targets in a variable");
    }

    /// Shapes taken from a real run (2026-10-05): compound, wrapped, with
    /// heredocs and command substitutions.
    #[test]
    fn real_compound_commands() {
        let suite = r#"cd /w/deprecate-services-kms && export PATH="$PWD/.venv/bin:$PATH" && log=/tmp/kms-full-test.log && pdm run test >"$log" 2>&1; rc=$?; lines=$(wc -l <"$log"); printf 'full-test exit=%s lines=%s log=%s\n' "$rc" "$lines" "$log"; tail -100 "$log"; exit "$rc""#;
        assert_eq!(k(suite), TestSuite, "the log tail must not win");
        let wrapped = r#"bash -o pipefail -c 'cd /w/x && export PATH="$PWD/.venv/bin:$PATH" && pdm run test 2>&1 | tee /tmp/full.log; result=${PIPESTATUS[0]}; exit $result'"#;
        assert_eq!(k(wrapped), TestSuite);
        let check_then_suite = r#"cd /w/m && export PATH="$PWD/.venv/bin:$PATH" && pdm run python -c 'import teehouse; from pathlib import Path; assert Path(teehouse.__file__).resolve().is_relative_to(Path.cwd().resolve())' && pdm run test > /tmp/t.log 2>&1; result=$?; tail -n 120 /tmp/t.log; exit $result"#;
        assert_eq!(k(check_then_suite), TestSuite);
        let prek = r#"cd /w/p && export PATH="$PWD/.venv/bin:$PATH" && prek run --files $(git diff --cached --name-only) > /tmp/prek.log 2>&1; rc=$?; tail -n 180 /tmp/prek.log; exit "$rc""#;
        assert_eq!(k(prek), LintType, "git inside $(…) is not the work");
        let heredoc = "cd /w/k && export PATH=\"$PWD/.venv/bin:$PATH\" && python - <<'PY'\nimport subprocess\nsubprocess.run(['pytest'])\nPY\nrg -n foo";
        assert_eq!(k(heredoc), Other, "heredoc body is not commands");
        assert_eq!(k("pdm run type-check > /tmp/tc.log 2>&1; cat /tmp/tc.log"), LintType);
        assert_eq!(k("pdm run fmt-check"), LintType);
        assert_eq!(k("pdm run py-compile"), Build);
        assert_eq!(k("git add -- a.py b.py && git commit -m 'x'"), Git);
        assert_eq!(k("for f in /tmp/a.log /tmp/b.log; do printf '%s\\n' \"$f\"; tail -n 5 \"$f\"; done"), ReadSearch);
        assert_eq!(k("git status --short && ps -eo pid,args | rg pytest"), Other);
        assert_eq!(k("rg -n 'from x' src | head -20"), ReadSearch);
        let function_wrapper = "run_check() {\n  \"$@\" >\"/tmp/$1.log\" 2>&1\n}\nrun_check fmt pdm run fmt-check || failed=1\nrun_check types pdm run type-check || failed=1";
        assert_eq!(k(function_wrapper), LintType, "a shell function wrapping known tools");
        assert_eq!(k("retry 3 cargo test"), TestSuite);
        assert_eq!(k("python tools/run.py pytest"), Other, "interpreter arguments are not commands");
    }

    /// Script names match `test` as a whole token (`test`, `test:unit`,
    /// `e2e-test`), not as a substring (`latest`, `attest`, `contest`).
    #[test]
    fn script_name_test_is_a_whole_token() {
        for cmd in ["npm run test", "npm run test:unit", "npm run test-e2e", "make e2e-test"] {
            assert_eq!(k(cmd), TestSuite, "{cmd}");
        }
        for cmd in ["npm run latest", "npm run attest", "npm run contest"] {
            assert_eq!(k(cmd), Other, "{cmd}");
        }
        assert_eq!(k("pdm run pytest"), TestSuite, "pytest still lands via TEST");
    }

    /// A wrapper option's value is not the subcommand or program
    /// (review of #35: `npm --prefix web run test` was `other`).
    #[test]
    fn wrapper_option_values_are_not_the_program() {
        for cmd in [
            "npm --prefix web run test",
            "npm --prefix web test",
            "npm --prefix=web test",
            "npm -w packages/api test",
            "pnpm -C web test",
            "pnpm --dir web run test",
            "pnpm --filter api test",
            "yarn --cwd web test",
            "uv run --directory backend pytest",
            "uv --directory backend run pytest",
            "uv run --project backend --with pytest-xdist pytest -n 4",
            "poetry -C backend run pytest",
            "make -C backend test",
            "make -j 8 test",
        ] {
            assert_eq!(k(cmd), TestSuite, "{cmd}");
        }
        assert_eq!(k("npm --prefix web run build"), Build);
        assert_eq!(k("pnpm --filter api lint"), LintType);
        assert_eq!(k("uv run --directory backend ruff check ."), LintType);
    }

    /// Selection flags in their attached forms (`-kexpr`, `-k=expr`,
    /// `--key=value`) select like the separated ones.
    #[test]
    fn attached_selection_flags_target_tests() {
        for cmd in [
            "pytest -k=auth",
            "pytest -kauth",
            "pytest -mslow",
            "pytest --last-failed",
            "npx jest --testNamePattern=login",
            "cargo test --test=observability",
            "cargo test --bin=pi-famulus",
            "cargo test --package=pi-famulus status_lines",
        ] {
            assert_eq!(k(cmd), Test, "{cmd}");
        }
        for cmd in ["pytest -q -n4", "pytest --ignore=tests/slow", "cargo test --package=pi-famulus", "cargo test --features=test-clock"] {
            assert_eq!(k(cmd), TestSuite, "{cmd}");
        }
    }

    /// Targets fed by xargs stay targets however many wrappers sit between
    /// xargs and the test runner.
    #[test]
    fn xargs_context_survives_nested_wrappers() {
        for cmd in [
            "xargs pytest",
            "xargs uv run python -m pytest -q",
            "xargs pdm run pytest -n 10",
            "xargs -n 50 env PYTHONPATH=. uv run pytest",
            "xargs npx vitest run",
            "xargs npm exec vitest run",
            "xargs retry 3 pytest",
        ] {
            assert_eq!(k(cmd), Test, "{cmd}");
        }
    }

    #[test]
    fn lexer_keeps_quoted_separators_inside_words() {
        let cmds = split_simple_commands(r#"echo 'a; b' && grep "x|y" f 2>&1 | wc -l"#);
        let words: Vec<Vec<&str>> = cmds.iter().map(|c| c.iter().map(String::as_str).collect()).collect();
        assert_eq!(words, vec![vec!["echo", "a; b"], vec!["grep", "x|y", "f"], vec!["wc", "-l"]]);
        let cmds = split_simple_commands("cat <<-EOF\n\tls; rm -rf /\n\tEOF\npwd");
        assert_eq!(cmds, vec![vec!["cat".to_string()], vec!["pwd".to_string()]]);
    }
}
