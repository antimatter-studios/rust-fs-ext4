use fs_ext4_test_support::{
    assert_files_equal, copy_file_sparse, first_different_offset,
    first_different_offset_from_bytes, read_file_range, read_small_file, sha256_file, sha256_hex,
    snapshot_file,
};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

fn image(tag: &str, length: u64) -> PathBuf {
    let image = PathBuf::from(fs_ext4_test_support::temp_path!("image-io-{tag}.img"));
    File::create(&image).unwrap().set_len(length).unwrap();
    image
}

fn write_at(image: &Path, offset: u64, bytes: &[u8]) {
    let mut file = OpenOptions::new().write(true).open(image).unwrap();
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn sparse_snapshots_preserve_contents_length_and_independence() {
    let image = image("sparse", 32 * 1024 * 1024 + 17);
    write_at(&image, 1024 * 1024 - 1, b"edge");
    write_at(&image, 32 * 1024 * 1024 + 16, b"z");
    let snapshot = snapshot_file(&image).unwrap();
    assert_files_equal(&image, &snapshot);
    assert_eq!(
        fs::metadata(&snapshot).unwrap().len(),
        32 * 1024 * 1024 + 17
    );
    write_at(&image, 1024 * 1024, b"X");
    assert_eq!(
        first_different_offset(&image, &snapshot).unwrap(),
        Some(1024 * 1024)
    );
    assert_eq!(
        read_file_range(&snapshot, 1024 * 1024 - 1, 4).unwrap(),
        b"edge"
    );
}

#[test]
fn comparisons_report_the_first_difference_across_chunk_boundaries() {
    let image = image("boundary", 2 * 1024 * 1024 + 3);
    let snapshot = snapshot_file(&image).unwrap();
    for offset in [0, 1024 * 1024 - 1, 1024 * 1024, 2 * 1024 * 1024 + 2] {
        write_at(&image, offset, b"X");
        assert_eq!(
            first_different_offset(&image, &snapshot).unwrap(),
            Some(offset)
        );
        write_at(&image, offset, &[0]);
    }
    assert_files_equal(&image, &snapshot);
}

#[test]
fn comparisons_detect_length_mismatches_and_empty_files() {
    let left = image("length-left", 0);
    let right = image("length-right", 0);
    assert_files_equal(&left, &right);
    File::create(&right)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    assert_eq!(first_different_offset(&left, &right).unwrap(), Some(0));
    assert_eq!(first_different_offset(&right, &left).unwrap(), Some(0));
    File::create(&left).unwrap().set_len(1024 * 1024).unwrap();
    assert_eq!(
        first_different_offset(&left, &right).unwrap(),
        Some(1024 * 1024)
    );
}

#[test]
fn sparse_copy_truncates_an_existing_destination_and_preserves_trailing_holes() {
    let source = image("copy-source", 1024 * 1024 + 7);
    write_at(&source, 0, b"head");
    let destination = image("copy-destination", 3 * 1024 * 1024);
    write_at(&destination, 1024 * 1024, b"stale");
    copy_file_sparse(&source, &destination).unwrap();
    assert_files_equal(&source, &destination);
}

#[test]
fn byte_comparisons_detect_content_and_length_mismatches() {
    let image = image("bytes", 0);
    fs::write(&image, b"expected").unwrap();
    assert_eq!(
        first_different_offset_from_bytes(b"expected", &image).unwrap(),
        None
    );
    assert_eq!(
        first_different_offset_from_bytes(b"expXcted", &image).unwrap(),
        Some(3)
    );
    assert_eq!(
        first_different_offset_from_bytes(b"expect", &image).unwrap(),
        Some(6)
    );
    assert_eq!(
        first_different_offset_from_bytes(b"expected!", &image).unwrap(),
        Some(8)
    );
}

#[test]
fn small_file_reads_refuse_disk_images_and_accept_the_documented_boundary() {
    let small = image("small", 3);
    write_at(&small, 0, b"abc");
    assert_eq!(read_small_file(&small).unwrap(), b"abc");
    let boundary = image("small-boundary", 1024 * 1024);
    assert_eq!(read_small_file(&boundary).unwrap().len(), 1024 * 1024);
    let oversized = image("oversized", 1024 * 1024 + 1);
    assert_eq!(
        read_small_file(&oversized).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn range_reads_reject_overflow_and_eof_without_allocating_the_requested_range() {
    let image = image("range", 7);
    write_at(&image, 0, b"payload");
    assert_eq!(read_file_range(&image, 2, 3).unwrap(), b"ylo");
    assert_eq!(read_file_range(&image, 7, 0).unwrap(), b"");
    assert_eq!(
        read_file_range(&image, 6, 2).unwrap_err().kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        read_file_range(&image, u64::MAX, 1).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

#[test]
fn streaming_sha256_matches_known_vectors_and_multichunk_inputs() {
    let image = image("sha256", 0);
    assert_eq!(
        sha256_file(&image).unwrap(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    fs::write(&image, b"abc").unwrap();
    assert_eq!(
        sha256_file(&image).unwrap(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let bytes: Vec<u8> = (0..2 * 1024 * 1024 + 17)
        .map(|index| (index % 251) as u8)
        .collect();
    fs::write(&image, &bytes).unwrap();
    assert_eq!(sha256_file(&image).unwrap(), sha256_hex(&bytes));
}
