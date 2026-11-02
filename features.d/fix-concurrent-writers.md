## Interfaces
| Concurrent callers | Supported: concurrent reads and serialized high-level writes on one mount; unmount and mutable mount controls require exclusive access | Unreleased (#609) | #606 | `capi_concurrency.rs`, `fs::concurrent_writers`, `capi::concurrent_writers` |
