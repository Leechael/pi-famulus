"""Inventory protocol regression tests; no Cargo build or nextest required."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("benchmark", Path(__file__).with_name("ci-nextest-benchmark.py"))
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class InventoryTests(unittest.TestCase):
    def setUp(self):
        # Actual nextest 0.9.150 schema: binary-name is flat, not binary.name.
        self.discovery = {"rust-suites": {"pi-famulus::example": {
            "binary-name": "example", "status": "listed", "testcases": {
                "normal": {"kind": "test", "ignored": False, "filter-match": {"status": "matches"}},
                "helper": {"kind": "test", "ignored": True, "filter-match": {"status": "mismatch", "reason": "ignored"}},
            },
        }}}

    def test_flat_schema_preserves_names_and_ignored_flags(self):
        self.assertEqual(benchmark.nextest_inventory(self.discovery), {"example": {"normal": False, "helper": True}})

    def test_filtered_test_cannot_disappear_silently(self):
        self.discovery["rust-suites"]["pi-famulus::example"]["testcases"]["normal"]["filter-match"] = {"status": "mismatch", "reason": "expression"}
        with self.assertRaisesRegex(AssertionError, "unexpected nextest filter"):
            benchmark.nextest_inventory(self.discovery)

    def test_nonignored_test_cannot_be_filtered_as_ignored(self):
        self.discovery["rust-suites"]["pi-famulus::example"]["testcases"]["normal"]["filter-match"] = {"status": "mismatch", "reason": "ignored"}
        with self.assertRaisesRegex(AssertionError, "unexpected nextest filter"):
            benchmark.nextest_inventory(self.discovery)

    def test_unlisted_binary_is_an_error(self):
        self.discovery["rust-suites"]["pi-famulus::example"]["status"] = "skipped"
        with self.assertRaisesRegex(AssertionError, "did not list"):
            benchmark.nextest_inventory(self.discovery)

    def test_duplicate_target_is_not_overwritten(self):
        self.discovery["rust-suites"]["other::example"] = copy.deepcopy(self.discovery["rust-suites"]["pi-famulus::example"])
        with self.assertRaisesRegex(AssertionError, "duplicate"):
            benchmark.nextest_inventory(self.discovery)

    def test_libtest_list_does_not_count_summary_as_test(self):
        self.assertEqual(benchmark.listed_tests("z: test\na::b: test\n\n2 tests, 0 benchmarks\n"), ["a::b", "z"])
        self.assertEqual(benchmark.listed_tests("\n0 tests, 0 benchmarks\n"), [])


class OrchestrationTests(unittest.TestCase):
    def test_abba_executes_both_phases_even_after_nextest_failure(self):
        with tempfile.TemporaryDirectory(prefix="nextest-orchestration-") as directory:
            root = Path(directory)
            names = sorted(benchmark.CARGO_ONLY | {"pi-famulus"})
            for name in names:
                binary = root / name
                binary.write_text(f"#!{sys.executable}\nimport sys\nif '--ignored' not in sys.argv and {name!r} != 'resource_bench': print('case: test')\n")
                binary.chmod(0o755)
            cargo = root / "cargo"
            cargo.write_text(f'''#!{sys.executable}
import json, os, pathlib, sys
root = pathlib.Path(os.environ['FIXTURE_ROOT'])
args = sys.argv[1:]
if '--no-run' in args:
    for name in {names!r}:
        print(json.dumps({{'reason': 'compiler-artifact', 'executable': str(root / name), 'profile': {{'test': True}}, 'target': {{'name': name, 'kind': ['bin' if name == 'pi-famulus' else 'test']}}}}))
elif args[:2] == ['nextest', 'list']:
    print(json.dumps({{'rust-suites': {{'pi-famulus': {{'binary-name': 'pi-famulus', 'status': 'listed', 'testcases': {{'case': {{'ignored': False, 'filter-match': {{'status': 'matches'}}}}}}}}}}}}))
elif args[:2] == ['nextest', 'run']:
    sys.exit(1)
''')
            cargo.chmod(0o755)
            output = root / "results"
            result = subprocess.run([sys.executable, str(Path(benchmark.__file__).resolve()), "--clock", "manual", "--pairs", "2", "--output", str(output)], env={**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"], "FIXTURE_ROOT": str(root)}, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0, 'failed candidate must fail the experiment')
            report = json.loads((output / "summary.json").read_text())
            self.assertEqual([run['runner'] for run in report['runs']], ['cargo', 'nextest', 'nextest', 'cargo'])
            self.assertEqual([run['passed'] for run in report['runs']], [True, False, False, True])
            for run in report['runs']:
                for step in run['steps']:
                    self.assertIn('--features', step['command'])
                    self.assertIn('test-clock', step['command'])
                if run['runner'] == 'nextest':
                    for name in benchmark.CARGO_ONLY:
                        self.assertNotIn(name, run['steps'][0]['command'])
                        self.assertIn(name, run['steps'][1]['command'])
                    self.assertEqual([step['exit_code'] for step in run['steps']], [1, 0], 'isolated Cargo tests must still run after candidate failure')
                    self.assertIn('--no-fail-fast', run['steps'][0]['command'])
                    self.assertEqual(run['steps'][0]['command'][-5:], ['--no-fail-fast', '--retries', '0', '--no-tests', 'fail'])


if __name__ == "__main__":
    unittest.main()
