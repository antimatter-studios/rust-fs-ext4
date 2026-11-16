#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir "$work/bin"
cat > "$work/bin/cargo" <<'EOF'
#!/usr/bin/env bash
echo called >> "$CARGO_MARKER"
exit 99
EOF
chmod +x "$work/bin/cargo"

refuses_profile() {
    marker="$work/cargo-called"
    if PATH="$work/bin:$PATH" CARGO_MARKER="$marker" \
        "$REPO/scripts/test.sh" --locked "$@" >"$work/output" 2>&1; then
        echo "FAIL  scripts/test.sh accepted $*" >&2
        exit 1
    fi
    if ! grep -q 'test.sh: .*profile' "$work/output"; then
        echo "FAIL  scripts/test.sh did not explain its profile refusal for $*" >&2
        cat "$work/output" >&2
        exit 1
    fi
    if [ -e "$marker" ]; then
        echo "FAIL  scripts/test.sh invoked cargo for $*" >&2
        exit 1
    fi
}

refuses_profile --release
refuses_profile -r
refuses_profile -qr
refuses_profile --profile=release
refuses_profile --profile release

echo 'PASS  scripts/test.sh refuses explicit Cargo profiles'
