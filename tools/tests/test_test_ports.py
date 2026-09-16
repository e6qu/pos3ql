import importlib.util
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch


HELPER = Path(__file__).resolve().parents[2] / "tests/external/test_ports.py"
SPEC = importlib.util.spec_from_file_location("test_ports_helper", HELPER)
ports = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ports)


class TestPortReservationTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="pos3ql-port-test-")
        self.addCleanup(self.directory.cleanup)
        self.state = Path(self.directory.name) / "ports.json"
        self.owner = os.getpid()

    def claim(self, requested="", first=28000, last=28100):
        return ports.update_reservations(self.state, self.owner, requested, first, last)

    def test_concurrent_claims_are_distinct_before_any_server_binds(self):
        environment = dict(os.environ, POS3QL_TEST_PORT_STATE=str(self.state))
        children = [subprocess.Popen(
            [sys.executable, str(HELPER), "claim", str(self.owner), "", "28000", "28100"],
            env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        ) for _ in range(8)]
        selected = []
        for child in children:
            stdout, stderr = child.communicate(timeout=10)
            self.assertEqual(child.returncode, 0, stderr)
            selected.append(int(stdout))
        self.assertEqual(len(set(selected)), 8)
        self.assertEqual(len(json.loads(self.state.read_text())), 8)

    def test_an_unbound_claim_survives_delay_until_owner_releases_it(self):
        port = self.claim()
        with self.assertRaisesRegex(ValueError, "no unreserved available"):
            self.claim(str(port))
        ports.update_reservations(self.state, self.owner)
        self.assertEqual(self.claim(str(port)), port)

    def test_an_independent_listener_is_never_claimed(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            with self.assertRaisesRegex(ValueError, "no unreserved available"):
                self.claim(str(listener.getsockname()[1]))

    def test_release_preserves_other_live_owners(self):
        with patch.object(ports, "owner_alive", return_value=True):
            first = ports.update_reservations(self.state, 11, "", 28000, 28100)
            second = ports.update_reservations(self.state, 22, "", 28000, 28100)
            ports.update_reservations(self.state, 11)
        self.assertNotEqual(first, second)
        self.assertEqual(json.loads(self.state.read_text()), {str(second): 22})

    def test_dead_owner_reclamation_is_atomic_with_new_claim(self):
        with patch.object(ports, "owner_alive", return_value=True):
            port = ports.update_reservations(self.state, 11, "", 28000, 28100)
        with patch.object(ports, "owner_alive", side_effect=lambda owner: owner == 22):
            self.assertEqual(
                ports.update_reservations(self.state, 22, str(port), 28000, 28100), port
            )
        self.assertEqual(json.loads(self.state.read_text()), {str(port): 22})

    def test_invalid_state_is_loud_and_is_not_replaced(self):
        for contents in ["", "[]", '{"28000": 0}', '{"not-a-port": 11}']:
            self.state.write_text(contents)
            with self.assertRaises(ValueError):
                self.claim()
            self.assertEqual(self.state.read_text(), contents)

    def test_invalid_owner_and_port_ranges_fail_before_creating_state(self):
        for owner, requested, first, last in [
            (0, "", 28000, 28100), (self.owner, "0", 28000, 28100),
            (self.owner, "65536", 28000, 28100), (self.owner, "", 28100, 28000),
        ]:
            with self.assertRaises(ValueError):
                ports.update_reservations(self.state, owner, requested, first, last)
            self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
