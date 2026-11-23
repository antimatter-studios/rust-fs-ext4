# QEMU runner pilot

This opt-in experiment measures whether software-emulated guests finish ext4
tests sooner than waiting for accelerated GitHub-hosted runners. It does not
replace the required `ci-ok` workflow or change the shared harness.

## Matched trials

| Guest | GitHub-hosted x86_64 Ubuntu 24.04 | VPS x86_64 Ubuntu 24.04 |
|---|---|---|
| x86_64 Debian 12 | KVM, CPU `host` | TCG, CPU `max` |
| aarch64 Debian 12 | TCG, CPU `max` | TCG, CPU `max` |

Each guest gets two CPUs, 2 GiB RAM, a fresh 12 GiB sparse disk, the same pinned
Debian cloud-image build (20260909-2596), and the repository's pinned Rust
toolchain and core sibling. SHA512 digests are checked before a cold boot. Prepared
disks are required from GHCR; prebuilt test bundles can also be restored as described below.
Each job boots only one guest. ARM64 is a real ARM64 kernel
and userspace: both `uname -m` and every compiled test executable's ELF machine
are checked. It is not an x86_64 test substituted for ARM64 coverage.

The default `single` workload builds only the `capi_basic` integration target
and executes exactly `mount_and_umount_basic_image` against a kernel-made
`ext4-basic.img`. It is an early boot, toolchain and test-launch diagnostic,
not the full test contract. On a cache miss, dependencies and tests are compiled
outside QEMU by a native compiler targeting the guest architecture.

The smoke workload builds and executes the library's host-independent tests
and `ro_compat_write_guard`, using the normal debug profile and a kernel-made
`ext4-basic.img`. Compile and execution are timed separately. Smoke deliberately
does not claim to satisfy the full merge contract. The `full` workload instead
builds all fixtures and runs the complete release-profile suite, with the existing
1,078-test floor and no ignored tests. This preserves the current profile for
measurement; changing test optimization is separate work (#634).

## Native builds and content-keyed reuse

The old ARM64 trial booted in 8m21s, spent about 75 minutes in guest setup and
toolchain installation, then compiled dependencies for about 15 minutes before
the 90-minute workload deadline returned status 124. No test ran. The final
console tail showed earlier successful mounts, not a guest crash. This cold
cross-architecture setup is not a viable execution path. The pilot now builds
before booting QEMU and executes the resulting
ELF test binaries directly inside the matching guest. It never substitutes a
host-architecture test for the requested guest architecture.

Compilation and QEMU run in a provisioned Debian 12 OCI environment. Its base
digest is recorded in `scripts/qemu-pilot-environment.json`; its platform matches
the Docker daemon's native architecture. Both Rust Linux targets and a GCC
cross linker produce ARM64 or x86_64 binaries without an emulated compiler.
The pipeline container has two CPUs and a 4 GiB memory cap. Compilation finishes
before the 2 GiB guest starts; no nested Docker daemon, privileged container or
Docker socket is required. Standalone local compilation uses a 3 GiB container.
Debian 12 avoids building against a newer host glibc than the guest provides.
Registry credentials are supplied only to ephemeral registry clients and the
pipeline coordinator container, never to image layers or the guest payload.

Three independent SHA256 keys select images and OCI artifacts under
`ghcr.io/antimatter-studios/rust-fs-ext4/qemu-pilot`:

- `tests-<arch>-<key>` holds the executable bundle and compiler/package evidence.
  Its key includes source and lockfiles, the core sibling, pinned compiler and
  builder image, target architecture, workload, profile and test filters.
- `guest-<arch>-<key>` holds a powered-off prepared disk. Its key includes the
  Debian cloud-image digest, guest installation and boot scripts, fixture
  builder, lwext4 inputs, core toolchain installer, architecture and workload.
- `runner-<native-arch>-<key>` holds QEMU, UEFI, ORAS, the pinned Rust compiler,
  both Rust targets, native and cross linkers, and the packages/configuration in
  `scripts/qemu-runner/`. Its recipe key includes those files, the pinned Debian
  base, Rust toolchain file, ORAS version/checksums, core installer and refresh
  epoch. Application source, placement and acceleration do not invalidate it.
  Consumers validate its recipe label and native architecture, then execute the
  resolved immutable `@sha256:` digest, not a mutable tag. Publishing preserves
  an existing recipe tag instead of overwriting it with a fresh apt resolution.

Guest and executable artifact downloads resolve their recipe tag once, validate
the returned SHA256 manifest digest, then pull only the immutable `@sha256:`
reference. Each successful registry download retains that exact reference in
`cache-reference.txt` beside the downloaded files. A manifest disappearing after
resolution is a failed download, not a cache miss or permission to provision.
Only an absent manifest at initial resolution permits the existing preparation
or native-build policy to handle a cache miss; authentication and network errors
remain fatal. Explicit local-directory reuse is unchanged and is not registry
publication evidence.

Runner placement and KVM/TCG are deliberately excluded: matching hosted and VPS
trials reuse the same artifacts. A separate `QEMU guest preparation (pilot)`
workflow supplies installed tools and kernel-made fixtures before measurement:
x86_64 preparation uses KVM on a hosted x86_64 runner; ARM64 preparation uses
a native ARM64 hosted runner, not an ARM64 guest on x86_64. Native ARM64 still
uses TCG where KVM is unavailable, but never cross-architecture tool installation.
Full workloads also
install Cargo for metadata assertions, not for compiling tests. Before publishing,
the guest removes transferred sources, binaries and its ephemeral SSH key,
cleans cloud-init state, removes guest SSH host keys and the saved random seed,
and trims freed root-filesystem blocks through the discard-enabled disk before
shutting down normally. A live disk is never published.
Each new trial still gets a private writable overlay and a fresh SSH key.
Every seed supplies `scripts/qemu-guest-network.yaml`, using the explicit QEMU
private subnet, MAC and static guest address. A reused image previously booted
with only a link-local IPv6 address after its cloud-init state was cleaned; the
pilot no longer relies on the cloud image's fallback DHCP configuration. This
network recipe also participates in the guest environment key. The journal
identified DHCPv6 client initialization failing before IPv4 was configured.
The underlying cause was the pinned guest's old cloud-init deleting
`/etc/machine-id`: the next boot had no machine-id file for networkd to read.
The hashed `scripts/ci-qemu-image-clean.sh` recipe now leaves an empty inode
after cleaning, so systemd can generate and commit a fresh ID on the next boot.
This IPv4-only private link also explicitly disables IPv6 advertisements and
link-local configuration rather than relying on DHCP flags alone. Host SSH remains
bound to loopback. Read-only result collection retries transport status 255 up
to three times; it never reruns a test or converts a failed test into a pass.

These are recipe keys, not a claim that apt resolves packages identically forever.
Actual package versions are retained. Increment `refresh_epoch` in the environment
JSON to force a package refresh without changing the recipe. Registry authentication
and network errors fail visibly; only an absent artifact is a cache miss. Executable
hashes, ELF architectures, workload identity and archive paths are checked before
execution. The benchmark report rejects mismatched package or compiler evidence.

`compile.time` describes the original native build, even on a cache hit.
`host-build-seconds.txt` measures this trial's build or restore time, while
`cache-events.tsv` distinguishes hits and misses. Keep cold and warm results
separate when interpreting time saved. For GHCR, set `BENCH_CACHE_REGISTRY`,
`GH_TOKEN`, `GITHUB_ACTOR` and `GITHUB_REPOSITORY` only when authorized to publish.
Pipeline measurements require registry access;
an absent prepared image fails before compilation or boot, naming the preparation
workflow. They never fall back to apt or toolchain installation. An absent marker
inside a purportedly prepared guest also fails rather than silently setting up
Debian. A missing test bundle may be built natively outside QEMU.

Preparation and measurement are separate workflows: a preparation job is not a
required predecessor queued ahead of every warm VPS run. On first use, wait for
the preparation workflow to publish both images, then rerun the measurement
workflow once. A missing-image failure before that is a prerequisite failure,
not an emulator benchmark. Single-test measurements have a 10-minute guest
workload deadline and 30-minute job limit. Single-test preparation has a
30-minute guest setup/test deadline and a 45-minute job limit. Do not stretch
these single-test deadlines to repeat the failed cold experiment; if preparation
or a warm single test is still impractical, stop the pilot instead of rolling
it out. Full preparation retains its existing 7,200-second guest workload
deadline and uses a 180-minute job limit, matching the full benchmark jobs.
The outer limit covers preparation, publication and the separate warm-overlay
verification; the old 45-minute limit cancelled progressing ARM64 full tests
in run 45091356059 before publication. This is not full-workload success or
permission to skip a test, publish a live disk or provision during warm trials.

An always-run host step restores ownership of the generated diagnostic and
image directories before artifact upload, without following symlink roots or
recursive symlink targets. A cancelled container can miss its own exit trap
and leave root-owned private directories; the host recovery preserves those
logs for upload even then. Ownership-recovery errors remain visible failures.

## Running

### Building the reusable environment locally

Start a Docker-compatible daemon and fetch pinned siblings. The build context
contains only provisioning files, not the checkout, credentials or test output:

```sh
chore siblings
bash scripts/ci-qemu-environment.sh reference aarch64
bash scripts/ci-qemu-environment.sh build aarch64
# Export an authorized GH_TOKEN and GITHUB_ACTOR without putting secrets in logs.
bash scripts/ci-qemu-environment.sh publish aarch64
```

Use `x86_64` for a native x86_64 builder; `arm64`/`amd64` aliases are accepted.
Do not use a cross-architecture container to measure compiler performance.
`publish` returns the immutable registry digest. A local classic PAT needs
package read/write access; the preparation workflow instead uses its scoped
`GITHUB_TOKEN`. Expired credentials, permission errors and failed downloads are
not cache misses and do not trigger runtime installation.

The preparation workflow first publishes the x86_64 environment on a VPS runner.
Its native ARM64 preparation job builds/publishes the ARM64 environment on its
ARM64 host. Warm benchmark jobs only resolve/pull it and run
`scripts/ci-qemu-runner.sh`. They do not run apt, rustup or environment setup.
The remaining host bootstrap is checkout, the pinned chore download and sibling
fetch, plus Docker and (when requested) usable `/dev/kvm`. First-use image pulls,
guest boot and application compilation/cache restore remain measured work.
The container does not replace the ephemeral GitHub Actions host itself.

Host bootstrap is measured from the first GitHub job-step timestamp to
`result.json`'s `started_unix`, before the guest trial begins. Subtract the
separately retained `runner-image-pull-seconds.txt` to distinguish checkout,
contract checks, the chore download, sibling fetch and container launch from
the OCI image pull. Guest image download, native build/cache restore, boot and
test execution have separate evidence files; they are not counted as eliminated
setup time.

For a local Apple Silicon proof without registry credentials, explicitly select
a private local artifact directory and the already-built native compiler image:

```sh
export BENCH_CACHE_DIRECTORY="$PWD/tmp/qemu-local-cache"
export BENCH_RUNNER_IMAGE="$(bash scripts/ci-qemu-environment.sh local aarch64)"
export QEMU_ARM_FIRMWARE=/opt/homebrew/share/qemu/edk2-aarch64-code.fd
bash scripts/ci-qemu-benchmark.sh prepare-local arm64 hvf single
bash scripts/ci-qemu-benchmark.sh run arm64 hvf single
```

This caches a cleanly powered-off guest and test bundle locally. `prepare-local`
is refused in GitHub Actions; only this explicit local operation permits
one-time cross-architecture TCG provisioning. Local storage is not evidence of
GHCR publication. To publish its artifacts later, unset `BENCH_CACHE_DIRECTORY`
and pass each `files/` directory, key, kind and filenames to
`scripts/ci-qemu-cache.sh push`. Do not publish a live guest disk. Source changes
rebuild the executable bundle but reuse the provisioned runner and guest keys.

### Running measured trials

The `QEMU runner benchmark (pilot)` workflow runs when its implementation changes
in an owner-authored PR. Once available on the default branch, it can also be
dispatched with `workload=single`, `workload=smoke` or `workload=full`. Each trial starts independently
and runs the QEMU argument regression tests itself. VPS trials explicitly request
`antimatter-vps-linux-x64`; hosted trials request `ubuntu-24.04`. There is no hosted
bootstrap job delaying the VPS trials, and no fallback that could invalidate a
placement comparison. A full or unavailable VPS pool leaves its trials queued
until a matching runner is available; it does not silently send them to hosted.
This fixed placement is specific to the controlled benchmark, not a replacement
for production autobalancing. The required CI workflow and its gate stay unchanged.

For a local trial, install QEMU, `cloud-image-utils` (or use macOS `hdiutil`),
Python 3.11+, OpenSSH, ORAS 1.3.0, registry access, a running Docker-compatible
daemon for test-bundle misses, and ARM64 UEFI firmware for an ARM64 guest. Check out
the pinned core sibling with `chore siblings`, then run:

```sh
bash scripts/test-ci-qemu-benchmark.sh
bash scripts/ci-qemu-benchmark.sh run x86_64 kvm smoke
bash scripts/ci-qemu-benchmark.sh run x86_64 tcg smoke
bash scripts/ci-qemu-benchmark.sh run aarch64 tcg smoke
```

`QEMU_ARM_FIRMWARE` can name the firmware file; on Homebrew it is typically
`/opt/homebrew/share/qemu/edk2-aarch64-code.fd`. KVM is explicit and fails if it
is unavailable; TCG never requires `/dev/kvm`. Guests use loopback-only SSH,
one ephemeral key, and isolated known-hosts files. The driver stops only its
own QEMU child and removes its guest overlay, seed, and private key on exit.
It never stops or modifies an existing harness guest.

Native compiler progress, guest setup and executed tests stream to the job log. A
30-second heartbeat records elapsed time, phase, QEMU CPU/RSS and the latest
console line. Separate source revisions do not share a concurrency lock, so a
corrective trial cannot cancel or block behind an older measurement.

On an ARM64 VPS trial, the required `/boot/efi` device timed out and systemd
entered emergency mode; slow emulated device discovery is a suspected cause,
not yet a confirmed explanation. Cloud-init now attempts
one bounded recovery: settle device discovery, restart the actual EFI mount,
verify its mountpoint, and resume the normal boot target. A recovery failure is
fatal, not a skipped mount. Readiness also requires the normal multi-user target
and, for ARM64, the mounted EFI filesystem. Recovery behavior has offline
regression coverage; a new VPS trial is required to prove it resolves that
guest's observed failure. The recovery time remains part of the measurement.

The follow-up VPS ARM64 trial mounted EFI without recovery but exceeded the
driver's original 900-second boot deadline while cloud-init was still running.
Its 3-second SSH probes timed out even though a separate longer connection
authenticated and retrieved the guest journal. TCG trials now have a bounded
1,800-second boot allowance; KVM keeps 900 seconds. SSH allows 30 seconds for
connection setup, and each complete readiness probe is capped at 120 seconds
or the remaining boot budget, whichever is smaller. A reachable but not-ready
guest reports cloud-init status and relevant systemd journals into
`boot-probes.log`. The ready marker, multi-user target, and required EFI mount
remain mandatory. More time is an explicit allowance for the measured slow
boot, not evidence that the original EFI recovery works (#640).

Host identity, QEMU version, source commits and `boot-policy.txt` are recorded
before or during boot so they survive a readiness failure. Regressions exercise
a progressing TCG boot beyond 900 seconds, KVM's unchanged deadline, a guest
that never becomes ready, and a readiness command that hangs.

## Evidence and decision

Artifacts retain guest and host identities, the exact QEMU command, boot time,
setup/toolchain/fixture/native-compile/test timings, GNU time max RSS, sampled QEMU RSS,
console and failure logs, and every passing test name. The report compares test
sets and source/toolchain identities before accepting a comparison. It also
independently reconciles raw test summaries with every passing-name occurrence,
preserves the workload's existing executed-test floor, and rejects failed or
ignored tests, filtered full workloads and empty or missing test evidence.
Single workloads must execute exactly one test. Per-trial passed, failed and
ignored counts accompany the timing table; matching empty files are not proof.
Source cleanliness must be explicitly recorded as JSON `false`; missing, null
or non-boolean evidence cannot prove that a commit describes the tested checkout.
Hosted evidence must identify both `ubuntu-24.04` and `github-hosted`; a VPS
runner or missing placement metadata cannot count as the hosted comparison.

The primary metric is **time from workflow submission to a verified result**,
not raw CPU speed. Job `started_at` can precede runner allocation, so queue
measurement uses workflow creation and the first actual step timestamp instead.
Reported guest runtime includes download, boot and setup; compile/test times
are also available separately. A slower emulator wins when the avoided queue
time exceeds its additional execution time. Failed, cancelled, expired-reservation,
or hosted-fallback trials are not successful VPS evidence.

Run repeated paired smoke trials, then full workloads on both architectures.
Only consider moving the required jobs or extracting a generic runner interface
after their full tests pass within the runner's memory, disk and time limits.
Keep guest architecture and test scope invariant. The eventual interface should
separate guest architecture from acceleration (`kvm` or `tcg`) so pipelines run
the same workload without depending on a particular runner label.

## Shared-framework extraction

The implementation remains an ext4 pilot, not a universal image containing every
consumer's dependencies. Issue #643 tracks extraction after full-workload
validation. The shared layer should own recipe keys, immutable artifact
publication/resolution, QEMU acceleration and lifecycle, fresh guest overlays and
identity, bounded execution, progress and measurements. Consumers should declare
their package/toolchain versions and provisioning inputs, fixture recipes, build
and test commands, test floors and oracle policy. A different Go or Rust version
is a different declared recipe, not a runtime installation or a silent fallback.

Choose the shared repository and interface before migrating consumers. Do not
copy the pilot into each repository or remove the current implementation before
the shared replacement preserves the same test and security contracts.
