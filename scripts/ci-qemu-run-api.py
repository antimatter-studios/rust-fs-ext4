#!/usr/bin/env python3
"""Fetch one GitHub REST API document for the QEMU pilot report.

The report job runs on the Antimatter VPS runner, which has no `gh`. This
does what `gh api <path> > <output>` did there, with the Python the report
already needs: one authenticated GET of the same path, the body written only
when it is JSON, and a non-zero exit on any HTTP or network failure.

Usage: ci-qemu-run-api.py <api-path> <output>
Reads GH_TOKEN, and GITHUB_API_URL when set (Actions sets it).
"""
import json
import os
import pathlib
import sys
import urllib.error
import urllib.request


def fetch(path, token, base):
    request = urllib.request.Request(
        base.rstrip("/") + "/" + path.lstrip("/"),
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "User-Agent": "rust-fs-ext4-qemu-pilot",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        body = response.read()
    json.loads(body)
    return body


def main(argv):
    if len(argv) != 3:
        print("usage: ci-qemu-run-api.py <api-path> <output>", file=sys.stderr)
        return 2
    token = os.environ.get("GH_TOKEN", "")
    if not token:
        print("GH_TOKEN is not set; cannot read the run's jobs", file=sys.stderr)
        return 1
    base = os.environ.get("GITHUB_API_URL") or "https://api.github.com"
    try:
        body = fetch(argv[1], token, base)
    except urllib.error.HTTPError as error:
        print(f"GET {argv[1]} failed: HTTP {error.code}", file=sys.stderr)
        return 1
    except (urllib.error.URLError, OSError, ValueError) as error:
        print(f"GET {argv[1]} failed: {error}", file=sys.stderr)
        return 1
    pathlib.Path(argv[2]).write_bytes(body)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
