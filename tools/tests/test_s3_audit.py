"""Test-store diagnostics preserve mutation order without recording credentials."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SOURCE = Path(__file__).resolve().parents[2] / "tests/external/s3_test_server.py"
SPEC = importlib.util.spec_from_file_location("s3_test_audit", SOURCE)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ObjectAuditTest(unittest.TestCase):
    def test_mutation_order_and_only_recovery_roots_are_recorded(self):
        with tempfile.TemporaryDirectory() as temporary:
            server = object.__new__(MODULE.S3TestServer)
            server.audit_file = Path(temporary) / "audit.jsonl"
            server.audit_mutation("put", "prefix/blocks/abc", b"1234\x06bytes", False)
            server.audit_mutation("put", "prefix/manifest",
                                  b"lsn 10\nwriter 123\ndsst 0 0 1 2 a b c\n"
                                  b"subscription secret-connection-string\n", True)
            server.audit_mutation("delete", "prefix/blocks/abc", b"", True)
            events = [json.loads(line) for line in server.audit_file.read_text().splitlines()]
            self.assertEqual([event["operation"] for event in events], ["put", "put", "delete"])
            self.assertEqual(events[0]["block_type"], 6)
            self.assertEqual(events[1]["roots"], ["lsn 10", "writer 123", "dsst 0 0 1 2 a b c"])
            self.assertTrue(events[2]["existed"])
            self.assertNotIn("secret-connection-string", server.audit_file.read_text())
            self.assertLessEqual(events[0]["time_ns"], events[2]["time_ns"])

    def test_disabled_audit_does_not_change_test_store_operations(self):
        server = object.__new__(MODULE.S3TestServer)
        server.audit_file = None
        server.audit_mutation("delete", "blocks/abc", b"", True)


if __name__ == "__main__":
    unittest.main()
