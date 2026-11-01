#!/usr/bin/env python3
"""Controlled same-runner Cargo/nextest comparison; never drops test coverage.

Compile once, reconcile names/ignored flags from the actual test executables,
then alternate runner order. Keep stress/isolation-sensitive suites on Cargo.
This experiment does not change the normal CI entry point.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import time

# These suites deliberately rely on isolation across binaries or on concurrent
# threads within one binary (see manager/TESTING.md, C6/D2 and upgrade u11).
CARGO_ONLY = {"backpressure", "lifecycle_adversarial", "upgrade", "timing_canary", "platform", "resource_bench"}


def execute(args, cwd, output, env=None):
    started = time.monotonic()
    with output.open("w", encoding="utf-8") as stream:
        result = subprocess.run(args, cwd=cwd, env=env, stdout=stream, stderr=subprocess.STDOUT)
    return {"command": args, "seconds": time.monotonic() - started, "exit_code": result.returncode, "log": output.name}


def capture(args, cwd, output):
    result = subprocess.run(args, cwd=cwd, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    output.write_text(result.stdout, encoding="utf-8")
    output.with_suffix(output.suffix + ".stderr").write_text(result.stderr, encoding="utf-8")
    if result.returncode:
        raise RuntimeError(f"{args} failed ({result.returncode}): {result.stderr[-4000:]}")
    return result.stdout


def listed_tests(text):
    # Libtest's human summary is not a testcase. Do not infer ignored from the
    # ordinary list: query --ignored independently, using the same executable.
    return sorted(line[:-6] for line in text.splitlines() if line.endswith(": test"))


def nextest_inventory(discovery):
    actual = {}
    for suite in discovery["rust-suites"].values():
        name = suite["binary-name"]
        if name in actual:
            raise AssertionError(f"duplicate nextest binary: {name}")
        if suite["status"] != "listed":
            raise AssertionError(f"nextest did not list {name}: {suite['status']}")
        actual[name] = {}
        for test, metadata in suite["testcases"].items():
            match = metadata["filter-match"]
            ignored_only = metadata["ignored"] and match == {"status": "mismatch", "reason": "ignored"}
            if match["status"] != "matches" and not ignored_only:
                raise AssertionError(f"unexpected nextest filter: {name}::{test}")
            actual[name][test] = metadata["ignored"]
    return actual


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--clock", choices=["real", "manual"], required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--pairs", type=int, default=2, choices=[0, 1, 2])
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    manager = root / "manager"
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    features = ["--features", "test-clock"] if args.clock == "manual" else []
    report = {"clock": args.clock, "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(), "cargo_only": sorted(CARGO_ONLY), "runs": []}
    report_path = output / "summary.json"
    # Diagnostic only: these host tools cause hermetic.rs to mirror an entire
    # PATH directory for every process. Count work, without copying host paths
    # or changing the hermetic environment used by any test.
    mirrors = []
    for directory in os.get_exec_path():
        try:
            names = os.listdir(directory)
        except OSError:
            continue
        if any(name.lower() in {"claude", "pi", "herdr", "codex"} for name in names):
            mirrors.append(len(names))
    report["environment"] = {
        "hermetic_path_mirror_directories": len(mirrors),
        "hermetic_path_mirror_entries": sum(mirrors),
        "rustc": subprocess.check_output(["rustc", "-Vv"], text=True).strip(),
        "cargo": subprocess.check_output(["cargo", "--version"], text=True).strip(),
        "nextest": subprocess.check_output(["cargo", "nextest", "--version"], text=True).strip(),
        "host": platform.platform(), "architecture": platform.machine(), "logical_cpus": os.cpu_count(),
        "runner": {key: os.environ.get(key) for key in ["RUNNER_OS", "RUNNER_ARCH", "RUNNER_NAME", "ImageOS", "ImageVersion"]},
    }
    try:
        started = time.monotonic()
        compilation = capture(["cargo", "test", "--locked", "--no-run", "--message-format=json", *features], manager, output / "compile.jsonl")
        report["compile_seconds"] = time.monotonic() - started
        artifacts = {}
        for line in compilation.splitlines():
            message = json.loads(line)
            if message.get("reason") == "compiler-artifact" and message.get("executable") and message["profile"]["test"]:
                name = message["target"]["name"]
                if name in artifacts:
                    raise AssertionError(f"duplicate test target: {name}")
                artifacts[name] = message
        if not artifacts or not CARGO_ONLY <= artifacts.keys():
            raise AssertionError(f"missing compiled test artifacts: {sorted(artifacts)}")

        # Passing explicit Cargo target selectors prevents the nextest phase
        # from launching intentionally isolated suites, rather than silently
        # filtering their individual tests. New targets join automatically.
        selectors = []
        canonical = {}
        for name, artifact in sorted(artifacts.items()):
            binary = artifact["executable"]
            all_names = listed_tests(capture([binary, "--list"], manager, output / f"{name}.list.txt"))
            ignored = set(listed_tests(capture([binary, "--list", "--ignored"], manager, output / f"{name}.ignored.txt")))
            if not ignored <= set(all_names):
                raise AssertionError(f"ignored inventory is not a subset for {name}")
            canonical[name] = {test: test in ignored for test in all_names}
            if name not in CARGO_ONLY:
                kind = artifact["target"]["kind"]
                if kind == ["bin"]:
                    selectors += ["--bin", name]
                elif kind == ["test"]:
                    selectors += ["--test", name]
                else:
                    raise AssertionError(f"unsupported target kind {name}: {kind}; explicitly preserve its coverage")
        if not canonical.get("platform"):
            raise AssertionError("custom platform harness must expose its tests")
        if canonical.get("resource_bench"):
            raise AssertionError("debug benchmark must stay opt-in; preserve its separate release execution")
        (output / "canonical-inventory.json").write_text(json.dumps(canonical, indent=2), encoding="utf-8")

        discovery = json.loads(capture(["cargo", "nextest", "list", "--locked", *features, *selectors, "--message-format", "json"], manager, output / "nextest-list.json"))
        actual = nextest_inventory(discovery)
        expected = {name: tests for name, tests in canonical.items() if name not in CARGO_ONLY}
        if actual != expected:
            (output / "nextest-inventory.json").write_text(json.dumps(actual, indent=2), encoding="utf-8")
            raise AssertionError("Cargo and nextest names/ignored states differ; inspect inventory JSON")
        report["inventory"] = {"targets": len(canonical), "tests": sum(len(tests) for tests in canonical.values()), "ignored": sum(sum(tests.values()) for tests in canonical.values()), "nextest_targets": len(expected), "identical": True}
        print(json.dumps({"inventory": report["inventory"]}), flush=True)

        baseline = ["cargo", "test", "--locked", "--no-fail-fast", *features]
        candidate = ["cargo", "nextest", "run", "--locked", *features, *selectors, "--no-fail-fast", "--retries", "0", "--no-tests", "fail"]
        isolated = ["cargo", "test", "--locked", "--no-fail-fast", *features]
        for name in sorted(CARGO_ONLY):
            isolated += ["--test", name]
        for pair in range(args.pairs):
            # AB then BA reduces the bias of always giving one runner the
            # later, warmer execution. It is not a statistically large sample.
            order = ["cargo", "nextest"] if pair % 2 == 0 else ["nextest", "cargo"]
            for runner in order:
                entry = {"pair": pair + 1, "runner": runner, "steps": []}
                commands = [baseline] if runner == "cargo" else [candidate, isolated]
                for index, command in enumerate(commands):
                    result = execute(command, manager, output / f"pair-{pair + 1}-{runner}-{index}.log")
                    entry["steps"].append(result)
                    # Even if nextest fails, execute the isolated phase: no
                    # fail-fast or retries may hide coverage in an experiment.
                entry["seconds"] = sum(step["seconds"] for step in entry["steps"])
                entry["passed"] = all(step["exit_code"] == 0 for step in entry["steps"])
                report["runs"].append(entry)
                report_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
                print(json.dumps(entry), flush=True)
        if not all(run["passed"] for run in report["runs"]):
            raise AssertionError("one or more measured runs failed; no successful speedup claim")
    finally:
        report_path.write_text(json.dumps(report, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
