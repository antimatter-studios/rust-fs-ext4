# Building against an unreleased rust-fs-core

`Cargo.toml` and `fuzz/Cargo.toml` name `rust-fs-core` by an exact
published version and no path:

```toml
rust-fs-core = { version = "=0.4.3" }
```

So every build -- on a workstation, in CI and in the harness guest --
compiles the release from crates.io, and a consumer that uses several
drivers from this family gets the one core each of them was tested with.
`tests/ci_profile.rs` holds `Cargo.toml`, both lock files and
`FS_CORE_REF` in `chores.yml` to the same version.

The sibling checkout that `chore siblings` keeps at `../rust-fs-core` is
still needed, for core's scripts (`tier.sh`, `test-floor.sh`,
`ci-gate.sh` and the rest), but cargo does not read it.

## Working on both at once

When a change here needs a core change that has not been released yet,
point cargo at the checkout with a `[patch]` in a local
`.cargo/config.toml`. The file is gitignored and must never be
committed:

```toml
# .cargo/config.toml -- local only
[patch.crates-io]
rust-fs-core = { path = "../rust-fs-core" }
```

A relative path in a cargo config file is read from the directory that
holds `.cargo/`, so this names the sibling on the host and `/rust-fs-core`
in the guest, where `chore test:vm` stages the sibling's committed `HEAD`.

If the unreleased core has a new version number, change the requirement
in both manifests to it as well; the patch satisfies it until the release
exists. The pull request that depends on it merges only after that core
release is published, with the patch file removed and the lock files
resolved against the registry (`cargo update -p rust-fs-core`, and the
same in `fuzz/`).
