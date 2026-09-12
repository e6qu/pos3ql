#!/usr/bin/env python3
"""Deterministic test server for pos3ql's locked S3 compatibility profile."""

import argparse
import hashlib
import hmac
import http.server
import json
import os
import pathlib
import threading
import time
import urllib.parse
import xml.sax.saxutils


def signing_key(secret, date, region):
    date_key = hmac.new(("AWS4" + secret).encode(), date.encode(), hashlib.sha256).digest()
    region_key = hmac.new(date_key, region.encode(), hashlib.sha256).digest()
    service_key = hmac.new(region_key, b"s3", hashlib.sha256).digest()
    return hmac.new(service_key, b"aws4_request", hashlib.sha256).digest()


def normalize_header(value):
    return " ".join(value.strip().split())


class S3TestServer(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    root = None
    bucket = None
    region = None
    access_key = None
    secret_key = None
    session_token = None
    page_size = 1000
    mutation_lock = threading.Lock()
    metrics_lock = threading.Lock()
    metrics_file = None
    latency_seconds = 0.0
    metrics = {
        "schema_version": 1,
        "requests": {"put": 0, "get": 0, "range_get": 0, "list": 0, "delete": 0},
        "request_body_bytes": 0,
        "response_body_bytes": 0,
        "errors": 0,
    }

    def log_message(self, *_):
        pass

    @classmethod
    def record(cls, operation, request_bytes=0, response_bytes=0, error=False):
        if cls.metrics_file is None:
            return
        with cls.metrics_lock:
            cls.metrics["requests"][operation] += 1
            cls.metrics["request_body_bytes"] += request_bytes
            cls.metrics["response_body_bytes"] += response_bytes
            cls.metrics["errors"] += int(error)

    @classmethod
    def write_metrics_forever(cls):
        while True:
            with cls.metrics_lock:
                snapshot = json.dumps(cls.metrics, sort_keys=True) + "\n"
            temporary = cls.metrics_file.with_suffix(cls.metrics_file.suffix + ".tmp")
            temporary.write_text(snapshot, encoding="utf-8")
            os.replace(temporary, cls.metrics_file)
            time.sleep(0.05)

    @classmethod
    def delay(cls):
        if cls.latency_seconds:
            time.sleep(cls.latency_seconds)

    def error(self, status, code, message):
        body = (
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>"
            f"<Error><Code>{code}</Code><Message>{xml.sax.saxutils.escape(message)}</Message>"
            "</Error>"
        ).encode()
        self.send_response(status)
        self.send_header("content-type", "application/xml")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def read_body(self):
        length = int(self.headers.get("content-length", "0"))
        if length < 0:
            raise ValueError("negative content length")
        body = self.rfile.read(length)
        if len(body) != length:
            raise ValueError("truncated request body")
        return body

    def target(self):
        parsed = urllib.parse.urlsplit(self.path)
        parts = parsed.path.split("/")
        if len(parts) < 2 or urllib.parse.unquote(parts[1]) != self.bucket:
            raise FileNotFoundError("bucket not found")
        key = urllib.parse.unquote("/".join(parts[2:]))
        if key and any(part in ("", ".", "..") for part in key.split("/")):
            raise ValueError("invalid key")
        return parsed, key

    def object_path(self, key):
        if not key:
            raise ValueError("object key is empty")
        return self.root / self.bucket / key

    @staticmethod
    def etag(data):
        return '"' + hashlib.sha256(data).hexdigest() + '"'

    def authenticate(self, body):
        authorization = self.headers.get("authorization", "")
        prefix = "AWS4-HMAC-SHA256 "
        if not authorization.startswith(prefix):
            raise PermissionError("missing Signature Version 4 authorization")
        fields = {}
        for field in authorization[len(prefix) :].split(","):
            name, value = field.strip().split("=", 1)
            if name in fields:
                raise PermissionError(f"duplicate authorization field {name}")
            fields[name] = value
        if set(fields) != {"Credential", "SignedHeaders", "Signature"}:
            raise PermissionError("wrong authorization fields")
        credential = fields.get("Credential", "").split("/")
        if len(credential) != 5 or credential[0] != self.access_key:
            raise PermissionError("wrong access key")
        date, region, service, terminal = credential[1:]
        if region != self.region or service != "s3" or terminal != "aws4_request":
            raise PermissionError("wrong signing scope")
        timestamp = self.headers.get("x-amz-date", "")
        if len(timestamp) != 16 or timestamp[:8] != date:
            raise PermissionError("wrong signing timestamp")
        payload_hash = hashlib.sha256(body).hexdigest()
        if self.headers.get("x-amz-content-sha256") != payload_hash:
            raise PermissionError("wrong payload hash")
        if self.session_token is not None:
            if self.headers.get("x-amz-security-token") != self.session_token:
                raise PermissionError("wrong session token")
        elif self.headers.get("x-amz-security-token") is not None:
            raise PermissionError("unexpected session token")

        signed_names = fields.get("SignedHeaders", "").split(";")
        required_signed = ["host", "x-amz-content-sha256", "x-amz-date"]
        if self.session_token is not None:
            required_signed.append("x-amz-security-token")
        if signed_names != required_signed:
            raise PermissionError("wrong signed-header set or order")
        canonical_headers = ""
        for name in signed_names:
            value = self.headers.get(name)
            if value is None:
                raise PermissionError(f"missing signed header {name}")
            canonical_headers += f"{name}:{normalize_header(value)}\n"
        parsed = urllib.parse.urlsplit(self.path)
        query_fields = parsed.query.split("&") if parsed.query else []
        if query_fields != sorted(query_fields):
            raise PermissionError("query parameters are not canonically sorted")
        canonical_request = "\n".join(
            [
                self.command,
                parsed.path,
                parsed.query,
                canonical_headers,
                ";".join(signed_names),
                payload_hash,
            ]
        )
        scope = f"{date}/{region}/s3/aws4_request"
        string_to_sign = "\n".join(
            [
                "AWS4-HMAC-SHA256",
                timestamp,
                scope,
                hashlib.sha256(canonical_request.encode()).hexdigest(),
            ]
        )
        expected = hmac.new(
            signing_key(self.secret_key, date, region),
            string_to_sign.encode(),
            hashlib.sha256,
        ).hexdigest()
        if not hmac.compare_digest(fields.get("Signature", ""), expected):
            raise PermissionError("signature mismatch")

    def begin(self):
        body = self.read_body()
        self.authenticate(body)
        parsed, key = self.target()
        return body, parsed, key

    def do_PUT(self):
        try:
            body, _, key = self.begin()
            self.delay()
            path = self.object_path(key)
            with self.mutation_lock:
                exists = path.exists()
                if self.headers.get("if-none-match") == "*" and exists:
                    self.record("put", request_bytes=len(body), error=True)
                    return self.error(412, "PreconditionFailed", "object exists")
                expected = self.headers.get("if-match")
                if expected is not None and (
                    not exists or self.etag(path.read_bytes()) != expected
                ):
                    self.record("put", request_bytes=len(body), error=True)
                    return self.error(412, "PreconditionFailed", "generation changed")
                path.parent.mkdir(parents=True, exist_ok=True)
                temporary = self.root / ".temporary" / self.bucket / key
                temporary.parent.mkdir(parents=True, exist_ok=True)
                temporary.write_bytes(body)
                os.replace(temporary, path)
            self.record("put", request_bytes=len(body))
            self.send_response(200)
            self.send_header("etag", self.etag(body))
            self.send_header("content-length", "0")
            self.end_headers()
        except PermissionError as error:
            self.error(403, "SignatureDoesNotMatch", str(error))
        except FileNotFoundError:
            self.error(404, "NoSuchBucket", "bucket not found")
        except ValueError as error:
            self.error(400, "InvalidRequest", str(error))

    def do_GET(self):
        try:
            _, parsed, key = self.begin()
            self.delay()
            query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
            if not key and query.get("list-type") == ["2"]:
                return self.list_objects(query)
            data = self.object_path(key).read_bytes()
            body = data
            status = 200
            if self.headers.get("range") is not None:
                spec = self.headers["range"]
                if not spec.startswith("bytes=") or "," in spec:
                    return self.error(400, "InvalidRange", "invalid byte range")
                first, last = (int(value) for value in spec[6:].split("-", 1))
                if first > last or first >= len(data):
                    return self.error(416, "InvalidRange", "range not satisfiable")
                body = data[first : min(last + 1, len(data))]
                status = 206
            operation = "range_get" if status == 206 else "get"
            self.record(operation, response_bytes=len(body))
            self.send_response(status)
            self.send_header("etag", self.etag(data))
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        except PermissionError as error:
            self.error(403, "SignatureDoesNotMatch", str(error))
        except FileNotFoundError:
            self.error(404, "NoSuchKey", "object not found")
        except (ValueError, IndexError) as error:
            self.error(400, "InvalidRequest", str(error))

    def list_objects(self, query):
        if query.get("encoding-type") != ["url"]:
            return self.error(400, "InvalidArgument", "encoding-type must be url")
        prefix = query.get("prefix", [""])[0]
        token = query.get("continuation-token", ["0"])[0]
        try:
            offset = int(token)
        except ValueError:
            return self.error(400, "InvalidToken", "invalid continuation token")
        directory = self.root / self.bucket
        keys = [] if not directory.exists() else sorted(
            str(path.relative_to(directory))
            for path in directory.rglob("*")
            if path.is_file()
        )
        keys = [key for key in keys if key.startswith(prefix)]
        page = keys[offset : offset + self.page_size]
        next_offset = offset + len(page)
        truncated = next_offset < len(keys)
        contents = "".join(
            f"<Contents><Key>{urllib.parse.quote(key, safe='')}</Key></Contents>"
            for key in page
        )
        next_token = (
            f"<NextContinuationToken>{next_offset}</NextContinuationToken>"
            if truncated
            else ""
        )
        body = (
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>"
            "<ListBucketResult>"
            f"<IsTruncated>{str(truncated).lower()}</IsTruncated>"
            f"{contents}{next_token}</ListBucketResult>"
        ).encode()
        self.record("list", response_bytes=len(body))
        self.send_response(200)
        self.send_header("content-type", "application/xml")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_DELETE(self):
        try:
            _, _, key = self.begin()
            self.delay()
            path = self.object_path(key)
            with self.mutation_lock:
                path.unlink(missing_ok=True)
            self.record("delete")
            self.send_response(204)
            self.end_headers()
        except PermissionError as error:
            self.error(403, "SignatureDoesNotMatch", str(error))
        except FileNotFoundError:
            self.error(404, "NoSuchBucket", "bucket not found")
        except ValueError as error:
            self.error(400, "InvalidRequest", str(error))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True)
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--region", required=True)
    parser.add_argument("--access-key", required=True)
    parser.add_argument("--secret-key", required=True)
    parser.add_argument("--session-token")
    parser.add_argument("--page-size", type=int, default=1000)
    parser.add_argument("--metrics-file")
    parser.add_argument("--latency-ms", type=float, default=0.0)
    args = parser.parse_args()
    S3TestServer.root = pathlib.Path(args.root).resolve()
    S3TestServer.bucket = args.bucket
    S3TestServer.region = args.region
    S3TestServer.access_key = args.access_key
    S3TestServer.secret_key = args.secret_key
    S3TestServer.session_token = args.session_token
    S3TestServer.page_size = args.page_size
    S3TestServer.latency_seconds = args.latency_ms / 1000.0
    if args.metrics_file:
        S3TestServer.metrics_file = pathlib.Path(args.metrics_file).resolve()
        S3TestServer.metrics_file.parent.mkdir(parents=True, exist_ok=True)
        threading.Thread(target=S3TestServer.write_metrics_forever, daemon=True).start()
    (S3TestServer.root / args.bucket).mkdir(parents=True, exist_ok=True)
    http.server.ThreadingHTTPServer(("127.0.0.1", args.port), S3TestServer).serve_forever()


if __name__ == "__main__":
    main()
