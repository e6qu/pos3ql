#!/usr/bin/env python3

import http.client
import http.server
import pathlib
import socket
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))
from object_store_gateway import Gateway


class GatewayMutationTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        Gateway.root = pathlib.Path(self.temporary.name)
        Gateway.mutation_lock = threading.Lock()
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Gateway)
        self.worker = threading.Thread(target=self.server.serve_forever)
        self.worker.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()
        self.temporary.cleanup()

    @property
    def address(self):
        return self.server.server_address

    def put(self, body, headers=None):
        connection = http.client.HTTPConnection(*self.address)
        connection.request("PUT", "/v1/objects/test/manifest", body, headers or {})
        response = connection.getresponse()
        status = response.status
        etag = response.getheader("etag")
        response.read()
        connection.close()
        return status, etag

    def test_truncated_put_does_not_replace_an_object(self):
        status, etag = self.put(b"stable")
        self.assertEqual(status, 200)

        client = socket.create_connection(self.address)
        request = (
            b"PUT /v1/objects/test/manifest HTTP/1.1\r\n"
            b"Host: localhost\r\n"
            b"Connection: close\r\n"
            b"Content-Length: 20\r\n"
            + f"If-Match: {etag}\r\n\r\n".encode()
            + b"broken"
        )
        client.sendall(request)
        client.shutdown(socket.SHUT_WR)
        response = b""
        while chunk := client.recv(4096):
            response += chunk
        client.close()

        self.assertIn(b" 400 ", response)
        self.assertEqual((Gateway.root / "test" / "manifest").read_bytes(), b"stable")

    def test_compare_and_swap_is_atomic(self):
        status, etag = self.put(b"initial")
        self.assertEqual(status, 200)
        barrier = threading.Barrier(2)
        statuses = []

        def replace(body):
            barrier.wait()
            statuses.append(self.put(body, {"If-Match": etag})[0])

        workers = [
            threading.Thread(target=replace, args=(b"left",)),
            threading.Thread(target=replace, args=(b"right",)),
        ]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join()

        self.assertEqual(sorted(statuses), [200, 412])
        self.assertIn(
            (Gateway.root / "test" / "manifest").read_bytes(), (b"left", b"right")
        )


if __name__ == "__main__":
    unittest.main()
