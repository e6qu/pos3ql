import pathlib
import subprocess
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
CHECK = ROOT / "tools" / "check_dups.py"


class DuplicateCodeTests(unittest.TestCase):
    def run_check(self, files):
        with tempfile.TemporaryDirectory() as directory:
            source = pathlib.Path(directory) / "src"
            source.mkdir()
            for name, text in files.items():
                (source / name).write_text(text)
            return subprocess.run(
                [sys.executable, str(CHECK), "--root", directory, "--min-lines", "4", "--threshold", "1"],
                capture_output=True,
                text=True,
                check=False,
            )

    def test_rejects_a_normalized_clone(self):
        body = "\n".join(f"let  value_{number}  =  {number};" for number in range(4))
        result = self.run_check({"one.rs": body, "two.rs": body.replace("  ", " ")})
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("clone:", result.stdout)

    def test_ignores_short_matches(self):
        result = self.run_check({"one.rs": "let one = 1;\nlet two = 2;\nlet three = 3;", "two.rs": "let one = 1;\nlet two = 2;\nlet other = 4;"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_project_source_obeys_the_ratchet(self):
        result = subprocess.run(
            [sys.executable, str(CHECK)], capture_output=True, text=True, check=False
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
