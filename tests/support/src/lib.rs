//! Shared helpers for the ext4 test suite: where scratch files live,
//! where fixtures come from, and the three outside opinions this suite
//! holds itself to — e2fsprogs (see [`oracle`]), the Linux kernel (see
//! [`kernel`]) and lwext4, a third implementation (see [`lwext4`]).

mod kernel;
mod lwext4;
mod oracle;
pub mod verdict;

pub use kernel::{
    guest_kernel_report, guest_kernel_stopped, guest_kernel_try_write, guest_kernel_write,
    sha256_hex, Stopped,
};
pub use lwext4::{
    lwext4_refusal, lwext4_report, lwext4_write, Report, PATCH as LWEXT4_PATCH, PIN as LWEXT4_PIN,
};
pub use oracle::{guest_base64, guest_quote, oracle, Oracle};
pub use verdict::{Judge, Judged, Verdict};

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_TEMP_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The scratch root: `<repo>/tmp`, or `FS_EXT4_TEST_TMPDIR` when a
/// caller supplies an exact directory of its own.
///
/// ONE RULE, AND IT IS THE ORACLE'S. Scratch files are what the oracle
/// tools read, and those tools run in the harness VM, which sees this
/// repository mounted at the path the host knows it by — and nothing
/// else of the host. A scratch directory under `/tmp` or `$RUNNER_TEMP`
/// would not exist there. So it lives in the repository (gitignored),
/// on every machine and on CI alike, and a caller-supplied directory
/// outside the repository is refused rather than quietly breaking every
/// oracle test.
#[track_caller]
pub fn select_temp_dir(explicit: Option<&OsStr>, worktree: &Path) -> PathBuf {
    let Some(path) = explicit.filter(|path| !path.is_empty()) else {
        return worktree.join("tmp");
    };
    let path = PathBuf::from(path);
    assert!(
        path.starts_with(worktree),
        "FS_EXT4_TEST_TMPDIR is {}, which is outside {}. The oracle tools run in the \
         harness VM, which sees this repository and nothing else of the host, so scratch \
         files have to live inside it.",
        path.display(),
        worktree.display()
    );
    path
}

/// Create a collision-resistant per-process scratch directory below `base`.
#[doc(hidden)]
pub fn create_unique_temp_dir(base: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(base)?;
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..1_024_u16 {
        let candidate = base.join(format!(
            "fs-ext4-tests.{}.{}.{}",
            std::process::id(),
            started,
            attempt
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "cannot allocate unique scratch directory below {}",
            base.display()
        ),
    ))
}

/// Preserve an explicit directory or isolate a process beneath a selected root.
#[doc(hidden)]
pub fn materialize_temp_dir(explicit: Option<&OsStr>, root: &Path) -> io::Result<PathBuf> {
    if explicit.filter(|path| !path.is_empty()).is_some() {
        fs::create_dir_all(root)?;
        Ok(root.to_path_buf())
    } else {
        create_unique_temp_dir(root)
    }
}

/// Return the shared scratch directory for this integration-test process.
pub fn temp_dir() -> &'static Path {
    TEST_TEMP_DIR
        .get_or_init(|| {
            let support_crate = Path::new(env!("CARGO_MANIFEST_DIR"));
            let worktree = support_crate
                .parent()
                .and_then(Path::parent)
                .expect("test support crate must live at <worktree>/tests/support");
            let explicit = std::env::var_os("FS_EXT4_TEST_TMPDIR");
            let selected_root = select_temp_dir(explicit.as_deref(), worktree);
            let dir =
                materialize_temp_dir(explicit.as_deref(), &selected_root).unwrap_or_else(|error| {
                    panic!(
                        "cannot create ext4 test scratch directory below {}: {error}",
                        selected_root.display()
                    )
                });
            if explicit.filter(|path| !path.is_empty()).is_none() {
                remove_at_exit(dir.clone());
            }
            dir
        })
        .as_path()
}

/// The per-process directory [`temp_dir`] created, removed when the
/// process exits.
///
/// Under `scripts/test.sh` the runner names the directory and removes it
/// itself, so there is nothing here to do. A plain `cargo test` names
/// none, each test binary creates `tmp/fs-ext4-tests.<pid>.*`, and before
/// this every run left one behind per binary. Rust runs no destructor
/// for a static, so the removal is an `atexit` handler: libtest returns
/// from `main` whether its tests passed or failed, and the C runtime
/// runs the handler then. A process killed by a signal leaves its
/// directory, as it would leave anything else.
///
/// `RFE_KEEP_IMAGES` asks for what a run wrote to survive it, so with it
/// set only an EMPTY directory is removed.
fn remove_at_exit(dir: PathBuf) {
    static OWNED: OnceLock<PathBuf> = OnceLock::new();
    if OWNED.set(dir).is_err() {
        return;
    }
    extern "C" fn remove_owned_scratch() {
        let Some(dir) = OWNED.get() else { return };
        if std::env::var_os("RFE_KEEP_IMAGES").is_some() {
            let _ = fs::remove_dir(dir);
        } else {
            let _ = fs::remove_dir_all(dir);
        }
    }
    extern "C" {
        fn atexit(callback: extern "C" fn()) -> std::os::raw::c_int;
    }
    // SAFETY: `atexit` is the C runtime's, takes a plain function with no
    // arguments, and is called from one place, once per process.
    let registered = unsafe { atexit(remove_owned_scratch) };
    assert_eq!(
        registered, 0,
        "atexit refused the scratch-directory cleanup"
    );
}

/// Format a test filename beneath the selected scratch directory.
#[doc(hidden)]
pub fn formatted_temp_path(arguments: fmt::Arguments<'_>) -> String {
    temp_dir()
        .join(arguments.to_string())
        .to_string_lossy()
        .into_owned()
}

#[macro_export]
macro_rules! temp_path {
    ($($argument:tt)*) => {
        $crate::formatted_temp_path(format_args!($($argument)*))
    };
}

/// The path of a generated fixture under `test-disks/`, or a panic that
/// says how to build it (#137).
///
/// The images are gitignored and built by `chore fixtures` (the kernel
/// populates them, inside the fs-linux-test-harness VM). THE ONLY WAY A
/// TEST REACHES A FIXTURE: a test that found its image absent used to
/// print "skip" and return, and a skipped test reads exactly like a
/// passing one, so a checkout without fixtures ran most of the suite
/// against nothing and reported green. `chore test:unit` also relies on
/// this: a test binary that never calls it needs no fixture.
#[track_caller]
pub fn fixture(manifest_dir: &str, name: &str) -> String {
    let path = format!("{manifest_dir}/test-disks/{name}");
    assert!(
        Path::new(&path).is_file(),
        "test-disks/{name} is missing: the fixtures are gitignored and generated. \
         Build them with `chore fixtures` (it boots the fs-linux-test-harness VM; \
         `chore siblings` checks the harness out) and run the tests again. \
         Tests never skip on a missing fixture."
    );
    path
}

const IMAGE_COMPARE_BUFFER_SIZE: usize = 1024 * 1024;
static IMAGE_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(0);

#[track_caller]
pub fn snapshot_file(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let snapshot = PathBuf::from(temp_path!(
        "image-snapshot-{}-{}.img",
        std::process::id(),
        IMAGE_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(error) = copy_file_sparse(path, &snapshot) {
        let _ = fs::remove_file(&snapshot);
        return Err(error);
    }
    Ok(snapshot)
}

pub fn copy_file_sparse(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> io::Result<()> {
    let mut source = fs::File::open(source)?;
    let length = source.metadata()?.len();
    let mut destination = fs::File::create(destination)?;
    let mut buffer = vec![0; IMAGE_COMPARE_BUFFER_SIZE];
    let mut offset = 0u64;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        if chunk.iter().any(|byte| *byte != 0) {
            destination.seek(SeekFrom::Start(offset))?;
            destination.write_all(chunk)?;
        }
        offset += count as u64;
    }
    destination.set_len(length)
}

pub fn first_different_offset(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
) -> io::Result<Option<u64>> {
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let left_len = left.metadata()?.len();
    let right_len = right.metadata()?.len();
    let compare_len = left_len.min(right_len);
    let mut left_buffer = vec![0; IMAGE_COMPARE_BUFFER_SIZE];
    let mut right_buffer = vec![0; IMAGE_COMPARE_BUFFER_SIZE];
    let mut offset = 0u64;
    while offset < compare_len {
        let count = (compare_len - offset).min(IMAGE_COMPARE_BUFFER_SIZE as u64) as usize;
        left.read_exact(&mut left_buffer[..count])?;
        right.read_exact(&mut right_buffer[..count])?;
        if let Some(difference) = left_buffer[..count]
            .iter()
            .zip(&right_buffer[..count])
            .position(|(left, right)| left != right)
        {
            return Ok(Some(offset + difference as u64));
        }
        offset += count as u64;
    }
    Ok((left_len != right_len).then_some(compare_len))
}

#[track_caller]
pub fn assert_files_equal(left: impl AsRef<Path>, right: impl AsRef<Path>) {
    let left_path = left.as_ref();
    let right_path = right.as_ref();
    match first_different_offset(left_path, right_path) {
        Ok(None) => {}
        Ok(Some(offset)) => panic!(
            "{} differs from {} at byte offset {offset}",
            left_path.display(),
            right_path.display()
        ),
        Err(error) => panic!(
            "cannot compare {} and {}: {error}",
            left_path.display(),
            right_path.display()
        ),
    }
}

#[track_caller]
pub fn read_small_file(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    let length = fs::metadata(path)?.len();
    if length > IMAGE_COMPARE_BUFFER_SIZE as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{} is {length} bytes; read_small_file is limited to 1 MiB",
                path.display()
            ),
        ));
    }
    fs::read(path)
}

#[track_caller]
pub fn read_file_range(path: impl AsRef<Path>, offset: u64, length: usize) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    let mut file = fs::File::open(path)?;
    let end = offset
        .checked_add(length as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file range overflows u64"))?;
    if end > file.metadata()?.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("range {offset}..{end} exceeds {}", path.display()),
        ));
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// `e2fsck -fn` on `image` must examine it and find nothing, or the test
/// fails with its report (#88, #280).
///
/// The oracle suites checked their images with this crate's own reader,
/// which cannot see a wrong checksum. `-f` forces a full check, `-n`
/// answers no to every repair, so it reports without touching the image.
/// It runs in the harness VM, like every oracle tool (see [`oracle`]).
///
/// EXIT 0 IS NOT THE VERDICT. `e2fsck -fn` exits 0 having answered "no"
/// to `Free blocks count wrong`, and exits 0 having skipped the replay of
/// a journal it then graded around. [`Judged::clean`] reads the report.
#[track_caller]
pub fn assert_e2fsck_clean(image: &str, tag: &str) {
    oracle("e2fsck").args(["-fn", image]).judged().clean(tag);
}
