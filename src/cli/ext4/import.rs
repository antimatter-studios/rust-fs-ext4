//! Single-file import over core 0.3.14's bounded writer destination.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use fs_core::cli::{transfer, CliError, Json, Outcome};
use fs_ext4::features::Compat;
use fs_ext4::inode::InodeFlags;
use fs_ext4::runtime::process_id;
use fs_ext4::{Filesystem, InodeRef};

fn error(e: impl std::fmt::Display) -> CliError {
    CliError::failed(e.to_string())
}

pub(crate) struct LocalSource {
    file: File,
    size: u64,
}

impl LocalSource {
    pub(crate) fn open(path: &OsStr) -> Result<Self, CliError> {
        if !std::fs::symlink_metadata(path).map_err(error)?.is_file() {
            return Err(error(
                "import source must be a regular file; symlinks are not followed",
            ));
        }
        let file = File::open(path).map_err(error)?;
        let size = file.metadata().map_err(error)?.len();
        Ok(Self { file, size })
    }

    fn stdin() -> Result<Self, CliError> {
        // The released shared pump requires a known length. Spool to disk,
        // retaining bounded memory. The anonymous file obeys TMPDIR, which the
        // CLI tests set from their scratch selector, and closes without residue.
        let mut file = tempfile::tempfile().map_err(error)?;
        let size = std::io::copy(&mut std::io::stdin().lock(), &mut file).map_err(error)?;
        file.seek(SeekFrom::Start(0)).map_err(error)?;
        Ok(Self { file, size })
    }
}

impl transfer::FileSource for LocalSource {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, CliError> {
        self.file.seek(SeekFrom::Start(offset)).map_err(error)?;
        self.file.read(buf).map_err(error)
    }
}

/// A transaction with an externally visible, recognisable staging name.
/// Persistence closes the writable mount before verification or publication.
pub(crate) struct Stage {
    target: OsString,
    offset: u64,
    dir: InodeRef,
    name: Vec<u8>,
    destination: Vec<u8>,
    inode: InodeRef,
    fs: Option<Filesystem>,
    written: u64,
    replaced: bool,
    published: bool,
}

impl Stage {
    pub(crate) fn create(
        target: &OsStr,
        offset: u64,
        path: &[u8],
        replace: bool,
    ) -> Result<Self, CliError> {
        let split = path.iter().rposition(|&b| b == b'/').map_or(0, |n| n + 1);
        let (parent, destination) = path.split_at(split);
        if destination.is_empty()
            || destination == b"."
            || destination == b".."
            || destination.len() > 255
            || destination.contains(&0)
        {
            return Err(error(
                "import PATH must name a file, without a trailing slash",
            ));
        }
        let target = target.to_os_string();
        let fs = super::device::mount(&target, offset, false)?;
        let dir = fs.lookup_dir_path_bytes(parent).map_err(error)?;
        let parent_inode = fs.stat_ino(dir).map_err(error)?;
        if !parent_inode.is_dir() {
            return Err(error("import parent is not a directory"));
        }
        // A direct-write volume cannot promise atomic publication.
        if fs.sb.feature_compat & Compat::HAS_JOURNAL.bits() == 0
            || fs.sb.journal_inode == 0
            || !fs.flavor.uses_extents()
        {
            return Err(CliError::not_implemented(
                "bounded atomic import needs an ext4 volume with an internal journal",
            ));
        }
        let replaced = match fs.lookup_at(dir, destination) {
            Ok(ino) => {
                if !replace {
                    return Err(error("destination exists; give --replace to replace it"));
                }
                let inode = fs.stat_ino(ino).map_err(error)?;
                if !inode.is_file()
                    || inode.flags & (InodeFlags::IMMUTABLE | InodeFlags::APPEND).bits() != 0
                {
                    return Err(error("import can replace only a mutable regular file"));
                }
                true
            }
            Err(fs_ext4::Error::NotFound) => false,
            Err(e) => return Err(error(e)),
        };
        let dir = InodeRef::new(dir, parent_inode.generation);
        drop(fs);
        let fs = super::device::mount(&target, offset, true)?;
        for n in 0..1000 {
            let name = format!(".dj-import-{}-{n}.partial", process_id()).into_bytes();
            if name == destination {
                continue;
            }
            let ino = match fs.apply_create_at(dir, &name, 0o600) {
                Ok(ino) => ino,
                Err(fs_ext4::Error::AlreadyExists) => continue,
                Err(e) => return Err(error(e)),
            };
            let generation = fs.stat_ino(ino).map_err(error)?.generation;
            return Ok(Self {
                target,
                offset,
                dir,
                name,
                destination: destination.to_vec(),
                inode: InodeRef::new(ino, generation),
                fs: Some(fs),
                written: 0,
                replaced,
                published: false,
            });
        }
        Err(error("no free import staging name"))
    }

    pub(crate) fn copy(
        &mut self,
        source: &mut dyn transfer::FileSource,
    ) -> Result<transfer::Copied, CliError> {
        let copied = transfer::export_file(source, transfer::Destination::Writer(self))?;
        self.fs
            .take()
            .expect("live staging mount")
            .finish()
            .map_err(error)?;
        Ok(copied)
    }

    fn verify(&self, copied: &transfer::Copied) -> Result<(), CliError> {
        let fs = super::device::mount(&self.target, self.offset, false)?;
        let size = fs.stat_ino(self.inode).map_err(error)?.size;
        let mut source = StagedSource {
            fs: &fs,
            inode: self.inode,
            size,
        };
        let readback = transfer::export_file(
            &mut source,
            transfer::Destination::Writer(&mut std::io::sink()),
        )?;
        if readback.bytes != copied.bytes || readback.sha256 != copied.sha256 {
            return Err(error("staged import verification failed"));
        }
        Ok(())
    }

    fn publish(&mut self, replace: bool) -> Result<(), CliError> {
        let fs = super::device::mount(&self.target, self.offset, true)?;
        fs.apply_chmod_ino(self.inode, 0o644).map_err(error)?;
        // Same-directory rename: the destination entry and the old inode's
        // reclamation share one journal transaction (apply_rename_at).
        fs.apply_rename_at(self.dir, &self.name, self.dir, &self.destination, replace)
            .map_err(error)?;
        self.published = true;
        fs.finish()
            .map_err(|e| error(format!("import published, but finishing failed: {e}")))
    }

    fn abort(&mut self) -> Result<(), CliError> {
        self.fs.take(); // retire a mount after any write failure
        let fs = super::device::mount(&self.target, self.offset, true)?;
        match fs.apply_unlink_at(self.dir, &self.name) {
            Ok(()) | Err(fs_ext4::Error::NotFound) => {}
            Err(e) => return Err(error(e)),
        }
        fs.finish().map_err(error)?;
        self.published = true; // cleanup is complete
        Ok(())
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.published {
            let _ = self.abort();
        }
    }
}

impl Write for Stage {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.fs
            .as_ref()
            .expect("live staging mount")
            .apply_pwrite_ino(self.inode, self.written, buf)
            .map_err(std::io::Error::other)?;
        self.written += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.fs
            .as_ref()
            .expect("live staging mount")
            .flush()
            .map_err(std::io::Error::other)
    }
}

struct StagedSource<'a> {
    fs: &'a Filesystem,
    inode: InodeRef,
    size: u64,
}
impl transfer::FileSource for StagedSource<'_> {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, CliError> {
        self.fs.read_ino(self.inode, offset, buf).map_err(error)
    }
}

pub(crate) fn run(
    target: &OsString,
    offset: u64,
    path: &[u8],
    src: &OsString,
    replace: bool,
    verify: bool,
) -> Result<Outcome, CliError> {
    let mut source = if src == "-" {
        None
    } else {
        Some(LocalSource::open(src)?)
    };
    if let Some(source) = &source {
        let input =
            same_file::Handle::from_file(source.file.try_clone().map_err(error)?).map_err(error)?;
        let image = same_file::Handle::from_path(target).map_err(error)?;
        if input == image {
            return Err(error("import source aliases the backing image"));
        }
    }
    let mut stage = Stage::create(target, offset, path, replace)?;
    let result: Result<Outcome, CliError> = (|| {
        if source.is_none() {
            source = Some(LocalSource::stdin()?);
        }
        let copied = stage.copy(source.as_mut().expect("opened source"))?;
        if verify {
            stage.verify(&copied)?;
        }
        stage.publish(replace)?;
        let text = format!(
            "imported {} bytes to {} (sha256 {})",
            copied.bytes,
            String::from_utf8_lossy(path),
            copied.sha256_hex()
        );
        Ok(Outcome::report(Json::object([
            ("path", Json::from(String::from_utf8_lossy(path).as_ref())),
            ("bytes", Json::from(copied.bytes)),
            ("sha256", Json::from(copied.sha256_hex())),
            ("replaced", Json::from(stage.replaced)),
            ("verified", Json::from(verify)),
        ]))
        .with_text(text))
    })();
    if result.is_err() && !stage.published {
        if let Err(cleanup) = stage.abort() {
            return Err(error(format!(
                "{}; cleanup failed: {}; staging name {} remains",
                result.expect_err("failed import").message,
                cleanup.message,
                String::from_utf8_lossy(&stage.name)
            )));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_core::cli::transfer::FileSource;
    use std::process::{Command, Stdio};

    #[test]
    fn stdin_spool_uses_selected_scratch_without_named_residue() {
        const CHILD: &str = "EXT4_STDIN_SPOOL_TEST";
        if let Some(mode) = std::env::var_os(CHILD) {
            let result = LocalSource::stdin();
            if mode == "missing" {
                assert!(result.is_err(), "a missing scratch directory must fail");
                return;
            }
            let mut source = result.expect("spool stdin in selected scratch");
            assert_eq!(source.size(), 11);
            let mut bytes = [0; 11];
            assert_eq!(source.read_at(0, &mut bytes).unwrap(), bytes.len());
            assert_eq!(&bytes, b"stdin bytes");
            let scratch = std::env::var_os("TMPDIR").unwrap();
            assert_eq!(std::fs::read_dir(scratch).unwrap().count(), 0);
            return;
        }
        let scratch = std::path::PathBuf::from(fs_ext4_test_support::temp_path!(
            "stdin-spool-{}",
            process_id()
        ));
        std::fs::create_dir(&scratch).unwrap();
        for (mode, directory) in [
            ("selected", scratch.clone()),
            ("missing", scratch.join("missing")),
        ] {
            let this_binary = std::env::current_exe().unwrap();
            let mut child = Command::new(this_binary)
                .arg("stdin_spool_uses_selected_scratch_without_named_residue")
                .env(CHILD, mode)
                .env("TMPDIR", directory)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            if mode == "selected" {
                stdin.write_all(b"stdin bytes").unwrap();
            }
            drop(stdin);
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
        }
        std::fs::remove_dir(scratch).unwrap();
    }
}
