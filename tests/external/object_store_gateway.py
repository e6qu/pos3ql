#!/usr/bin/env python3
"""Test-only implementation of pos3ql's generic object-store gateway."""

import argparse
import hashlib
import http.server
import os
import pathlib
import urllib.parse


def object_path(root, namespace, key):
    if not namespace or not key or any(part in ("", ".", "..") for part in key.split("/")):
        raise ValueError("invalid object path")
    return root / namespace / key


class Gateway(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    root = None

    def log_message(self, *_):
        pass

    def target(self):
        parsed = urllib.parse.urlsplit(self.path)
        parts = [urllib.parse.unquote(part) for part in parsed.path.split("/")]
        if len(parts) < 4 or parts[1:3] != ["v1", "objects"]:
            raise ValueError("unknown route")
        return parsed, parts[3], "/".join(parts[4:])

    @staticmethod
    def etag(data):
        return '"' + hashlib.sha256(data).hexdigest() + '"'

    def fail(self, status, message):
        data = message.encode()
        self.send_response(status)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def read_body(self):
        return self.rfile.read(int(self.headers.get("content-length", "0")))

    def do_PUT(self):
        try:
            _, namespace, key = self.target()
            path = object_path(self.root, namespace, key)
            exists = path.exists()
            if self.headers.get("if-none-match") == "*" and exists:
                return self.fail(412, "exists")
            if "if-match" in self.headers:
                if not exists or self.etag(path.read_bytes()) != self.headers["if-match"]:
                    return self.fail(412, "generation changed")
            data = self.read_body()
            path.parent.mkdir(parents=True, exist_ok=True)
            temporary = path.with_suffix(path.suffix + ".tmp")
            temporary.write_bytes(data)
            os.replace(temporary, path)
            self.send_response(200)
            self.send_header("etag", self.etag(data))
            self.send_header("content-length", "0")
            self.end_headers()
        except ValueError:
            self.fail(400, "invalid object path")

    def do_GET(self):
        try:
            parsed, namespace, key = self.target()
            if not key:
                prefix = urllib.parse.parse_qs(parsed.query).get("prefix", [""])[0]
                directory = self.root / namespace
                keys = [] if not directory.exists() else sorted(
                    str(path.relative_to(directory)) for path in directory.rglob("*") if path.is_file()
                )
                body = "".join(key + "\n" for key in keys if key.startswith(prefix)).encode()
                self.send_response(200)
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                return self.wfile.write(body)
            data = object_path(self.root, namespace, key).read_bytes()
            first, last = 0, len(data) - 1
            if "range" in self.headers:
                first, last = map(int, self.headers["range"].removeprefix("bytes=").split("-"))
            body = data[first:last + 1]
            self.send_response(206 if "range" in self.headers else 200)
            self.send_header("etag", self.etag(data))
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except FileNotFoundError:
            self.fail(404, "not found")
        except (ValueError, IndexError):
            self.fail(400, "invalid request")

    def do_DELETE(self):
        try:
            _, namespace, key = self.target()
            path = object_path(self.root, namespace, key)
            path.unlink(missing_ok=True)
            self.send_response(204)
            self.end_headers()
        except ValueError:
            self.fail(400, "invalid object path")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    Gateway.root = pathlib.Path(args.root).resolve()
    Gateway.root.mkdir(parents=True, exist_ok=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Gateway).serve_forever()


if __name__ == "__main__":
    main()
