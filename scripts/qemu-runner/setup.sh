#!/usr/bin/env bash
set -euo pipefail
cd /opt/ext4-runner-recipe
arch="$(dpkg --print-architecture)"
case "$arch" in
    amd64) cross=gcc-aarch64-linux-gnu; cross_libc=libc6-dev-arm64-cross; cross_compiler=aarch64-linux-gnu-gcc ;;
    arm64) cross=gcc-x86-64-linux-gnu; cross_libc=libc6-dev-amd64-cross; cross_compiler=x86_64-linux-gnu-gcc ;;
    *) echo "unsupported runner architecture: $arch" >&2; exit 1 ;;
esac
mapfile -t packages <packages.txt
apt-get update -qq
apt-get install -y --no-install-recommends "${packages[@]}" "$cross" "$cross_libc"
for compiler in gcc "$cross_compiler"; do
    for object in Scrt1.o crti.o; do
        test -r "$("$compiler" -print-file-name="$object")"
    done
done
FS_CORE_CALLER="$PWD" bash core-toolchain.sh
rustup target add aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu
read -r version checksum < <(python3 - "$arch" <<'PY'
import json, sys
config = json.load(open('environment.json'))['oras']
print(config['version'], config['sha256'][sys.argv[1]])
PY
)
curl -fsSL --retry 3 --connect-timeout 15 --max-time 300 \
    "https://github.com/oras-project/oras/releases/download/v$version/oras_${version}_linux_$arch.tar.gz" -o /tmp/oras.tar.gz
printf '%s  /tmp/oras.tar.gz\n' "$checksum" | sha256sum -c -
tar -xzf /tmp/oras.tar.gz -C /usr/local/bin oras
dpkg-query -W >/opt/ext4-runner-packages.txt
rustc -vV >/opt/ext4-runner-rustc.txt
python3 - "$RUNNER_RECIPE_KEY" "$(uname -m)" <<'PY'
import json, pathlib, sys
pathlib.Path('/opt/ext4-runner.json').write_text(json.dumps({'key': sys.argv[1], 'arch': sys.argv[2]}) + '\n')
PY
rm -rf /var/lib/apt/lists/* /tmp/oras.tar.gz /opt/rustup/downloads /opt/rustup/tmp
