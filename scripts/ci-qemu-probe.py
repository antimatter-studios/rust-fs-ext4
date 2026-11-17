#!/usr/bin/env python3
import math
import subprocess
import sys


def main(arguments):
    if len(arguments) < 2:
        print("usage: ci-qemu-probe.py SECONDS COMMAND [ARG...]", file=sys.stderr)
        return 2
    try:
        seconds = float(arguments[0])
    except ValueError:
        return 2
    if not math.isfinite(seconds) or seconds <= 0:
        return 2
    try:
        return subprocess.run(arguments[1:], timeout=seconds).returncode
    except subprocess.TimeoutExpired:
        print(f"guest boot probe exceeded {seconds:g} seconds", file=sys.stderr)
        return 124


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
