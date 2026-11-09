import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "workflow_timeouts", ROOT / "tools/check-workflow-timeouts.py"
)
TIMEOUTS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(TIMEOUTS)


class WorkflowTimeoutTests(unittest.TestCase):
    def test_step_timeout_does_not_replace_or_duplicate_job_timeout(self):
        text = """jobs:
  build:
    runs-on: ubuntu-24.04
    timeout-minutes: 15
    steps:
      - name: Format
        timeout-minutes: 5
        run: cargo fmt --check
"""
        self.assertEqual(TIMEOUTS.errors(text, 15), [])
        self.assertTrue(TIMEOUTS.errors(text.replace("    timeout-minutes: 15\n", ""), 15))

    def test_equal_global_counts_cannot_hide_missing_or_duplicate_job_timeout(self):
        text = """jobs:
  first:
    runs-on: ubuntu-24.04
    timeout-minutes: 15
    timeout-minutes: 10
  second:
    runs-on: ubuntu-24.04
"""
        problems = TIMEOUTS.errors(text, 15)
        self.assertEqual(len(problems), 2)
        self.assertIn("job first:", problems[0])
        self.assertIn("job second:", problems[1])

    def test_job_and_step_timeouts_are_positive_bounded_literals(self):
        for value in ("0", "16", "-1", "1.5", "${{ inputs.timeout }}", ""):
            for indent in (4, 8):
                with self.subTest(value=value, indent=indent):
                    text = "jobs:\n  build:\n    runs-on: ubuntu-24.04\n"
                    text += " " * indent + "timeout-minutes: " + value + "\n"
                    self.assertTrue(TIMEOUTS.errors(text, 15))

    def test_reusable_job_has_no_runner_timeout_and_comments_are_allowed(self):
        text = """jobs:
  shared:
    uses: ./.github/workflows/formatting.yml
  build: # native compilation
    timeout-minutes: 15 # unchanged ceiling
    runs-on: ubuntu-24.04
permissions:
  contents: read
"""
        self.assertEqual(TIMEOUTS.errors(text, 15), [])

    def test_repository_jobs_keep_explicit_job_and_step_limits(self):
        for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
            with self.subTest(workflow=path.name):
                self.assertEqual(TIMEOUTS.errors(path.read_text(), 15), [])


if __name__ == "__main__":
    unittest.main()
