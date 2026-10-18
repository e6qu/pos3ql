#!/usr/bin/env python3
"""Stream a native test command and symbolize allocation-guard aborts."""

from pathlib import Path
import re
import shutil
import subprocess
import sys


def run(command):
    frames = {}
    allocation_fault = False
    process = subprocess.Popen(command, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, text=True, errors="replace", bufsize=1)
    with process.stdout:
        for line in process.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            allocation_fault |= "heap allocation after freeze or inside forbid_alloc" in line
            if not allocation_fault:
                continue
            match = re.search(r"([^\s()]+)\(\+(0x[0-9a-fA-F]+)\)", line)
            if match is None:
                continue
            binary = Path(match[1])
            if binary.parent.name != "deps" or re.fullmatch(r"pos3ql-[0-9a-f]+", binary.name) is None:
                continue
            addresses = frames.setdefault(binary, [])
            if match[2] not in addresses and len(addresses) < 64:
                addresses.append(match[2])
    status = process.wait()
    if status and frames:
        tool = shutil.which("addr2line")
        if tool is None:
            print("Allocation backtrace: addr2line is unavailable", flush=True)
        else:
            for binary, addresses in frames.items():
                print(f"Allocation backtrace for {binary}:", flush=True)
                if not binary.is_file():
                    print("Test executable is no longer available", flush=True)
                    continue
                subprocess.run([tool, "-C", "-f", "-i", "-e", str(binary), *addresses], check=False)
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    if len(sys.argv) < 2:
        raise SystemExit("usage: test_command.py COMMAND [ARG...]")
    raise SystemExit(run(sys.argv[1:]))
