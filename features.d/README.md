# features.d

Rows of [`docs/features.md`](../docs/features.md) that have not been
released yet, one file per pull request. A release applies them to the
page and deletes them, so this directory holds only what the next release
will change ("Keep the features page current" in AGENTS.md).

A fragment is named for its branch -- `fix/run-end-bound` writes
`fix-run-end-bound.md` -- and holds one or more `## Section` headings the
page has, each over rows in that section's columns, `remove: KEY` lines or
`rename: KEY` lines followed by the row that replaces it:

    ## Reading

    | Feature name | Supported | Unreleased (#N) | | `test_file.rs` |

A row's first cell is its key: an existing key replaces that row in place,
a new key is appended to the section. One fragment writes a row; to change
a row another unreleased fragment writes, edit that fragment.

- `../rust-fs-core/scripts/features-check.sh` (run in place, in `chore
  lint`) refuses a fragment that does not apply, and an `Unreleased` row
  written into the page directly.
- `../rust-fs-core/scripts/features-gather.sh VERSION` is the release step.
