### Changed

- **A pull request runs the quick CI profile; the merge queue runs the full one.**
  Every pull request, each layer of a stack included, runs the unit tier,
  lint, semver, msrv and provenance. The merge queue, a push, a tag, a manual
  dispatch and any unknown event run every VM, fixture, oracle, wasm and CLI
  job as before, and `ci-ok` refuses a skip the plan did not make.
