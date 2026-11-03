use std::collections::BTreeMap;

fn methods(source: &str, marker: &str, end: &str) -> BTreeMap<String, String> {
    source
        .match_indices(marker)
        .filter_map(|(start, _)| {
            let rest = &source[start + marker.len()..];
            let signature_end = rest.find('{')?;
            let signature = &rest[..signature_end];
            let name_start = signature.find("fn ")? + 3;
            let name = signature[name_start..].split('(').next()?.trim();
            let body = rest[signature_end + 1..].split(end).next()?;
            let body: String = body
                .lines()
                .map(|line| line.split("//").next().unwrap())
                .flat_map(str::chars)
                .filter(|character| !character.is_whitespace())
                .collect();
            Some((name.to_owned(), body))
        })
        .collect()
}

fn apply_calls(body: &str) -> Vec<&str> {
    body.match_indices(".apply_")
        .map(|(start, _)| {
            body[start + 1..]
                .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                .next()
                .unwrap()
        })
        .collect()
}

fn locked_entry(name: &str, entries: &BTreeMap<String, String>) -> bool {
    let mut current = name;
    for _ in 0..entries.len() {
        let Some(body) = entries.get(current) else {
            return false;
        };
        if body.starts_with("let_write_guard=self.lock_write()?;") {
            return true;
        }
        let calls = apply_calls(body);
        if !body.starts_with("self.apply_") || body.contains(';') || calls.len() != 1 {
            return false;
        }
        current = calls[0];
        if current.ends_with("_locked") {
            return false;
        }
    }
    false
}

fn rust_violations(source: &str) -> Vec<String> {
    let entries = methods(source, "\n    ", "\n    }");
    let public = methods(source, "\n    pub", "\n    }");
    let mut failures = Vec::new();
    for (name, body) in entries
        .iter()
        .filter(|(name, _)| name.starts_with("apply_"))
    {
        let owns_lock = public.contains_key(name) && !name.ends_with("_locked");
        if owns_lock && !locked_entry(name, &entries) {
            failures.push(format!(
                "{name} must lock before any work or only delegate to a locking entry"
            ));
        }
        let holds_lock = !owns_lock || body.starts_with("let_write_guard=self.lock_write()?;");
        if holds_lock {
            for called in apply_calls(body) {
                if locked_entry(called, &entries) {
                    failures.push(format!("{name} reacquires the write lock through {called}"));
                }
            }
        }
    }
    failures
}

#[test]
fn every_apply_entry_locks_before_staging_without_relocking() {
    let source = include_str!("../src/fs.rs")
        .split("\n#[cfg(test)]\n")
        .next()
        .unwrap();
    let entries = methods(source, "\n    ", "\n    }");
    assert!(
        entries
            .keys()
            .filter(|name| name.starts_with("apply_"))
            .count()
            >= 50
    );
    let failures = rust_violations(source);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_guard_rejects_new_unlocked_paths_and_nested_locking() {
    let source = "
    pub fn apply_create(&self) {
        let _write_guard = self.lock_write()?;
        self.apply_create_locked()
    }
    fn apply_create_locked(&self) {
        self.commit()
    }
    pub fn apply_wrapper(&self) {
        self.apply_create()
    }
    pub fn apply_unlocked(&self) {
        self.stage();
        self.commit()
    }
    pub fn apply_late_lock(&self) {
        self.stage();
        let _write_guard = self.lock_write()?;
        self.commit()
    }
    pub fn apply_nested(&self) {
        let _write_guard = self.lock_write()?;
        self.apply_wrapper()
    }
    pub fn apply_bypass(&self) {
        self.apply_create_locked()
    }
";
    let failures = rust_violations(source);
    assert_eq!(failures.len(), 4, "{failures:?}");
    for name in [
        "apply_unlocked",
        "apply_late_lock",
        "apply_nested",
        "apply_bypass",
    ] {
        assert!(
            failures.iter().any(|failure| failure.starts_with(name)),
            "{name}: {failures:?}"
        );
    }
}

fn capi_violations(source: &str) -> (usize, Vec<String>) {
    let mut count = 0;
    let mut failures = Vec::new();
    for (name, body) in methods(source, "\npub unsafe extern \"C\" ", "\n}") {
        let calls = apply_calls(&body);
        count += calls.len();
        for called in calls {
            if called.ends_with("_locked") {
                let before = body.split(&format!(".{called}(")).next().unwrap();
                if !before.contains(".lock_write()") {
                    failures.push(format!(
                        "{name} calls {called} without holding the writer lock"
                    ));
                }
                if let Some((before_lock, _)) = before.split_once(".lock_write()") {
                    if before_lock.contains("resolve_path(") {
                        failures.push(format!("{name} resolves the target before locking"));
                    }
                }
            } else if body.contains(".lock_write()") {
                failures.push(format!(
                    "{name} reacquires the writer lock through {called}"
                ));
            } else if body.contains("resolve_path(")
                && (called.ends_with("_ino") || called.ends_with("_at"))
            {
                failures.push(format!("{name} resolves the target outside the write lock"));
            }
        }
    }
    (count, failures)
}

#[test]
fn capi_writes_use_locking_entries_or_hold_the_lock_before_resolution() {
    let (count, failures) = capi_violations(include_str!("../src/capi.rs"));
    assert!(count >= 20, "the C ABI write scan found only {count} calls");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn the_capi_guard_rejects_an_unlocked_internal_entry() {
    let source = "
pub unsafe extern \"C\" fn fs_ext4_new_write() {
    fs.apply_truncate_ino_locked(ino, size)
}
";
    let (count, failures) = capi_violations(source);
    assert_eq!(count, 1);
    assert_eq!(failures.len(), 1);
}

#[test]
fn the_capi_guard_rejects_resolution_before_locking_and_nested_locking() {
    let source = "
pub unsafe extern \"C\" fn fs_ext4_late_lock() {
    let ino = resolve_path(fs, path)?;
    let _write_guard = fs.lock_write()?;
    fs.apply_truncate_ino_locked(ino, size)
}
pub unsafe extern \"C\" fn fs_ext4_nested_lock() {
    let _write_guard = fs.lock_write()?;
    fs.apply_truncate_ino(ino, size)
}
pub unsafe extern \"C\" fn fs_ext4_unlocked_resolution() {
    let ino = resolve_path(fs, path)?;
    fs.apply_truncate_ino(ino, size)
}
";
    let (count, failures) = capi_violations(source);
    assert_eq!(count, 3);
    assert_eq!(failures.len(), 3, "{failures:?}");
}
