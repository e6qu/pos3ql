import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


TOOL = Path(__file__).resolve().parents[1] / "native-test-artifact.py"
REVISION = "1234567890abcdef" * 2 + "12345678"
TARGET = "aarch64-apple-darwin"


class NativeTestArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "pos3ql-abcd"
        self.source.write_bytes(b"native test executable" + b"x" * (1 << 20))
        self.source.chmod(0o755)
        self.records = self.root / "build.jsonl"
        self.output = self.root / "artifact"
        self.write_records([self.record()])

    def record(self, kind="lib", test=True, name="pos3ql"):
        return {"reason": "compiler-artifact", "target": {"name": name, "kind": [kind]},
                "profile": {"test": test}, "executable": str(self.source)}

    def write_records(self, records):
        completed = [*records, {"reason": "build-finished", "success": True}]
        self.records.write_text("".join(json.dumps(record) + "\n" for record in completed))

    def run_tool(self, *arguments):
        return subprocess.run([sys.executable, str(TOOL), *map(str, arguments)],
                              capture_output=True, text=True)

    def prepare(self):
        result = self.run_tool("prepare", self.records, self.output, REVISION, TARGET)
        self.assertEqual(result.returncode, 0, result.stderr)

    def verify(self, revision=REVISION, target=TARGET):
        return self.run_tool("verify", self.output, revision, target)

    def change_manifest(self, **changes):
        path = self.output / "manifest.json"
        manifest = json.loads(path.read_text())
        manifest.update(changes)
        path.write_text(json.dumps(manifest))

    def test_selects_only_library_test_and_restores_transport_permissions(self):
        self.write_records([self.record(kind="bin"), self.record(test=False), self.record()])
        self.prepare()
        binary = self.output / "pos3ql-native-tests"
        binary.chmod(0o644)
        self.source.unlink()
        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(binary.resolve()))
        self.assertEqual(binary.stat().st_mode & 0o777, 0o755)
        manifest = json.loads((self.output / "manifest.json").read_text())
        self.assertEqual(manifest["revision"], REVISION)
        self.assertEqual(manifest["target"], TARGET)
        self.assertEqual(manifest["sha256"], hashlib.sha256(binary.read_bytes()).hexdigest())

    def test_rejects_missing_duplicate_or_wrong_library_artifacts(self):
        for records in [[], [self.record(), self.record()], [self.record(kind="test")],
                        [self.record(test=False)], [self.record(name="other")]]:
            with self.subTest(records=records):
                self.write_records(records)
                result = self.run_tool("prepare", self.records, self.output, REVISION, TARGET)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.output.exists())

    def test_rejects_truncated_failed_or_nonfinal_build_completion(self):
        for records in [[self.record()],
                        [self.record(), {"reason": "build-finished", "success": False}],
                        [{"reason": "build-finished", "success": True}, self.record()]]:
            with self.subTest(records=records):
                self.records.write_text("".join(json.dumps(record) + "\n" for record in records))
                result = self.run_tool("prepare", self.records, self.output, REVISION, TARGET)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.output.exists())

    def test_rejects_wrong_revision_and_target(self):
        self.prepare()
        for revision, target in [("a" * 40, TARGET), (REVISION, "aarch64-unknown-linux-gnu")]:
            result = self.verify(revision, target)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, "")

    def test_rejects_corruption_beyond_first_hash_chunk(self):
        self.prepare()
        binary = self.output / "pos3ql-native-tests"
        with binary.open("r+b") as stream:
            stream.seek((1 << 20) + 5)
            stream.write(b"!")
        binary.chmod(0o644)
        self.assertNotEqual(self.verify().returncode, 0)
        self.assertEqual(binary.stat().st_mode & 0o777, 0o644)

    def test_rejects_manifest_path_injection_and_invalid_fields(self):
        self.prepare()
        original = (self.output / "manifest.json").read_text()
        for changes in [{"executable": "../pos3ql-abcd"}, {"executable": str(self.source)},
                        {"version": 2}, {"version": True}, {"size": True}, {"size": 1},
                        {"sha256": "not-a-hash"}, {"extra": "unsupported"}]:
            with self.subTest(changes=changes):
                (self.output / "manifest.json").write_text(original)
                self.change_manifest(**changes)
                self.assertNotEqual(self.verify().returncode, 0)

    def test_rejects_symlink_binary_and_manifest(self):
        self.prepare()
        binary = self.output / "pos3ql-native-tests"
        binary.unlink()
        binary.symlink_to(self.source)
        self.assertNotEqual(self.verify().returncode, 0)
        binary.unlink()
        binary.write_bytes(self.source.read_bytes())
        manifest = self.output / "manifest.json"
        saved = self.root / "manifest-copy.json"
        manifest.rename(saved)
        manifest.symlink_to(saved)
        self.assertNotEqual(self.verify().returncode, 0)

    def test_rejects_malformed_cargo_json_and_existing_output(self):
        self.records.write_text("not JSON\n")
        self.assertNotEqual(self.run_tool("prepare", self.records, self.output,
                                         REVISION, TARGET).returncode, 0)
        self.write_records([self.record()])
        self.prepare()
        self.assertNotEqual(self.run_tool("prepare", self.records, self.output,
                                         REVISION, TARGET).returncode, 0)
        self.assertEqual(self.verify().returncode, 0)


if __name__ == "__main__":
    unittest.main()
