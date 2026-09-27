#!/usr/bin/env bash
# semver-check.sh — refuse a public-API break the version does not declare.
#
# Compares this crate's public API against the newest version PUBLISHED TO
# CRATES.IO — not a tag, not origin/main — with cargo-semver-checks, and fails
# when the change needs a bigger bump than Cargo.toml's version makes. For a
# 0.x crate a break needs the minor to move (0.5.1 -> 0.6.0); an addition
# needs at least the patch.
#
# WHY THIS EXISTS (#120). Two source-breaking changes — `XattrEntry` gaining a
# field and `Error` gaining a variant — merged with the version sitting on the
# published 0.5.1 and the changelog's [Unreleased] empty, and a pull request
# checklist saying "No change to public Rust API". Review caught one of the two.
# The same shape turned up independently in four repositories, so it is a
# missing step, and a step a person has to remember is the step that goes
# missing. This one runs on every pull request.
#
# THE BASELINE IS THE REGISTRY because that is what a consumer has. A tag can
# exist for a version that never published, and a version can publish from a
# commit no tag names; the registry is the one list a downstream `cargo update`
# actually reads.
#
# WHAT IT CANNOT SEE. A change of behaviour behind an unchanged signature, and
# a change to equality from a derived `PartialEq` over a new field. Those still
# need a changelog line written by a person — this catches the ones a compiler
# would, before a consumer's compiler does.
#
# cargo-semver-checks is MIT/Apache-2.0. CARGO_SEMVER_CHECKS_VERSION pins the
# version CI installs; a different local one is reported, not refused.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PINNED="${CARGO_SEMVER_CHECKS_VERSION:-0.50.0}"

if ! cargo semver-checks --version >/dev/null 2>&1; then
  echo "semver-check: cargo-semver-checks is not installed." >&2
  echo "              cargo install cargo-semver-checks --locked --version $PINNED" >&2
  exit 1
fi
have="$(cargo semver-checks --version | awk '{print $2}')"
if [ "$have" != "$PINNED" ]; then
  echo "semver-check: note: cargo-semver-checks $have here, CI pins $PINNED" >&2
fi

version="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
echo "semver-check: am-fs-ext4 $version against the newest crates.io release"

# --release-type is NOT passed: the bump is read from Cargo.toml, so the
# version a release would publish is the version being checked.
exec cargo semver-checks check-release --package am-fs-ext4
