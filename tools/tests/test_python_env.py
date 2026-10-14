"""Exercise interrupted downloads and incomplete environments without network I/O."""

import hashlib
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("python_env", ROOT / "tests/external/python_env.py")
ENV = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ENV)


class PythonEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.wheel = b"verified test wheel"
        self.digest = hashlib.sha256(self.wheel).hexdigest()
        self.packages = {"psycopg": ("3.3.6", {self.digest})}
        self.lock = self.root / "requirements.lock"
        self.lock.write_text(f"psycopg==3.3.6 --hash=sha256:{self.digest}\n")
        self.addCleanup(patch.stopall)
        patch.object(ENV, "LOCK", self.lock).start()
        self.venv = self.root / "venv"
        (self.venv / "bin").mkdir(parents=True)
        (self.venv / "bin/python").touch()
        self.marker = self.venv / ".pos3ql-python-ready"
        self.cache = self.root / "cache"
        patch.dict(ENV.os.environ, {"POS3QL_PYTHON_WHEELHOUSE": str(self.cache)}).start()

    def seed_cache(self):
        directory = self.cache / ENV.cache_key()
        directory.mkdir(parents=True)
        (directory / "psycopg-3.3.6-py3-none-any.whl").write_bytes(self.wheel)
        return directory

    def test_lock_rejects_empty_unpinned_and_duplicate_dependencies(self):
        for invalid in ("", "psycopg>=3\n", self.lock.read_text() * 2):
            with self.subTest(lock=invalid):
                self.lock.write_text(invalid)
                with self.assertRaises(ValueError):
                    ENV.pins()

    def test_cached_wheel_bytes_must_match_pinned_hash(self):
        directory = self.seed_cache()
        self.assertTrue(ENV.wheels_ready(directory, self.packages))
        next(directory.glob("*.whl")).write_bytes(b"interrupted or corrupted")
        self.assertFalse(ENV.wheels_ready(directory, self.packages))

    def test_interrupted_stream_retries_whole_download_with_same_time_limit(self):
        failure = subprocess.CalledProcessError(2, "pip download")
        with patch.object(ENV, "run", side_effect=[failure, None]) as run, \
                patch.object(ENV, "wheels_ready", return_value=True), \
                patch.object(ENV.time, "sleep") as sleep:
            ENV.download(["python", "-m", "pip"], self.root, self.packages)
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args_list[0], run.call_args_list[1])
        command, seconds = run.call_args.args
        self.assertEqual(seconds, 30)
        self.assertEqual(command[command.index("--timeout") + 1], "15")
        self.assertEqual(command[command.index("--retries") + 1], "0")
        sleep.assert_called_once_with(1)

    def test_repeated_download_timeout_stops_after_three_attempts(self):
        failure = subprocess.TimeoutExpired("pip download", 30)
        with patch.object(ENV, "run", side_effect=failure) as run, \
                patch.object(ENV.time, "sleep") as sleep:
            with self.assertRaises(subprocess.TimeoutExpired):
                ENV.download(["pip"], self.root, self.packages)
        self.assertEqual(run.call_count, 3)
        self.assertEqual([call.args for call in sleep.call_args_list], [(1,), (2,)])

    def test_successful_download_with_wrong_hash_is_rejected(self):
        with patch.object(ENV, "run") as run, \
                patch.object(ENV, "wheels_ready", return_value=False):
            with self.assertRaisesRegex(ValueError, "dependency lock"):
                ENV.download(["pip"], self.root, self.packages)
        run.assert_called_once()

    def test_verified_cache_installs_offline_and_marks_only_verified_environment(self):
        self.seed_cache()
        with patch.object(ENV, "run") as run, \
                patch.object(ENV, "download") as download, \
                patch.object(ENV, "environment_ready", return_value=True):
            ENV.prepare(self.venv)
        download.assert_not_called()
        self.assertEqual(run.call_count, 2)
        install = run.call_args_list[0].args[0]
        self.assertIn("--no-index", install)
        self.assertIn("--require-hashes", install)
        self.assertEqual(self.marker.read_text(), ENV.cache_key())

    def test_failed_install_does_not_leave_partial_environment_marked_ready(self):
        self.seed_cache()
        self.marker.write_text(ENV.cache_key())
        with patch.object(ENV, "environment_ready", return_value=False), \
                patch.object(ENV, "run", side_effect=subprocess.CalledProcessError(1, "pip")):
            with self.assertRaises(subprocess.CalledProcessError):
                ENV.prepare(self.venv)
        self.assertFalse(self.marker.exists())
        self.assertTrue((self.venv / "bin/python").exists())

    def test_import_failure_after_install_prevents_ready_marker(self):
        self.seed_cache()
        with patch.object(ENV, "environment_ready", return_value=False), \
                patch.object(ENV, "run"):
            with self.assertRaisesRegex(ValueError, "version/import"):
                ENV.prepare(self.venv)
        self.assertFalse(self.marker.exists())

    def test_ready_environment_checks_versions_before_skipping_work(self):
        self.marker.write_text(ENV.cache_key())
        with patch.object(ENV, "environment_ready", return_value=True) as ready, \
                patch.object(ENV, "run") as run:
            ENV.prepare(self.venv)
        ready.assert_called_once()
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
