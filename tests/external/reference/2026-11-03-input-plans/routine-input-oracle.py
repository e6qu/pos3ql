"""Temporary two-session PostgreSQL cache probe; removed before final PR head."""

import json
from pathlib import Path
import signal
import subprocess
import sys


def expired(_signal, _frame):
    raise TimeoutError("PostgreSQL oracle command did not finish in 20 seconds")


signal.signal(signal.SIGALRM, expired)
container, destination = sys.argv[1:]
sessions = []
evidence = []


def command(session, label, sql):
    marker = f"oracle_end_{len(evidence)}"
    process = sessions[session]
    signal.alarm(20)
    try:
        process.stdin.write(f"{sql}\n\\echo {marker}\n")
        process.stdin.flush()
        output = []
        while True:
            line = process.stdout.readline()
            if not line:
                raise RuntimeError(f"PostgreSQL oracle closed during {label}: {output}")
            if line.strip() == marker:
                break
            output.append(line.rstrip("\n"))
        record = dict(session=session, label=label, sql=sql, output=output)
        evidence.append(record)
        print(json.dumps(record), flush=True)
    finally:
        signal.alarm(0)


try:
    for _ in range(2):
        sessions.append(subprocess.Popen(
            ["docker", "exec", "-i", container, "psql", "-U", "postgres", "-X", "-A", "-t", "-q", "-v", "ON_ERROR_STOP=1"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True, bufsize=1,
        ))
    for session in range(2):
        command(session, "warm", "SET search_path=validity_probe,public; SELECT opaque(); SELECT last_value FROM checks;")
    command(0, "create pending schema", "BEGIN; CREATE SCHEMA oracle_cross_committed;")
    command(1, "other before commit", "SELECT opaque(); SELECT last_value FROM checks;")
    command(0, "creator before commit", "SELECT opaque(); SELECT last_value FROM checks;")
    command(0, "creator after commit", "COMMIT; SELECT opaque(); SELECT last_value FROM checks;")
    command(1, "other after commit", "SELECT opaque(); SELECT last_value FROM checks;")
    command(0, "create rolled back schema", "BEGIN; CREATE SCHEMA oracle_cross_rollback; SELECT opaque(); SELECT last_value FROM checks;")
    command(1, "other before rollback", "SELECT opaque(); SELECT last_value FROM checks;")
    command(0, "creator after rollback", "ROLLBACK; SELECT opaque(); SELECT last_value FROM checks;")
    command(1, "other after rollback", "SELECT opaque(); SELECT last_value FROM checks;")
    command(0, "statistics setup", "RESET search_path; CREATE SEQUENCE cross_analyze_checks; CREATE TYPE cross_analyze_choice AS ENUM('a'); CREATE DOMAIN cross_analyze_checked AS cross_analyze_choice CHECK(nextval('cross_analyze_checks')>0); CREATE TABLE cross_analyze_anchor(id integer); CREATE TABLE cross_analyze_unrelated(id integer); INSERT INTO cross_analyze_anchor VALUES(1); CREATE FUNCTION cross_analyze_body() RETURNS cross_analyze_checked[] LANGUAGE sql SET search_path=public AS $$SELECT '{a}'::cross_analyze_checked[] FROM cross_analyze_anchor LIMIT 1$$; ALTER SEQUENCE cross_analyze_checks RESTART WITH 1;")
    command(1, "statistics path", "RESET search_path;")
    command(0, "statistics step 0", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(1, "statistics step 1", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(0, "statistics step 2", 'BEGIN; INSERT INTO cross_analyze_anchor VALUES(2); ANALYZE cross_analyze_anchor; SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(1, "statistics step 3", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(0, "statistics step 4", 'ROLLBACK; SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(1, "statistics step 5", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(0, "statistics step 6", 'ANALYZE cross_analyze_anchor; SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(1, "statistics step 7", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(0, "statistics step 8", 'BEGIN; INSERT INTO cross_analyze_anchor VALUES(2); ANALYZE cross_analyze_anchor; SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(1, "statistics step 9", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(0, "statistics step 10", 'COMMIT; SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(1, "statistics step 11", 'SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
    command(0, "statistics step 12", 'ANALYZE cross_analyze_unrelated; SELECT cross_analyze_body(); SELECT last_value FROM cross_analyze_checks;')
finally:
    Path(destination).write_text("\n".join(json.dumps(row) for row in evidence) + "\n")
    for process in sessions:
        process.kill()
        process.wait()
