#!/usr/bin/env python3
"""Package one checked-head Cargo libtest executable for native CI reuse."""

import argparse
import hashlib
import json
import pathlib
import re
import shutil
import stat


EXECUTABLE = "pos3ql-native-tests"
MANIFEST = "manifest.json"
FIELDS = {"version", "revision", "target", "executable", "size", "sha256"}


def identity(revision, target):
    if re.fullmatch(r"[0-9a-f]{40}", revision) is None:
        raise ValueError("revision must be the complete lowercase Git SHA")
    if re.fullmatch(r"[a-zA-Z0-9_-]+", target) is None:
        raise ValueError("invalid native target")


def regular_file(path):
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError(f"artifact file must be regular, without symlinks: {path}")


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1 << 20):
            value.update(chunk)
    return value.hexdigest()


def executable(build_jsonl):
    candidates = []
    finished = False
    with build_jsonl.open(encoding="utf-8") as stream:
        for line in stream:
            record = json.loads(line)
            if not isinstance(record, dict):
                raise ValueError("Cargo build records must be JSON objects")
            if finished:
                raise ValueError("Cargo build has records after build-finished")
            if record.get("reason") == "build-finished":
                if record.get("success") is not True:
                    raise ValueError("Cargo native build did not succeed")
                finished = True
            if (record.get("reason") == "compiler-artifact"
                    and record.get("target", {}).get("name") == "pos3ql"
                    and record.get("target", {}).get("kind") == ["lib"]
                    and record.get("profile", {}).get("test") is True
                    and record.get("executable")):
                candidates.append(record["executable"])
    if not finished:
        raise ValueError("Cargo build is missing its successful build-finished record")
    if len(candidates) != 1 or not isinstance(candidates[0], str):
        raise ValueError("Cargo build must identify exactly one pos3ql libtest executable")
    source = pathlib.Path(candidates[0])
    regular_file(source)
    if not source.stat().st_mode & stat.S_IXUSR:
        raise ValueError("Cargo libtest output is not executable")
    return source.resolve()


def prepare(build_jsonl, directory, revision, target):
    identity(revision, target)
    source = executable(build_jsonl)
    if directory.is_symlink():
        raise ValueError("artifact directory must not be a symlink")
    directory.mkdir(parents=True, exist_ok=True)
    if any(directory.iterdir()):
        raise ValueError("artifact output directory must be empty")
    binary = directory / EXECUTABLE
    with source.open("rb") as incoming, binary.open("xb") as outgoing:
        shutil.copyfileobj(incoming, outgoing, length=1 << 20)
    binary.chmod(0o755)
    size = binary.stat().st_size
    if size == 0:
        raise ValueError("Cargo libtest executable is empty")
    manifest = {
        "version": 1,
        "revision": revision,
        "target": target,
        "executable": EXECUTABLE,
        "size": size,
        "sha256": digest(binary),
    }
    with (directory / MANIFEST).open("x", encoding="utf-8") as stream:
        json.dump(manifest, stream, sort_keys=True)
        stream.write("\n")


def verify(directory, revision, target):
    identity(revision, target)
    if directory.is_symlink():
        raise ValueError("artifact directory must not be a symlink")
    manifest_path = directory / MANIFEST
    regular_file(manifest_path)
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not isinstance(manifest, dict) or set(manifest) != FIELDS:
        raise ValueError("invalid native test artifact manifest fields")
    if type(manifest["version"]) is not int or manifest["version"] != 1:
        raise ValueError("unsupported native test artifact version")
    if manifest["revision"] != revision or manifest["target"] != target:
        raise ValueError("native test artifact revision or target does not match this job")
    if manifest["executable"] != EXECUTABLE:
        raise ValueError("invalid native test artifact executable path")
    if type(manifest["size"]) is not int or manifest["size"] <= 0:
        raise ValueError("invalid native test artifact size")
    if not isinstance(manifest["sha256"], str) or re.fullmatch(
            r"[0-9a-f]{64}", manifest["sha256"]) is None:
        raise ValueError("invalid native test artifact digest")
    binary = directory / EXECUTABLE
    regular_file(binary)
    if binary.stat().st_size != manifest["size"] or digest(binary) != manifest["sha256"]:
        raise ValueError("native test artifact executable does not match its manifest")
    # GitHub artifact transport normalizes file modes; restore only verified bytes.
    binary.chmod(0o755)
    return binary.resolve()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    select = commands.add_parser("executable")
    select.add_argument("build_jsonl", type=pathlib.Path)
    create = commands.add_parser("prepare")
    create.add_argument("build_jsonl", type=pathlib.Path)
    create.add_argument("directory", type=pathlib.Path)
    create.add_argument("revision")
    create.add_argument("target")
    check = commands.add_parser("verify")
    check.add_argument("directory", type=pathlib.Path)
    check.add_argument("revision")
    check.add_argument("target")
    arguments = parser.parse_args()
    try:
        if arguments.command == "executable":
            print(executable(arguments.build_jsonl))
        elif arguments.command == "prepare":
            prepare(arguments.build_jsonl, arguments.directory,
                    arguments.revision, arguments.target)
        else:
            print(verify(arguments.directory, arguments.revision, arguments.target))
    except (OSError, ValueError, TypeError, AttributeError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
