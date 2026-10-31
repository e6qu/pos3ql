#!/usr/bin/env python3
"""Prepare the pinned SQL test clients; never accept a partial installation."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import sysconfig
import time


LOCK = Path(__file__).with_name("requirements.lock")
ATTEMPTS = 3
DOWNLOAD_SECONDS = 30


def cache_key():
    return "-".join((sys.implementation.cache_tag, sysconfig.get_platform(),
                     hashlib.sha256(LOCK.read_bytes()).hexdigest()))


def pins():
    entries = LOCK.read_text().replace("\\\n", " ").splitlines()
    result = {}
    for entry in entries:
        if not entry.strip() or entry.startswith("#"):
            continue
        match = re.fullmatch(r"([\w-]+)==([\w.]+)\s+((?:--hash=sha256:[0-9a-f]{64}\s*)+)",
                             entry.strip())
        if not match:
            raise ValueError("invalid Python test dependency lock entry")
        name, version, hashes = match.groups()
        if name in result:
            raise ValueError("duplicate Python test dependency lock entry")
        result[name] = (version, set(re.findall(r"sha256:([0-9a-f]{64})", hashes)))
    if not result:
        raise ValueError("empty Python test dependency lock")
    return result


def wheels_ready(directory, packages):
    # The directory is keyed by interpreter, platform and lock. Check bytes
    # again after restoring CI caches, before allowing an offline install.
    for name, (version, hashes) in packages.items():
        prefix = name.replace("-", "_") + "-" + version + "-"
        if not any(wheel_hash(path) in hashes
                   for path in directory.glob(prefix + "*.whl")):
            return False
    return True


def wheel_hash(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(command, seconds):
    subprocess.run(command, check=True, timeout=seconds)


def download(pip, directory, packages):
    command = pip + ["download", "--no-deps", "--only-binary=:all:",
                     "--require-hashes", "--retries", "0", "--timeout", "15",
                     "--index-url", "https://pypi.org/simple", "-r", str(LOCK),
                     "--dest", str(directory)]
    for attempt in range(ATTEMPTS):
        try:
            run(command, DOWNLOAD_SECONDS)
            if not wheels_ready(directory, packages):
                raise ValueError("Python test wheels did not match the dependency lock")
            return
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
            if attempt + 1 == ATTEMPTS:
                raise
            print(f"Python test wheel download failed; retry {attempt + 2}/{ATTEMPTS}",
                  file=sys.stderr, flush=True)
            time.sleep(attempt + 1)


def environment_ready(python, packages):
    expected = {name: version for name, (version, _) in packages.items()}
    probe = ("import importlib.metadata as m, json; import psycopg, psycopg2, sqlalchemy, alembic; "
             "import sys, sysconfig; "
             "assert sys.implementation.cache_tag == " + repr(sys.implementation.cache_tag) + "; "
             "assert sysconfig.get_platform() == " + repr(sysconfig.get_platform()) + "; "
             "expected=json.loads(" + repr(json.dumps(expected)) + "); "
             "assert all(m.version(n)==v for n,v in expected.items()); "
             "assert psycopg.pq.__impl__ == 'binary'")
    try:
        subprocess.run([str(python), "-c", probe], check=True, timeout=15,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        return True
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired):
        return False


def prepare(venv):
    packages = pins()
    key = cache_key()
    marker = venv / ".pos3ql-python-ready"
    python = venv / "bin" / "python"
    if marker.is_file() and marker.read_text() == key and environment_ready(python, packages):
        return
    marker.unlink(missing_ok=True)
    if not python.is_file():
        run([sys.executable, "-m", "venv", str(venv)], 30)
    cache = Path(os.environ.get("POS3QL_PYTHON_WHEELHOUSE",
                                str(Path.home() / ".cache" / "pos3ql-test-wheels"))) / key
    cache.mkdir(parents=True, exist_ok=True)
    pip = [str(python), "-m", "pip", "--isolated", "--disable-pip-version-check"]
    if not wheels_ready(cache, packages):
        download(pip, cache, packages)
    run(pip + ["install", "--no-index", "--find-links", str(cache), "--no-deps",
               "--only-binary=:all:", "--require-hashes", "-r", str(LOCK)], 30)
    run(pip + ["check"], 15)
    if not environment_ready(python, packages):
        raise ValueError("Python test environment failed its version/import check")
    marker.write_text(key)


if __name__ == "__main__":
    if sys.argv[1:] == ["--cache-key"]:
        print(cache_key())
    elif len(sys.argv) == 2:
        try:
            prepare(Path(sys.argv[1]).resolve())
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            sys.exit(f"Python test environment: {error}")
    else:
        sys.exit("usage: python_env.py VENV | --cache-key")
