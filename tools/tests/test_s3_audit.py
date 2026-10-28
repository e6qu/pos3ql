"""Test-store diagnostics and listing cursors preserve their external contracts."""

import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import urllib.parse
import xml.etree.ElementTree as ET


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


class ObjectListingTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.server = object.__new__(MODULE.S3TestServer)
        self.server.root = Path(temporary.name)
        self.server.bucket = "listing"
        self.server.page_size = 2
        self.server.list_cursors = {}
        self.directory = self.server.root / self.server.bucket
        self.directory.mkdir()

    def put(self, key):
        path = self.directory / key
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"fixture")

    def page(self, prefix, token=None, **parameters):
        server = self.server
        server.wfile = io.BytesIO()
        server.send_response = lambda status: setattr(server, "response_status", status)
        server.send_header = lambda *_: None
        server.end_headers = lambda: None
        query = {"encoding-type": ["url"], "prefix": [prefix], **parameters}
        if token is not None:
            query["continuation-token"] = [token]
        server.list_objects(query)
        xml = ET.fromstring(server.wfile.getvalue())
        return (server.response_status,
                [urllib.parse.unquote(key.text) for key in xml.findall("Contents/Key")],
                xml.findtext("NextContinuationToken"), xml.findtext("Code"))

    def test_insertions_before_the_boundary_never_repeat_a_returned_key(self):
        for name in ["a", "c", "e", "g"]:
            self.put("p/" + name)
        status, keys, token, _ = self.page("p/")
        self.assertEqual((status, keys), (200, ["p/a", "p/c"]))
        self.put("p/b")
        status, keys, next_token, _ = self.page("p/", token)
        self.assertEqual((status, keys, next_token), (200, ["p/e", "p/g"], None))

    def test_deleting_returned_pages_does_not_skip_unseen_keys(self):
        for name in ["a", "c", "e", "g", "i"]:
            self.put("p/" + name)
        _, first, token, _ = self.page("p/")
        for key in first:
            (self.directory / key).unlink()
        status, second, token, _ = self.page("p/", token)
        self.assertEqual((status, second), (200, ["p/e", "p/g"]))
        for key in second:
            (self.directory / key).unlink()
        status, last, token, _ = self.page("p/", token)
        self.assertEqual((status, first + second + last, token),
                         (200, ["p/a", "p/c", "p/e", "p/g", "p/i"], None))

    def test_tokens_remain_bounded_for_maximum_keys_and_reject_wrong_scopes(self):
        prefix = "p/" + "/".join(["x" * 240] * 4) + "/"
        names = [prefix + "y" * (1024 - len(prefix) - 1) + digit for digit in "123"]
        # Exercise S3's 1,024-byte keys without the host filesystem path limit.
        self.enterContext(mock.patch.object(
            Path, "rglob", return_value=[self.directory / name for name in names]))
        self.enterContext(mock.patch.object(Path, "is_file", return_value=True))
        status, keys, token, _ = self.page(prefix)
        self.assertEqual((status, keys), (200, names[:2]))
        self.assertLessEqual(len(token), 1024)
        self.assertNotEqual(token, names[1])
        status, keys, next_token, _ = self.page(prefix, token)
        self.assertEqual((status, keys, next_token), (200, names[2:], None))
        self.assertEqual(self.page("other/", token)[::3], (400, "InvalidToken"))
        self.assertEqual(self.page(prefix, "0")[::3], (400, "InvalidToken"))
        for count in ["0", "1001", "invalid"]:
            self.assertEqual(self.page(prefix, **{"max-keys": [count]})[::3],
                             (400, "InvalidArgument"))


if __name__ == "__main__":
    unittest.main()
