<!--
  PR template for rust-fs-ext4. Keep PRs scoped: one logical change per PR
  unless the changes are genuinely interlocked. Delete sections that don't
  apply to your change rather than leaving them empty.
-->

## Summary

<!-- One-paragraph description of what this PR changes. Lead with the WHY, not the WHAT — readers can see the diff. -->

## Motivation

<!-- What problem prompted this change? Bug report, audit finding, perf measurement, missing capability for a downstream consumer? Link issues if relevant. -->

## Change shape

<!-- Bullet list of the discrete edits. Helps a reviewer (human or coderabbit) follow the diff. Group by file or by concern, whichever reads cleaner. -->

- 

## Behaviour change

<!-- What did the crate do before? What does it do now? Especially important if any public API, on-disk format, or C ABI surface moved. -->

- Before:
- After:

## Testing

<!-- New tests added? Existing tests that exercise the change? Manual reproduction steps if no automated coverage exists. `cargo test` output snippet is fine. -->

- [ ] `cargo test` passes locally
- [ ] `cargo clippy` clean (or pre-existing lints only)
- [ ] New tests cover the new code path (or "N/A" with rationale)

## ABI / on-disk compatibility

<!-- Tick the boxes that apply, or strike through and explain. -->

- [ ] No change to public Rust API
- [ ] No change to C ABI (`include/fs_ext4.h` shape, struct layouts, function signatures)
- [ ] No change to on-disk format
- [ ] If any of the above DID change, the change is binary-compatible (new fields appended, sentinel values reserved, etc.) — explained below.

## Risk

<!-- What's the worst plausible failure mode if a reviewer misses something here? "Read-only diagnostic — at worst we report a wrong count" is a valid answer. So is "writes to disk; bad logic could corrupt a mounted volume". Be honest. -->

## Checklist before merge

- [ ] Commit messages describe the WHY, not just the WHAT
- [ ] No unrelated changes mixed in (formatting drift, unrelated TODOs)
- [ ] Public docs / module headers updated if behaviour changed
- [ ] CHANGELOG entry if user-visible
