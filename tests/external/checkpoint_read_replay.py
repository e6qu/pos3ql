"""Temporary replay of the retained PR 718 crash evidence on a runner."""

from pathlib import Path
import subprocess
import time

import psycopg


root = Path("replay").resolve()
credentials = root / "credentials"
credentials.write_text("access_key = replay\nsecret_key = replay-secret\n")
credentials.chmod(0o600)
config = root / "server.conf"
template = Path("tests/external/run.sh").read_text().split('cat > "$WORK/server.conf" <<EOF\n', 1)[1].split("\nEOF", 1)[0]
for variable, value in {
    "PG_PORT": "15433", "OPERATIONS_PORT": "19340", "WORK": str(root),
    "data_name": "data", "S3_TEST_PORT": "19000", "S3_TEST_BUCKET": "pos3ql-external",
    "object_prefix": "run-4363/", "S3_TEST_REGION": "us-east-1",
    "POS3QL_CREDENTIALS_FILE": str(credentials),
}.items():
    template = template.replace("${" + variable + "}", value)
config.write_text(template)
with (root / "s3.log").open("wb") as s3_log, (root / "pos3ql.log").open("wb") as server_log:
    fixture = subprocess.Popen([
        "python3", "tests/external/s3_test_server.py", "--root", str(root / "latency-0/object-store"),
        "--port", "19000", "--bucket", "pos3ql-external", "--region", "us-east-1",
        "--access-key", "replay", "--secret-key", "replay-secret", "--page-size", "7",
        "--audit-file", str(root / "audit.log"),
    ], stdout=s3_log, stderr=s3_log)
    server = None
    try:
        time.sleep(0.3)
        server = subprocess.Popen(["replay-bin/pos3ql", "--config", str(config)], stdout=server_log, stderr=server_log)
        deadline = time.monotonic() + 15
        while True:
            try:
                connection = psycopg.connect(host="127.0.0.1", port=15433, user="postgres", dbname="postgres", autocommit=True)
                break
            except psycopg.OperationalError:
                if time.monotonic() >= deadline:
                    print((root / "pos3ql.log").read_text(), flush=True)
                    raise
                time.sleep(0.1)
        with connection:
            for attempt in range(3):
                for query in [
                    "SELECT count(*), sum(id), sum(length(pad)) FROM tb",
                    "SELECT id, md5(pad) FROM tb",
                    "SELECT id, md5(pad) FROM tb ORDER BY id",
                    "SELECT id FROM tb WHERE id BETWEEN 1520 AND 1550 ORDER BY id",
                ]:
                    rows = connection.execute(query).fetchall()
                    ids = [row[0] for row in rows]
                    missing = sorted(set(range(230, 3233)) - set(ids)) if len(rows) > 30 else []
                    print(attempt, query, "rows=", len(rows), "first=", rows[:3], "last=", rows[-3:], "missing=", missing[:30], "missing_count=", len(missing), flush=True)
    finally:
        if server is not None:
            server.kill()
            server.wait()
        fixture.terminate()
        fixture.wait()
