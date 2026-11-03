## Interfaces
| Concurrent callers | Supported: concurrent reads and serialized high-level writes on one journaled or journal-less mount; reads are not transaction snapshots; unmount and mutable mount controls require exclusive access | Unreleased (#610) | #606 | `capi_concurrency.rs`, `fs::concurrent_writers`, `capi::concurrent_writers`, `fs::write_serialization`, `write_lock_contract.rs` |
