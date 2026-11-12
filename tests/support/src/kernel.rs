//! THE KERNEL ORACLE: the real in-kernel ext4 driver, reading back what
//! this crate wrote.
//!
//! e2fsprogs is a second opinion on the same file format, written by the
//! same project, from the same specification. Linux is the thing the
//! images are actually for. A volume `e2fsck` calls clean can still be
//! one the kernel mounts differently — a directory entry it will not
//! find, an extent tree it reads shorter than we wrote it, an xattr it
//! puts in another place — and nothing on the host can catch that,
//! because a host mount is the kernel's job and macOS has no ext4 at all.
//!
//! So a kernel oracle is a script run INSIDE the harness VM against a
//! loop mount of one of our images. It is the only place in this suite
//! where a filesystem is mounted, and it never happens on the host.
//!
//! ONE GUEST CALL PER CHECK. The script does the whole comparison —
//! mount, walk, hash, read xattrs, unmount — and prints what it found,
//! rather than paying a round trip per question.
//!
//! THE IMAGE IS COPIED INTO THE GUEST'S OWN DISK for the mount, and
//! copied back afterwards when the test asked for a writable mount. A
//! loop mount reads through the page cache and writes back at its own
//! pace; pointing that at a file on the 9p share mixes two caches over
//! one file, and the host would be reading bytes the guest has not
//! written yet. The copy costs a fraction of a second for these images
//! and removes the question.

use std::collections::BTreeMap;
use std::process::Output;

use crate::oracle::{guest_quote, guest_shell, repo, session, Run};

/// Mount `image` read-only in the guest and run `script` against it —
/// the primitive [`guest_kernel_report`] is built from.
///
/// `$MNT` is the mount point, and `$IMG` the image, inside the guest.
/// The script runs under `bash -euo pipefail`, so an unchecked failure
/// inside it fails the call. Its stdout, its stderr and its exit status
/// come back exactly as they were.
///
/// The mount is `ro` AND the loop device is read-only, so a mount that
/// would have replayed a journal or updated a timestamp cannot change
/// the image behind the test's back.
#[track_caller]
fn guest_kernel_read(image: &str, script: &str) -> Output {
    run(image, script, false)
}

/// Mount `image` read-write in the guest, run `script`, and bring the
/// image back to the host with whatever the kernel wrote in it.
///
/// The reverse direction: the kernel writes, this crate reads.
#[track_caller]
pub fn guest_kernel_write(image: &str, script: &str) -> Output {
    run(image, script, true)
}

/// [`guest_kernel_write`] for a test whose answer may be that the kernel
/// refuses the mount.
///
/// Where `guest_kernel_write` panics, this returns `Err` with the same
/// account: the call ended before `script`'s status was recorded, and
/// what the guest printed says why. A refused mount is one such ending;
/// a harness that could not run at all is another, so a test reads the
/// text to tell them apart. Returning it rather than panicking keeps an
/// expected refusal out of the panic hook's output.
#[track_caller]
pub fn guest_kernel_try_write(image: &str, script: &str) -> Result<Output, String> {
    try_run(image, script, true)
}

#[track_caller]
fn run(image: &str, script: &str, writable: bool) -> Output {
    match try_run(image, script, writable) {
        Ok(out) => out,
        Err(stopped) => panic!("{stopped}"),
    }
}

#[track_caller]
fn try_run(image: &str, script: &str, writable: bool) -> Result<Output, String> {
    session();
    assert!(
        std::path::Path::new(image).starts_with(repo()),
        "the kernel oracle was given {image}, which is outside {}. The guest sees this \
         repository and nothing else of the host.",
        repo().display()
    );

    let run = Run::new();
    let guest = guest_script(image, script, writable, &run);
    let out = guest_shell(&guest)
        .unwrap_or_else(|error| panic!("cannot run the kernel oracle in the guest: {error}"));
    let options = if writable { "rw" } else { "ro" };
    let Some(code) = run.code() else {
        println!(
            "[kernel vm] mount -o {options} {image} -> stopped (harness exited {:?})",
            out.status.code()
        );
        return Err(format!(
            "the kernel oracle could not run in the fs-linux-test-harness VM \
             (the harness exited {:?}). `chore vm:status` shows the VM.\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    };
    let (stdout, stderr) = run.streams();
    println!("[kernel vm] mount -o {options} {image} -> {code}");
    Ok(Output {
        status: std::os::unix::process::ExitStatusExt::from_raw(code << 8),
        stdout,
        stderr,
    })
}

/// The guest side: copy in, mount, run, unmount, copy back.
///
/// Every step is checked, and the unmount happens on every path — a
/// loop device left attached to an image the next test rewrites is a
/// failure that lands somewhere else entirely. That includes a call
/// ended by a signal: `tests/kernel_oracle_stopped.rs` holds it to
/// leaving no mount, loop device or work directory behind.
fn guest_script(image: &str, script: &str, writable: bool, run: &Run) -> String {
    let options = if writable { "rw" } else { "ro" };
    let losetup = if writable { "" } else { "--read-only" };
    let copy_back = if writable {
        "cp --sparse=always \"$work/image\" $IMG"
    } else {
        "true"
    };
    format!(
        r#"set -euo pipefail
mkdir -p {dir}
IMG={image}
work=""
mnt=""
loop=""
script=""
# A SIGNAL ENDS THE CALL THROUGH THE EXIT TRAP, which undoes whatever the
# call has made so far (#623). Each variable is set once its step has
# happened and cleared once the normal path has undone it, so the trap
# undoes exactly what is left.
#
# The script is stopped first, all of it: a script still using the
# mount keeps `umount` from working, and the mount keeps the loop
# device. A mount still busy after that is detached lazily, so it is
# gone from the guest even before its last user is.
cleanup() {{
    local code=$?
    trap - EXIT HUP INT TERM
    if [ -n "$script" ]; then
        kill -s TERM -- "-$script" 2>/dev/null || true
        wait "$script" 2>/dev/null || true
        kill -s KILL -- "-$script" 2>/dev/null || true
    fi
    if [ -n "$mnt" ] && mountpoint -q "$mnt"; then
        for _ in 1 2 3 4 5; do
            umount "$mnt" 2>/dev/null && break
            sleep 0.2
        done
        ! mountpoint -q "$mnt" || umount -l "$mnt" || true
    fi
    [ -z "$loop" ] || losetup -d "$loop" 2>/dev/null || true
    [ -z "$work" ] || rm -rf "$work"
    exit "$code"
}}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
work="$(mktemp -d /var/tmp/fs-ext4-kernel.XXXXXX)"
mkdir -p "$work/mnt"
cp --sparse=always "$IMG" "$work/image"
loop="$(losetup --find --show {losetup} "$work/image")"
# Named before the mount, so a signal the mount returns into finds it.
mnt="$work/mnt"
mount -t ext4 -o {options} "$loop" "$mnt"
# IN THE BACKGROUND, IN A PROCESS GROUP OF ITS OWN. Bash runs a trap only
# once its foreground child has ended, so a foreground script would keep
# the mount until it finished. The group holds everything the script
# started, which is what the trap stops.
status=0
set -m
MNT="$mnt" IMG="$IMG" bash -euo pipefail -c {script} > {stdout} 2> {stderr} &
script=$!
set +m
wait "$script" || status=$?
script=""
# Unmounted BEFORE the copy back: the kernel writes on umount, and an
# image copied while mounted is one no test can reason about.
umount "$mnt"
mnt=""
losetup -d "$loop"
loop=""
if [ "$status" = 0 ]; then
    {copy_back}
fi
printf %s "$status" > {status}
rm -rf "$work"
work=""
trap - EXIT"#,
        dir = guest_quote(&run.dir.to_string_lossy()),
        image = guest_quote(image),
        script = guest_quote(script),
        stdout = guest_quote(&run.stdout.to_string_lossy()),
        stderr = guest_quote(&run.stderr.to_string_lossy()),
        status = guest_quote(&run.status.to_string_lossy()),
    )
}

/// Who a signal to a kernel oracle call reaches.
#[derive(Clone, Copy, Debug)]
pub enum Stopped {
    /// Every process in the call's process group: what a terminal's
    /// Ctrl-C, or a runner stopping a whole job, delivers.
    Group,
    /// The guest shell running the call, and nothing else: its script,
    /// and whatever the script started, are still running and still
    /// using the mount.
    ShellAlone,
}

/// Stop a kernel oracle call with `signal` while its mount is in use,
/// and report what the call left behind in the guest.
///
/// The call mounts `image` read-only as every kernel test does, and its
/// script parks a process inside the mount. Once the mount is there,
/// `signal` goes to the call's process group or to its shell alone, as
/// `who` says. When the call has ended, the guest reports one line for
/// each thing still there that the call made: `mounted <dir>`,
/// `loop <device>` and `left <dir>`; a call that does not end within 60
/// seconds adds `running`. An empty report is a call that cleaned up
/// after itself.
///
/// Whatever the call left is then removed, so a failing run does not
/// leave a mount behind for the next test either.
#[track_caller]
pub fn guest_kernel_stopped(image: &str, signal: &str, who: Stopped) -> String {
    session();
    assert!(
        std::path::Path::new(image).starts_with(repo()),
        "the kernel oracle was given {image}, which is outside {}. The guest sees this \
         repository and nothing else of the host.",
        repo().display()
    );
    let run = Run::new();
    // The script names its mount, then waits in it: `sleep` holds the
    // mount busy for as long as it runs.
    let call = guest_script(image, r#"cd "$MNT"; echo "$MNT"; sleep 600"#, false, &run);
    let target = match who {
        Stopped::Group => "-$call",
        Stopped::ShellAlone => "$call",
    };
    let wrapper = format!(
        r#"set -u
mkdir -p {dir}
rm -f {stdout}
# The call in a process group of its own, as a test's is: the group is
# what a signal to the whole call reaches.
set -m
bash -c {call} > /dev/null 2>&1 &
call=$!
set +m
mnt=""
for _ in $(seq 300); do
    mnt="$(head -n 1 {stdout} 2>/dev/null || true)"
    [ -z "$mnt" ] || break
    sleep 0.1
done
if [ -z "$mnt" ]; then
    echo "the call never mounted {image}" >&2
    kill -s KILL -- "-$call" 2>/dev/null
    exit 1
fi
work="$(dirname "$mnt")"
kill -s {signal} -- "{target}"
# The call's shell is this shell's child, so it is gone when `wait`
# returns, and `wait` is what reaps it. The watchdog bounds the wait.
( sleep 60; echo running; kill -s KILL "$call" ) 2>/dev/null &
watchdog=$!
wait "$call" 2>/dev/null
kill "$watchdog" 2>/dev/null && wait "$watchdog" 2>/dev/null
# A moment for anything the call's cleanup set going to finish.
sleep 1
mountpoint -q "$mnt" && echo "mounted $mnt"
losetup -a | grep -F "$work/" | cut -d: -f1 | sed 's/^/loop /'
[ ! -e "$work" ] || echo "left $work"
# Then remove what was left, so the guest stays usable whatever the
# verdict: the processes in the mount, the mount, the loop device.
kill -s KILL -- "-$call" 2>/dev/null
for proc in /proc/[0-9]*; do
    [ "$(readlink "$proc/cwd" 2>/dev/null)" != "$mnt" ] || kill -s KILL "${{proc#/proc/}}" 2>/dev/null
done
sleep 0.2
for _ in 1 2 3 4 5; do mountpoint -q "$mnt" || break; umount "$mnt" 2>/dev/null || sleep 0.2; done
mountpoint -q "$mnt" && umount -l "$mnt"
for loop in $(losetup -a | grep -F "$work/" | cut -d: -f1); do losetup -d "$loop"; done
rm -rf "$work"
rm -f {stdout} {stderr} {status}
exit 0"#,
        dir = guest_quote(&run.dir.to_string_lossy()),
        stdout = guest_quote(&run.stdout.to_string_lossy()),
        stderr = guest_quote(&run.stderr.to_string_lossy()),
        status = guest_quote(&run.status.to_string_lossy()),
        call = guest_quote(&call),
        image = guest_quote(image),
    );
    let out = guest_shell(&wrapper)
        .unwrap_or_else(|error| panic!("cannot run the kernel oracle in the guest: {error}"));
    assert!(
        out.status.success(),
        "the stopped kernel oracle call could not be set up ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `guest_kernel_read`, with the script's output as text and a failure
/// that prints everything the guest said.
#[track_caller]
fn guest_kernel_read_ok(image: &str, what: &str, script: &str) -> String {
    let out = guest_kernel_read(image, script);
    assert!(
        out.status.success(),
        "[{what}] the kernel could not read back {image} ({:?}):\n{}{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// SHA-256 of some bytes, as the hex `sha256sum` prints in the guest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.hex()
}

/// SHA-256, spelled out so the test support crate needs no dependency
/// for it (FIPS 180-4).
struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

impl Sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::new(),
            length: 0,
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.length += bytes.len() as u64;
        self.buffer.extend_from_slice(bytes);
        let mut at = 0;
        while self.buffer.len() - at >= 64 {
            let block: [u8; 64] = self.buffer[at..at + 64].try_into().unwrap();
            self.compress(&block);
            at += 64;
        }
        self.buffer.drain(..at);
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(chunk.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut v = self.state;
        for (&k, &wi) in Self::K.iter().zip(&w) {
            let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
            let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
            let t1 = v[7]
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(wi);
            let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
            let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
            let t2 = s0.wrapping_add(maj);
            v = [
                t1.wrapping_add(t2),
                v[0],
                v[1],
                v[2],
                v[3].wrapping_add(t1),
                v[4],
                v[5],
                v[6],
            ];
        }
        for (i, word) in v.iter().enumerate() {
            self.state[i] = self.state[i].wrapping_add(*word);
        }
    }

    fn hex(mut self) -> String {
        let bits = self.length * 8;
        self.buffer.push(0x80);
        while self.buffer.len() % 64 != 56 {
            self.buffer.push(0);
        }
        let tail = bits.to_be_bytes();
        self.buffer.extend_from_slice(&tail);
        let blocks: Vec<[u8; 64]> = self
            .buffer
            .chunks_exact(64)
            .map(|c| c.try_into().unwrap())
            .collect();
        for block in blocks {
            self.compress(&block);
        }
        self.state.iter().map(|w| format!("{w:08x}")).collect()
    }
}

/// What the kernel reported, as `(kind, path) -> value`.
fn parse(report: &str) -> BTreeMap<(String, String), String> {
    report
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut fields = line.splitn(3, '\t');
            let kind = fields.next().unwrap_or_default().to_string();
            let path = fields.next().unwrap_or_default().to_string();
            let value = fields.next().unwrap_or_default().to_string();
            ((kind, path), value)
        })
        .collect()
}

/// The guest script: walk the mounted tree, hash every file, read the
/// metadata this test cares about, and print `kind<TAB>path<TAB>value`.
///
/// One invocation does all of it. The alternative — a guest call per
/// question — is a hundred round trips for one image.
const REPORT: &str = r#"
cd "$MNT"
find . -mindepth 1 -printf '%P\n' | sort | while read -r path; do
    kind="$(stat -c '%F' "$path" | tr ' ' '-')"
    printf 'type\t%s\t%s\n' "$path" "$kind"
    printf 'mode\t%s\t%s\n' "$path" "$(stat -c '%a' "$path")"
    case "$kind" in
        regular-file | regular-empty-file)
            printf 'size\t%s\t%s\n' "$path" "$(stat -c '%s' "$path")"
            printf 'sha256\t%s\t%s\n' "$path" "$(sha256sum "$path" | cut -d' ' -f1)"
            ;;
        symbolic-link)
            printf 'target\t%s\t%s\n' "$path" "$(readlink "$path")"
            ;;
    esac
    names="$(getfattr -h -m 'user\.' --absolute-names "$path" 2>/dev/null | sed -n '2,$p' || true)"
    attrs=""
    for name in $names; do
        value="$(getfattr --only-values -n "$name" "$path" 2>/dev/null)"
        attrs="$attrs${attrs:+,}$name=$value"
    done
    if [ -n "$attrs" ]; then
        printf 'xattrs\t%s\t%s\n' "$path" "$attrs"
    fi
    if [ -d "$path" ]; then
        printf 'acl\t%s\t%s\n' "$path" \
            "$(getfacl -cpn "$path" 2>/dev/null | grep -v '^$' | paste -sd, -)"
    fi
done
printf 'kernel\t\t%s\n' "$(uname -r)"
"#;

/// Mount `image` read-only in the guest and report everything in it:
/// for every path, its type, mode, size, SHA-256, symlink target, `user.*`
/// attributes and ACL, keyed by `(kind, path)`.
///
/// ONE GUEST CALL. A test compares this against what it wrote.
#[track_caller]
pub fn guest_kernel_report(image: &str, what: &str) -> BTreeMap<(String, String), String> {
    parse(&guest_kernel_read_ok(image, what, REPORT))
}
