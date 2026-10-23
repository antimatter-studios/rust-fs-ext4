//! Extended attribute (xattr) reading.
//!
//! Spec: kernel.org/doc/html/latest/filesystems/ext4/dynamic.html#extended-attributes
//!
//! ext4 stores xattrs in two places:
//!
//! 1. **In-inode** — between the end of the base 128-byte inode + extra_isize
//!    region and the end of the on-disk inode (when inode_size > 128).
//!    Starts with a 4-byte header containing the magic 0xEA020000.
//!
//! 2. **External xattr block** — when more space is needed, `i_file_acl`
//!    (combined hi+lo, 48-bit physical block number) points at a single
//!    block whose layout is: 32-byte header (magic 0xEA020000 + refcount
//!    + ...) followed by `ext4_xattr_entry` records growing forward, with
//!    values stored from the END of the block growing backward.
//!
//! Entry layout (variable size, padded to 4 bytes):
//!   0x00 u8  e_name_len           (length of name in bytes, no NUL)
//!   0x01 u8  e_name_index         (namespace prefix code; see NAME_PREFIX)
//!   0x02 u16 e_value_offs         (offset within the block where value lives)
//!   0x04 u32 e_value_inum         (if EA_INODE feature: inode holding the value)
//!   0x08 u32 e_value_size         (length of value in bytes)
//!   0x0C u32 e_hash               (hash of name + value)
//!   0x10 ..  e_name (e_name_len bytes, no NUL, padded to 4)
//!
//! Phase 1: read-only, in-inode + external block. Hash verification + EA_INODE
//! large-value support deferred.

use crate::block_io::BlockDevice;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::inode::Inode;

/// Magic number at the start of an xattr region (in-inode or external block).
pub const EXT4_XATTR_MAGIC: u32 = 0xEA02_0000;

/// Standard namespace prefixes (`e_name_index` value → string).
pub const NAME_PREFIXES: &[(u8, &str)] = &[
    (1, "user."),
    (2, "system.posix_acl_access"),
    (3, "system.posix_acl_default"),
    (4, "trusted."),
    (5, "lustre."),
    (6, "security."),
    (7, "system."),
    (8, "system.richacl"),
];

/// Look up the human-readable prefix for a numeric name_index.
pub fn prefix_for_index(idx: u8) -> Option<&'static str> {
    NAME_PREFIXES
        .iter()
        .find(|(i, _)| *i == idx)
        .map(|(_, s)| *s)
}

/// One parsed xattr entry: fully-qualified name + raw value bytes.
///
/// `#[non_exhaustive]` (#120): the fields stay public to read, but a struct
/// literal outside this crate is refused, so the next field the on-disk entry
/// needs is not a break. Build one with [`XattrEntry::new`],
/// [`XattrEntry::with_raw_name`] or [`XattrEntry::in_ea_inode`].
///
/// Equality covers every field, `value_inum` and `value_size` included: an
/// entry whose value lives in an EA inode never equals an inline one, even
/// with the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct XattrEntry {
    /// Fully-qualified name, e.g. "user.com.apple.FinderInfo", for a person:
    /// [`raw_name`](Self::raw_name) shown lossily.
    pub name: String,
    /// The fully-qualified name's exact bytes, namespace prefix included.
    ///
    /// Linux takes any bytes but NUL in a name, so `user.raw-\xff` is a
    /// name a volume can hold; `name` shows it as `user.raw-\u{fffd}`,
    /// which names nothing. A lookup matches these bytes (#541).
    pub raw_name: Vec<u8>,
    /// Raw value bytes (Finder uses binary data, ACLs are binary, etc.).
    ///
    /// **Empty when `value_inum` is non-zero**, because the bytes are not
    /// in the region this was parsed from. See [`read_all`].
    pub value: Vec<u8>,
    /// `e_value_inum`. Zero for an ordinary entry whose value is stored
    /// inline. Non-zero when `INCOMPAT_EA_INODE` is in play and the value
    /// lives in the file body of that inode instead — in which case
    /// `e_value_offs` describes nothing and `value` above is empty.
    ///
    /// Resolving it needs a [`crate::fs::Filesystem`], which the
    /// buffer-level parsers do not have; [`read_all_resolved`] is the
    /// entry point that follows it.
    pub value_inum: u32,
    /// `e_value_size` — how long the entry says its value is.
    ///
    /// KEPT EVEN THOUGH THE INLINE CASE HAS ALREADY USED IT, because
    /// the EA-inode case has not: the value then comes from another
    /// inode's body, and without this there is nothing to compare what
    /// was read against. The kernel makes exactly that comparison and
    /// fails with `EUCLEAN` when the two disagree; this driver
    /// returned the EA inode's whole body and
    /// reported success (#121).
    pub value_size: u32,
}

impl XattrEntry {
    /// An entry whose value is stored inline, as the parsers produce one:
    /// `value_inum` zero and `value_size` the value's length.
    ///
    /// Panics if the value is longer than `u32::MAX` bytes, which no ext4
    /// entry can describe.
    pub fn new(name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        let name: String = name.into();
        Self::with_raw_name(name.into_bytes(), value)
    }

    /// [`new`](Self::new) for a name given as its exact bytes, which need
    /// not be UTF-8: `name` is their lossy form.
    pub fn with_raw_name(raw_name: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        let value = value.into();
        let value_size = u32::try_from(value.len()).expect("an xattr value fits in u32");
        let raw_name: Vec<u8> = raw_name.into();
        Self {
            name: String::from_utf8_lossy(&raw_name).into_owned(),
            raw_name,
            value,
            value_inum: 0,
            value_size,
        }
    }

    /// An entry whose value lives in the body of EA inode `value_inum`, as
    /// the parsers produce one: `value` empty, and `value_size` the length
    /// the entry declares.
    pub fn in_ea_inode(name: impl Into<String>, value_inum: u32, value_size: u32) -> Self {
        let name: String = name.into();
        Self {
            raw_name: name.clone().into_bytes(),
            name,
            value: Vec::new(),
            value_inum,
            value_size,
        }
    }

    /// An entry as a parser reads one: `prefix` and the stored name bytes,
    /// which need not be UTF-8.
    fn parsed(
        prefix: &str,
        name_bytes: &[u8],
        value: Vec<u8>,
        value_inum: u32,
        value_size: u32,
    ) -> Self {
        let mut raw_name = Vec::with_capacity(prefix.len() + name_bytes.len());
        raw_name.extend_from_slice(prefix.as_bytes());
        raw_name.extend_from_slice(name_bytes);
        Self {
            name: String::from_utf8_lossy(&raw_name).into_owned(),
            raw_name,
            value,
            value_inum,
            value_size,
        }
    }
}

/// Read all extended attributes attached to an inode.
///
/// `inode` is the parsed metadata (we need `file_acl` for the external xattr
/// block). `inode_raw` is the on-disk inode bytes (we need bytes past offset
/// 128 + i_extra_isize for the in-inode xattr region).
///
/// Returns entries from in-inode area first, then external xattr block (if any).
/// An inode with no xattrs returns `Ok(vec![])`.
pub fn read_all(
    dev: &dyn BlockDevice,
    inode: &Inode,
    inode_raw: &[u8],
    inode_size: u16,
    block_size: u32,
) -> Result<Vec<XattrEntry>> {
    let mut out = Vec::new();

    // 1. In-inode xattrs: data between end of i_extra_isize area and end of inode.
    if inode_raw.len() >= 128 + 4 {
        let extra_isize = u16::from_le_bytes(inode_raw[128..130].try_into().unwrap()) as usize;
        let xattr_region_start = 128 + extra_isize;
        // `i_extra_isize = 0` means the extra fields are unused and there
        // is no in-inode area: the format documentation places the area
        // after the `i_extra_isize` bytes ("Inode Size"), the kernel and
        // e2fsprogs see none, and neither does this. Bytes at 0x80 that look
        // like one are
        // not an attribute anybody else can see (#380).
        if extra_isize != 0
            && xattr_region_start + 4 <= inode_size as usize
            && xattr_region_start + 4 <= inode_raw.len()
        {
            let region = &inode_raw[xattr_region_start..(inode_size as usize).min(inode_raw.len())];
            let magic = u32::from_le_bytes(region[..4].try_into().unwrap());
            if magic == EXT4_XATTR_MAGIC {
                // Entries follow the 4-byte magic; values are at e_value_offs from
                // the start of the entry table (== start of region + 4).
                parse_entries(&region[4..], region.len() - 4, &mut out)?;
            }
        }
    }

    // 2. External xattr block: i_file_acl (combined hi+lo) → block number.
    if inode.file_acl != 0 {
        let mut buf = vec![0u8; block_size as usize];
        dev.read_at(inode.file_acl * block_size as u64, &mut buf)?;
        let magic = u32::from_le_bytes(buf[..4].try_into().unwrap());
        if magic != EXT4_XATTR_MAGIC {
            return Err(Error::Corrupt("xattr block magic mismatch"));
        }
        // External block layout: 32-byte header, then entries; values offset
        // is from the start of the block (NOT from end-of-header).
        parse_entries_block(&buf, &mut out)?;
    }

    Ok(out)
}

/// Parse entries from the in-inode xattr area.
///
/// `entries_buf` starts immediately AFTER the 4-byte magic header.
/// In the in-inode format, `e_value_offs` is measured from the start of the
/// entries area (i.e. directly indexes into `entries_buf`). Values are packed
/// backward from the end of the entries area while entries grow forward.
fn parse_entries(entries_buf: &[u8], _region_len: usize, out: &mut Vec<XattrEntry>) -> Result<()> {
    let mut pos = 0;
    while pos + 16 <= entries_buf.len() {
        // The list ends at an entry whose leading fields are zero
        // (attributes.html: "the first four fields ... are set to zero to
        // mark the end"); the first 4-byte word, name length, name index
        // and value offset, is what is read. We cannot short-circuit on
        // name_len == 0 alone, because ACL xattrs (name_index 2 / 3 for
        // system.posix_acl_{access,default}) legitimately store name_len=0
        // — their full name is implied by the index.
        let header = u32::from_le_bytes(entries_buf[pos..pos + 4].try_into().unwrap());
        if header == 0 {
            break;
        }
        let name_len = entries_buf[pos] as usize;
        let name_index = entries_buf[pos + 1];
        let value_offs =
            u16::from_le_bytes(entries_buf[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let value_inum = u32::from_le_bytes(entries_buf[pos + 4..pos + 8].try_into().unwrap());
        let value_size =
            u32::from_le_bytes(entries_buf[pos + 8..pos + 12].try_into().unwrap()) as usize;
        // pos+12..pos+16 = e_hash (ignored)

        let entry_size = 16 + name_len;
        let entry_padded = (entry_size + 3) & !3;
        if pos + 16 + name_len > entries_buf.len() {
            return Err(Error::Corrupt("xattr entry name overruns region"));
        }

        // Any bytes but NUL: a name that is not UTF-8 is not corruption,
        // and refusing it here refused every attribute on the inode (#541).
        let name_bytes = &entries_buf[pos + 16..pos + 16 + name_len];
        let prefix = prefix_for_index(name_index).unwrap_or("");

        // AN EA-INODE ENTRY'S VALUE IS NOT HERE. `e_value_offs` describes
        // nothing once `e_value_inum` is set, so slicing at it returns
        // whatever happens to sit at that offset — quite possibly another
        // attribute's value, which is worse than an error because the
        // caller will act on it.
        let value = if value_inum != 0 {
            Vec::new()
        } else if value_size > 0 {
            if value_offs + value_size > entries_buf.len() {
                return Err(Error::Corrupt("xattr value out of range"));
            }
            entries_buf[value_offs..value_offs + value_size].to_vec()
        } else {
            Vec::new()
        };

        out.push(XattrEntry::parsed(
            prefix,
            name_bytes,
            value,
            value_inum,
            value_size as u32,
        ));

        pos += entry_padded;
    }
    Ok(())
}

/// Parse entries from a full external xattr block.
/// Block layout: 32-byte header at offset 0, then entries starting at 32.
/// `e_value_offs` here is from the START of the block, not the entry table.
fn parse_entries_block(block: &[u8], out: &mut Vec<XattrEntry>) -> Result<()> {
    if block.len() < 32 {
        return Err(Error::Corrupt("xattr block too small"));
    }

    let mut pos = 32; // skip the 32-byte header
    while pos + 16 <= block.len() {
        // See parse_entries: terminator = first 4-byte header word all zero,
        // NOT name_len == 0 (which is legal for ACL entries).
        let header = u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap());
        if header == 0 {
            break;
        }
        let name_len = block[pos] as usize;
        let name_index = block[pos + 1];
        let value_offs = u16::from_le_bytes(block[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let value_inum = u32::from_le_bytes(block[pos + 4..pos + 8].try_into().unwrap());
        let value_size = u32::from_le_bytes(block[pos + 8..pos + 12].try_into().unwrap()) as usize;

        let entry_size = 16 + name_len;
        let entry_padded = (entry_size + 3) & !3;
        if pos + 16 + name_len > block.len() {
            return Err(Error::Corrupt("xattr entry name overruns block"));
        }

        // See `parse_entries`: any bytes but NUL (#541).
        let name_bytes = &block[pos + 16..pos + 16 + name_len];
        let prefix = prefix_for_index(name_index).unwrap_or("");

        // See `parse_entries`: with `e_value_inum` set the value is in
        // another inode's body, not at `e_value_offs` in this block.
        let value = if value_inum != 0 {
            Vec::new()
        } else if value_size > 0 {
            if value_offs + value_size > block.len() {
                return Err(Error::Corrupt("xattr block value out of range"));
            }
            block[value_offs..value_offs + value_size].to_vec()
        } else {
            Vec::new()
        };

        out.push(XattrEntry::parsed(
            prefix,
            name_bytes,
            value,
            value_inum,
            value_size as u32,
        ));

        pos += entry_padded;
    }
    Ok(())
}

/// Split a fully-qualified xattr name (e.g. `"user.com.apple.FinderInfo"`)
/// into (name_index, suffix). Returns `None` if no known prefix matches.
pub fn split_qualified_name(name: &str) -> Option<(u8, &str)> {
    for (idx, prefix) in NAME_PREFIXES {
        if let Some(rest) = name.strip_prefix(*prefix) {
            return Some((*idx, rest));
        }
    }
    None
}

/// Result of [`plan_remove_in_inode_region`]: the entry was either removed
/// (bytes in place updated) or wasn't present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// Region rewritten; caller must patch the inode checksum + write back.
    Removed,
    /// The name wasn't in this region.
    NotFound,
}

/// Remove an xattr by fully-qualified name from the in-inode region.
/// The `region` slice must span from the 4-byte magic (inclusive) to the
/// end of the inode (exclusive of any later metadata). Returns `Removed`
/// if the name was present and the bytes have been rewritten, `NotFound`
/// otherwise. `Error::InvalidArgument` if the name lacks a known
/// namespace prefix.
pub fn plan_remove_in_inode_region(region: &mut [u8], name: &str) -> Result<RemoveOutcome> {
    let Some((name_index, suffix)) = split_qualified_name(name) else {
        return Err(Error::InvalidArgument(
            "xattr name missing known namespace prefix",
        ));
    };
    if region.len() < 4 {
        return Ok(RemoveOutcome::NotFound);
    }
    let magic = u32::from_le_bytes(region[..4].try_into().unwrap());
    if magic != EXT4_XATTR_MAGIC {
        return Ok(RemoveOutcome::NotFound);
    }

    // Decode every entry (header + name + value bytes).
    let entries = decode_in_inode_entries(&region[4..])?;
    refuse_if_any_ea_inode_backed(&entries)?;
    let before = entries.len();
    let kept: Vec<DecodedEntry> = entries
        .into_iter()
        .filter(|e| !(e.name_index == name_index && e.name_bytes == suffix.as_bytes()))
        .collect();
    if kept.len() == before {
        return Ok(RemoveOutcome::NotFound);
    }

    encode_in_inode_entries(region, &kept);
    Ok(RemoveOutcome::Removed)
}

/// Result of [`plan_set_in_inode_region`]: the entry was either
/// inserted (no previous entry with this name) or replaced (new value
/// overwrote an existing entry's value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOutcome {
    Inserted,
    Replaced,
}

/// Set (create-or-replace) an xattr entry in the in-inode region.
/// `region` spans from the 4-byte magic to the end of the inode image.
/// On success the bytes have been rewritten to include the new entry.
///
/// Errors:
/// - `Error::InvalidArgument` when `name` lacks a known namespace prefix
///   or the suffix is empty (except for ACL namespaces 2 + 3).
/// - `Error::NameTooLong` when the suffix is longer than 255 bytes.
/// - `Error::NoSpaceLeftOnDevice` when the rewritten region wouldn't fit
///   (entries + values + 4-byte terminator > region capacity).
pub fn plan_set_in_inode_region(region: &mut [u8], name: &str, value: &[u8]) -> Result<SetOutcome> {
    let Some((name_index, suffix)) = split_qualified_name(name) else {
        return Err(Error::InvalidArgument(
            "xattr name missing known namespace prefix",
        ));
    };
    if suffix.is_empty() && !matches!(name_index, 2 | 3) {
        return Err(Error::InvalidArgument("xattr name suffix is empty"));
    }
    if suffix.len() > 255 {
        return Err(Error::NameTooLong);
    }
    if region.len() < 8 {
        return Err(Error::NoSpaceLeftOnDevice);
    }

    let magic_present = {
        let m = u32::from_le_bytes(region[..4].try_into().unwrap());
        m == EXT4_XATTR_MAGIC
    };
    let mut entries = if magic_present {
        decode_in_inode_entries(&region[4..])?
    } else {
        Vec::new()
    };
    refuse_if_any_ea_inode_backed(&entries)?;

    let mut outcome = SetOutcome::Inserted;
    let suffix_bytes = suffix.as_bytes();
    for e in entries.iter_mut() {
        if e.name_index == name_index && e.name_bytes == suffix_bytes {
            e.value = value.to_vec();
            outcome = SetOutcome::Replaced;
            break;
        }
    }
    if matches!(outcome, SetOutcome::Inserted) {
        entries.push(DecodedEntry {
            name_index,
            name_bytes: suffix_bytes.to_vec(),
            value: value.to_vec(),
            value_inum: 0,
        });
    }

    let area_len = region.len() - 4;
    let needed_entries: usize = entries
        .iter()
        .map(|e| (16 + e.name_bytes.len() + 3) & !3)
        .sum();
    let needed_values: usize = entries
        .iter()
        .filter(|e| !e.value.is_empty())
        .map(|e| (e.value.len() + 3) & !3)
        .sum();
    if needed_entries + 4 + needed_values > area_len {
        return Err(Error::NoSpaceLeftOnDevice);
    }

    encode_in_inode_entries(region, &entries);
    Ok(outcome)
}

/// One fully-owned xattr entry decoded from an in-inode region.
#[derive(Debug, Clone)]
struct DecodedEntry {
    name_index: u8,
    name_bytes: Vec<u8>,
    value: Vec<u8>,
    /// `e_value_inum`, carried so a rewrite can tell that it must not
    /// happen. [`encode_in_inode_entries`] repacks every value into the
    /// region, and a value that lives in another inode cannot be repacked;
    /// emitting the entry without its pointer would leave it describing a
    /// value that was never there and orphan the inode holding the real
    /// one. See [`refuse_if_any_ea_inode_backed`].
    value_inum: u32,
}

/// Refuse to rewrite a region that holds an EA-inode-backed entry.
///
/// # THIS IS NOT ONLY ABOUT THE ENTRY BEING EDITED
///
/// The in-inode region is rewritten wholesale: every surviving entry is
/// re-encoded and every value repacked. So removing or setting attribute
/// *A* re-emits attribute *B* too, and if B's value lives in an EA inode
/// there is nothing to repack — B comes back with `e_value_inum` zeroed
/// and an `e_value_offs` pointing at bytes that are not its value, while
/// the inode holding the real value is orphaned with its refcount
/// untouched and its blocks unfreed.
///
/// Following and refcounting EA inodes on the write path is the feature
/// that would make this work. Until then, refusing is the honest answer,
/// and it is the one this crate already gives for the other shapes it can
/// read but not maintain.
fn refuse_if_any_ea_inode_backed(entries: &[DecodedEntry]) -> Result<()> {
    if entries.iter().any(|e| e.value_inum != 0) {
        return Err(Error::Unsupported(
            "this inode has an extended attribute whose value lives in an EA inode; \
             rewriting the attribute area would orphan it",
        ));
    }
    Ok(())
}

/// Parse every entry out of the in-inode region's entries-area slice
/// (starts immediately AFTER the 4-byte magic).
fn decode_in_inode_entries(entries_buf: &[u8]) -> Result<Vec<DecodedEntry>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 16 <= entries_buf.len() {
        let header = u32::from_le_bytes(entries_buf[pos..pos + 4].try_into().unwrap());
        if header == 0 {
            break;
        }
        let name_len = entries_buf[pos] as usize;
        let name_index = entries_buf[pos + 1];
        let value_offs =
            u16::from_le_bytes(entries_buf[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let value_inum = u32::from_le_bytes(entries_buf[pos + 4..pos + 8].try_into().unwrap());
        let value_size =
            u32::from_le_bytes(entries_buf[pos + 8..pos + 12].try_into().unwrap()) as usize;
        if pos + 16 + name_len > entries_buf.len() {
            return Err(Error::Corrupt("xattr entry name overruns region"));
        }
        let name_bytes = entries_buf[pos + 16..pos + 16 + name_len].to_vec();
        let value = if value_inum != 0 || value_size == 0 {
            Vec::new()
        } else {
            if value_offs + value_size > entries_buf.len() {
                return Err(Error::Corrupt("xattr value out of range"));
            }
            entries_buf[value_offs..value_offs + value_size].to_vec()
        };
        out.push(DecodedEntry {
            name_index,
            name_bytes,
            value,
            value_inum,
        });
        pos += (16 + name_len + 3) & !3;
    }
    Ok(out)
}

/// Re-emit the in-inode region from a list of entries. Zeros the entire
/// region, stamps magic at [0..4], packs entries forward from offset 4,
/// and packs their values backward from the end. Leaves a u32 zero
/// terminator after the last entry (implicit via the initial zero sweep).
///
/// Caller must size `region` large enough; this function is only called
/// after `decode_in_inode_entries` produced the list so the byte budget
/// is always ≤ the original region.
fn encode_in_inode_entries(region: &mut [u8], entries: &[DecodedEntry]) {
    for b in region.iter_mut() {
        *b = 0;
    }
    region[..4].copy_from_slice(&EXT4_XATTR_MAGIC.to_le_bytes());
    let entries_area = &mut region[4..];
    let area_len = entries_area.len();

    // Stable sort: kernel stores entries ordered by (name_index, name).
    let mut sorted: Vec<&DecodedEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.name_index
            .cmp(&b.name_index)
            .then_with(|| a.name_bytes.cmp(&b.name_bytes))
    });

    let mut entry_cursor: usize = 0;
    let mut value_cursor: usize = area_len;

    for e in &sorted {
        let name_len = e.name_bytes.len();
        let entry_padded = (16 + name_len + 3) & !3;

        let value_offs = if e.value.is_empty() {
            0
        } else {
            let value_padded = (e.value.len() + 3) & !3;
            value_cursor -= value_padded;
            entries_area[value_cursor..value_cursor + e.value.len()].copy_from_slice(&e.value);
            value_cursor
        };

        entries_area[entry_cursor] = name_len as u8;
        entries_area[entry_cursor + 1] = e.name_index;
        entries_area[entry_cursor + 2..entry_cursor + 4]
            .copy_from_slice(&(value_offs as u16).to_le_bytes());
        // e_value_inum at +4..+8 = 0 (no EA_INODE)
        entries_area[entry_cursor + 8..entry_cursor + 12]
            .copy_from_slice(&(e.value.len() as u32).to_le_bytes());
        // e_hash at +12..+16 = 0 (in-inode hash is dedup-only)
        entries_area[entry_cursor + 16..entry_cursor + 16 + name_len]
            .copy_from_slice(&e.name_bytes);
        entry_cursor += entry_padded;
    }
    // Terminator u32 at entry_cursor is already zero from the sweep.
}

// ---------------------------------------------------------------------------
// External xattr block: write-side
// ---------------------------------------------------------------------------

/// Outcome of [`plan_remove_from_external_block`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRemoveOutcome {
    /// Entry removed; the block still has at least one entry remaining.
    Removed,
    /// Entry removed AND the block is now empty — caller should free the
    /// block and zero `i_file_acl`.
    RemovedNowEmpty,
    /// The named entry wasn't in this block.
    NotFound,
}

/// Decode every entry from an external xattr block (the 32-byte header is
/// expected at offset 0). Skips the terminator. Used by both the read path
/// and the write path.
fn decode_external_block_entries(block: &[u8]) -> Result<Vec<DecodedEntry>> {
    if block.len() < 32 {
        return Err(Error::Corrupt("xattr block too small"));
    }
    let mut out = Vec::new();
    let mut pos = 32usize;
    while pos + 16 <= block.len() {
        let header = u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap());
        if header == 0 {
            break;
        }
        let name_len = block[pos] as usize;
        let name_index = block[pos + 1];
        let value_offs = u16::from_le_bytes(block[pos + 2..pos + 4].try_into().unwrap()) as usize;
        let value_inum = u32::from_le_bytes(block[pos + 4..pos + 8].try_into().unwrap());
        let value_size = u32::from_le_bytes(block[pos + 8..pos + 12].try_into().unwrap()) as usize;
        if pos + 16 + name_len > block.len() {
            return Err(Error::Corrupt("xattr entry name overruns block"));
        }
        let name_bytes = block[pos + 16..pos + 16 + name_len].to_vec();
        let value = if value_inum != 0 || value_size == 0 {
            Vec::new()
        } else {
            if value_offs + value_size > block.len() {
                return Err(Error::Corrupt("xattr block value out of range"));
            }
            block[value_offs..value_offs + value_size].to_vec()
        };
        out.push(DecodedEntry {
            name_index,
            name_bytes,
            value,
            value_inum,
        });
        pos += (16 + name_len + 3) & !3;
    }
    Ok(out)
}

/// Re-emit a full external xattr block from a list of entries.
///
/// Lays out:
/// - `[0x00..0x04]` magic = `EXT4_XATTR_MAGIC`
/// - `[0x04..0x08]` `h_refcount` (caller-provided; default 1)
/// - `[0x08..0x0C]` `h_blocks` = 1 (always single-block)
/// - `[0x0C..0x10]` `h_hash`, the [`block_hash`] of the entries' hashes
/// - `[0x10..0x14]` `h_checksum` slot — left as 0 here; caller patches via
///   `Checksummer::patch_xattr_block` after layout.
/// - `[0x14..0x20]` reserved zeros
/// - `[0x20..]`     entries growing forward, values growing backward from
///   end of block. `e_value_offs` is BLOCK-relative
///   (different from in-inode where it's region-relative). Every entry's
///   `e_hash` is its [`entry_hash`].
fn encode_external_block(block: &mut [u8], entries: &[DecodedEntry], refcount: u32) {
    block.fill(0);
    block[0x00..0x04].copy_from_slice(&EXT4_XATTR_MAGIC.to_le_bytes());
    block[0x04..0x08].copy_from_slice(&refcount.to_le_bytes());
    block[0x08..0x0C].copy_from_slice(&1u32.to_le_bytes());

    // THE KERNEL'S ORDER, WHICH IS NOT ALPHABETICAL: namespace, then name
    // LENGTH, then name bytes -- the order its own blocks are written in.
    // Its lookup in a block stops at the first entry at or past the
    // target in that order, so a block sorted by name alone hides every
    // attribute that follows a longer name sorting earlier: with
    // `user.abc` first, `getxattr("user.zz")` answered ENODATA (#379).
    // `e2fsck` does not check the order.
    let mut sorted: Vec<&DecodedEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.name_index
            .cmp(&b.name_index)
            .then_with(|| a.name_bytes.len().cmp(&b.name_bytes.len()))
            .then_with(|| a.name_bytes.cmp(&b.name_bytes))
    });

    let mut entry_cursor: usize = 0x20;
    let mut value_cursor: usize = block.len();
    let mut hashes = Vec::with_capacity(sorted.len());

    for e in &sorted {
        let name_len = e.name_bytes.len();
        let entry_padded = (16 + name_len + 3) & !3;

        let value_offs = if e.value.is_empty() {
            0
        } else {
            let value_padded = (e.value.len() + 3) & !3;
            value_cursor -= value_padded;
            block[value_cursor..value_cursor + e.value.len()].copy_from_slice(&e.value);
            value_cursor
        };

        // e2fsck checks every block entry's e_hash against its name and
        // value ("has a hash (N) which is invalid"), so it is always real.
        let e_hash = entry_hash(&e.name_bytes, &e.value);
        hashes.push(e_hash);

        block[entry_cursor] = name_len as u8;
        block[entry_cursor + 1] = e.name_index;
        block[entry_cursor + 2..entry_cursor + 4]
            .copy_from_slice(&(value_offs as u16).to_le_bytes());
        // e_value_inum at +4..+8 = 0 (no EA_INODE)
        block[entry_cursor + 8..entry_cursor + 12]
            .copy_from_slice(&(e.value.len() as u32).to_le_bytes());
        block[entry_cursor + 12..entry_cursor + 16].copy_from_slice(&e_hash.to_le_bytes());
        block[entry_cursor + 16..entry_cursor + 16 + name_len].copy_from_slice(&e.name_bytes);
        entry_cursor += entry_padded;
    }
    block[0x0C..0x10].copy_from_slice(&block_hash(&hashes).to_le_bytes());
    // Terminator already zero from the wipe.
}

// ---------------------------------------------------------------------------
// Entry and block hashes
// ---------------------------------------------------------------------------
//
// kernel.org's attributes.html says only that `e_hash` is a hash of the
// name and value and `h_hash` a hash of all the attributes. The exact
// rules below were read off the harness VM: blocks the kernel wrote with
// `setfattr`, and blocks `debugfs ea_set` wrote, dumped byte for byte,
// with `e2fsck -fn` (which checks `e_hash` and not `h_hash`) as the judge.
//
// - An entry's hash starts at zero. Each byte of the name (without its
//   namespace prefix) is folded in by rotating the running value left 5
//   bits and XOR-ing the byte in, taken as an unsigned 0..=255. Then the
//   value, as little-endian 32-bit words with the last one zero-padded,
//   each folded in with a 16-bit left rotation. An empty value adds
//   nothing.
// - A name byte above 0x7F (`user.qé`) has two readings. debugfs writes
//   the unsigned one, and so does the kernel of the harness's aarch64
//   guest; the kernel of its x86_64 guest, the same Debian 12 image built
//   for amd64, writes the hash that sign-extending the byte gives. e2fsck
//   accepts either. This crate writes the unsigned reading on every
//   platform, as debugfs does.
// - The block's hash folds the entries' hashes, in on-disk order, the
//   same way with a 16-bit rotation -- unless any entry's hash is zero
//   (a name and value can be chosen to make it so), in which case the
//   kernel writes a block hash of zero, wherever that entry falls.

/// Left rotation applied before each name byte is folded in.
const NAME_BYTE_ROTATION: u32 = 5;
/// Left rotation applied before each value word, or entry hash, is folded in.
const WORD_ROTATION: u32 = 16;

/// `e_hash` of an attribute with this name (suffix after the namespace
/// prefix) and value.
fn entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let after_name = name.iter().fold(0u32, |acc, &byte| {
        acc.rotate_left(NAME_BYTE_ROTATION) ^ u32::from(byte)
    });
    value.chunks(4).fold(after_name, |acc, piece| {
        let mut word = [0u8; 4];
        word[..piece.len()].copy_from_slice(piece);
        acc.rotate_left(WORD_ROTATION) ^ u32::from_le_bytes(word)
    })
}

/// `h_hash` of a block whose entries, in on-disk order, have these hashes.
/// Zero when any of them is zero.
fn block_hash(entry_hashes: &[u32]) -> u32 {
    if entry_hashes.contains(&0) {
        return 0;
    }
    entry_hashes
        .iter()
        .fold(0u32, |acc, &hash| acc.rotate_left(WORD_ROTATION) ^ hash)
}

/// Set (create-or-replace) an xattr in an external block buffer.
///
/// `block` is the full xattr block (caller has already read it from disk
/// or freshly zeroed it for a brand-new allocation). `refcount` is what to
/// stamp into `h_refcount` — pass 1 for a non-shared block. On success the
/// bytes have been rewritten; the caller must (a) patch `h_checksum` via
/// `Checksummer::patch_xattr_block` and (b) write the block back.
///
/// Errors:
/// - `Error::InvalidArgument` if the name lacks a known prefix or has an
///   empty suffix (except ACL namespaces 2 + 3).
/// - `Error::NameTooLong` if the suffix > 255 bytes.
/// - `Error::NoSpaceLeftOnDevice` if the new layout would not fit in the
///   block (entries + values + terminator > block size).
pub fn plan_set_in_external_block(
    block: &mut [u8],
    name: &str,
    value: &[u8],
    refcount: u32,
) -> Result<SetOutcome> {
    let Some((name_index, suffix)) = split_qualified_name(name) else {
        return Err(Error::InvalidArgument(
            "xattr name missing known namespace prefix",
        ));
    };
    if suffix.is_empty() && !matches!(name_index, 2 | 3) {
        return Err(Error::InvalidArgument("xattr name suffix is empty"));
    }
    if suffix.len() > 255 {
        return Err(Error::NameTooLong);
    }
    if block.len() < 0x40 {
        return Err(Error::NoSpaceLeftOnDevice);
    }

    let magic_present = u32::from_le_bytes(block[..4].try_into().unwrap()) == EXT4_XATTR_MAGIC;
    let mut entries = if magic_present {
        decode_external_block_entries(block)?
    } else {
        Vec::new()
    };
    refuse_if_any_ea_inode_backed(&entries)?;

    let mut outcome = SetOutcome::Inserted;
    let suffix_bytes = suffix.as_bytes();
    for e in entries.iter_mut() {
        if e.name_index == name_index && e.name_bytes == suffix_bytes {
            e.value = value.to_vec();
            outcome = SetOutcome::Replaced;
            break;
        }
    }
    if matches!(outcome, SetOutcome::Inserted) {
        entries.push(DecodedEntry {
            name_index,
            name_bytes: suffix_bytes.to_vec(),
            value: value.to_vec(),
            value_inum: 0,
        });
    }

    let entries_capacity = block.len() - 0x20;
    let needed_entries: usize = entries
        .iter()
        .map(|e| (16 + e.name_bytes.len() + 3) & !3)
        .sum();
    let needed_values: usize = entries
        .iter()
        .filter(|e| !e.value.is_empty())
        .map(|e| (e.value.len() + 3) & !3)
        .sum();
    if needed_entries + 4 + needed_values > entries_capacity {
        return Err(Error::NoSpaceLeftOnDevice);
    }

    encode_external_block(block, &entries, refcount);
    Ok(outcome)
}

/// Remove an xattr from an external block buffer.
///
/// Returns [`BlockRemoveOutcome::RemovedNowEmpty`] when the last entry is
/// gone — the caller should free the underlying block + zero `i_file_acl`
/// rather than leaving an empty xattr block on disk. Otherwise rewrites
/// the block in place; caller must re-checksum + write back.
pub fn plan_remove_from_external_block(
    block: &mut [u8],
    name: &str,
    refcount: u32,
) -> Result<BlockRemoveOutcome> {
    let Some((name_index, suffix)) = split_qualified_name(name) else {
        return Err(Error::InvalidArgument(
            "xattr name missing known namespace prefix",
        ));
    };
    if block.len() < 0x20 {
        return Ok(BlockRemoveOutcome::NotFound);
    }
    let magic = u32::from_le_bytes(block[..4].try_into().unwrap());
    if magic != EXT4_XATTR_MAGIC {
        return Ok(BlockRemoveOutcome::NotFound);
    }

    let entries = decode_external_block_entries(block)?;
    refuse_if_any_ea_inode_backed(&entries)?;
    let before = entries.len();
    let kept: Vec<DecodedEntry> = entries
        .into_iter()
        .filter(|e| !(e.name_index == name_index && e.name_bytes == suffix.as_bytes()))
        .collect();
    if kept.len() == before {
        return Ok(BlockRemoveOutcome::NotFound);
    }
    if kept.is_empty() {
        return Ok(BlockRemoveOutcome::RemovedNowEmpty);
    }
    encode_external_block(block, &kept, refcount);
    Ok(BlockRemoveOutcome::Removed)
}

/// Refuse an external xattr block that is not one: no magic, an `h_blocks`
/// other than 1, or (on `metadata_csum`) a checksum that does not verify.
///
/// The kernel makes the same checks before it reads or edits the block,
/// and answers EUCLEAN ("Structure needs cleaning"). Every block ext4
/// has ever written is exactly one block long; a larger `h_blocks` is not
/// a layout anybody can edit in place.
pub(crate) fn check_external_block(
    csum: &crate::checksum::Checksummer,
    block_nr: u64,
    block: &[u8],
) -> Result<()> {
    if block.len() < 0x20 || u32::from_le_bytes(block[..4].try_into().unwrap()) != EXT4_XATTR_MAGIC
    {
        return Err(Error::Corrupt(
            "i_file_acl names a block without the xattr magic",
        ));
    }
    if u32::from_le_bytes(block[8..12].try_into().unwrap()) != 1 {
        return Err(Error::Corrupt("xattr block h_blocks is not 1"));
    }
    if !csum.verify_xattr_block(block_nr, block) {
        return Err(Error::BadChecksum {
            what: "xattr block",
        });
    }
    Ok(())
}

/// Check `inode`'s external xattr block, if it has one, before a read
/// returns what is in it. See [`check_external_block`]: without it a block
/// whose checksum fails still answers, with bytes that may be another
/// attribute's or none at all.
fn check_block_of(fs: &Filesystem, inode: &Inode) -> Result<()> {
    if inode.file_acl == 0 {
        return Ok(());
    }
    let bs = fs.sb.block_size();
    let mut block = vec![0u8; bs as usize];
    fs.dev.read_at(inode.file_acl * bs as u64, &mut block)?;
    check_external_block(&fs.csum, inode.file_acl, &block)
}

/// Convenience: get a single xattr value by name. Returns `None` if not present.
///
/// The name is matched as bytes against [`XattrEntry::raw_name`]; one that
/// is not UTF-8 is found with [`get_resolved_raw`].
pub fn get(
    dev: &dyn BlockDevice,
    inode: &Inode,
    inode_raw: &[u8],
    inode_size: u16,
    block_size: u32,
    name: &str,
) -> Result<Option<Vec<u8>>> {
    let all = read_all(dev, inode, inode_raw, inode_size, block_size)?;
    Ok(all
        .into_iter()
        .find(|e| e.raw_name == name.as_bytes())
        .map(|e| e.value))
}

/// Every xattr on `inode`, with EA-inode values followed.
///
/// # WHY THIS EXISTS ALONGSIDE [`read_all`]
///
/// `INCOMPAT_EA_INODE` means an attribute whose value is too large for the
/// xattr area does not store its value there at all: `e_value_inum` names
/// an inode whose file body **is** the value, and `e_value_offs` stops
/// describing anything. Following that pointer needs to read another
/// inode, so it needs a [`Filesystem`] — which the buffer-level parsers
/// deliberately do not take, since they are also used on a region held in
/// memory during a rewrite.
///
/// So [`read_all`] reports the pointer and leaves the value empty, and
/// this resolves it. A caller that uses [`read_all`] directly and reads
/// `value` without checking `value_inum` gets an empty value rather than
/// the wrong one — the failure is visible instead of plausible, which is
/// the whole point.
pub fn read_all_resolved(
    fs: &Filesystem,
    inode: &Inode,
    inode_raw: &[u8],
) -> Result<Vec<XattrEntry>> {
    check_block_of(fs, inode)?;
    let mut entries = read_all(
        fs.dev.as_ref(),
        inode,
        inode_raw,
        fs.sb.inode_size,
        fs.sb.block_size(),
    )?;
    for e in entries.iter_mut() {
        if e.value_inum != 0 {
            e.value = crate::ea_inode::read_value_inode(fs, e.value_inum, e.value_size)?;
        }
    }
    Ok(entries)
}

/// The fully-qualified names of an inode's attributes, and nothing else.
///
/// What `fs_ext4_listxattr` needs (#122). It used [`read_all_resolved`],
/// which reads every EA-inode-backed VALUE from disk only for the caller to
/// discard it -- and fails the whole listing when one of those values
/// cannot be read, though every name was right there. A names-only listing
/// does not depend on any value being readable.
///
/// A name that is not UTF-8 is listed lossily, which names nothing on the
/// volume; [`list_raw_names`] gives each name's exact bytes.
pub fn list_names(fs: &Filesystem, inode: &Inode, inode_raw: &[u8]) -> Result<Vec<String>> {
    Ok(names_of(fs, inode, inode_raw)?
        .into_iter()
        .map(|e| e.name)
        .collect())
}

/// [`list_names`] with each name's exact bytes, which need not be UTF-8
/// (#541): what `fs_ext4_listxattr` hands back.
pub fn list_raw_names(fs: &Filesystem, inode: &Inode, inode_raw: &[u8]) -> Result<Vec<Vec<u8>>> {
    Ok(names_of(fs, inode, inode_raw)?
        .into_iter()
        .map(|e| e.raw_name)
        .collect())
}

/// Every entry of `inode`, EA-inode values left unread.
fn names_of(fs: &Filesystem, inode: &Inode, inode_raw: &[u8]) -> Result<Vec<XattrEntry>> {
    check_block_of(fs, inode)?;
    read_all(
        fs.dev.as_ref(),
        inode,
        inode_raw,
        fs.sb.inode_size,
        fs.sb.block_size(),
    )
}

/// One xattr by fully-qualified name, with an EA-inode value followed.
/// See [`read_all_resolved`].
pub fn get_resolved(
    fs: &Filesystem,
    inode: &Inode,
    inode_raw: &[u8],
    name: &str,
) -> Result<Option<Vec<u8>>> {
    get_resolved_raw(fs, inode, inode_raw, name.as_bytes())
}

/// [`get_resolved`] for a name given as its exact bytes, which need not be
/// UTF-8 (#541): matched against [`XattrEntry::raw_name`].
pub fn get_resolved_raw(
    fs: &Filesystem,
    inode: &Inode,
    inode_raw: &[u8],
    name: &[u8],
) -> Result<Option<Vec<u8>>> {
    check_block_of(fs, inode)?;
    let Some(entry) = read_all(
        fs.dev.as_ref(),
        inode,
        inode_raw,
        fs.sb.inode_size,
        fs.sb.block_size(),
    )?
    .into_iter()
    .find(|e| e.raw_name == name) else {
        return Ok(None);
    };
    if entry.value_inum != 0 {
        return Ok(Some(crate::ea_inode::read_value_inode(
            fs,
            entry.value_inum,
            entry.value_size,
        )?));
    }
    Ok(Some(entry.value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_known() {
        assert_eq!(prefix_for_index(1), Some("user."));
        assert_eq!(prefix_for_index(7), Some("system."));
        assert_eq!(prefix_for_index(99), None);
    }

    #[test]
    fn split_qualified_name_roundtrip() {
        assert_eq!(split_qualified_name("user.color"), Some((1, "color")));
        assert_eq!(
            split_qualified_name("user.com.apple.FinderInfo"),
            Some((1, "com.apple.FinderInfo"))
        );
        assert_eq!(
            split_qualified_name("security.selinux"),
            Some((6, "selinux"))
        );
        assert_eq!(split_qualified_name("unknown.foo"), None);
    }

    /// `XattrEntry` is `#[non_exhaustive]`, so its constructors are the only
    /// way a caller builds one. `new` must produce exactly what the parser
    /// does for an inline value, or a caller comparing against a parsed
    /// entry is told they differ when the volume agrees with them (#120).
    #[test]
    fn new_builds_what_the_parser_reads_for_an_inline_value() {
        let mut region = vec![0u8; 64];
        encode_in_inode_entries(
            &mut region,
            &[DecodedEntry {
                name_index: 1,
                name_bytes: b"color".to_vec(),
                value: b"red".to_vec(),
                value_inum: 0,
            }],
        );
        let mut parsed = Vec::new();
        parse_entries(&region[4..], region.len() - 4, &mut parsed).unwrap();
        assert_eq!(parsed, vec![XattrEntry::new("user.color", b"red".to_vec())]);
    }

    /// An EA-inode entry carries no bytes of its own and the size its entry
    /// declares; it never equals an inline entry of the same name, since
    /// equality covers every field.
    #[test]
    fn in_ea_inode_carries_the_inode_and_declared_size_and_no_bytes() {
        let e = XattrEntry::in_ea_inode("user.big", 42, 70_000);
        assert_eq!(e.name, "user.big");
        assert!(e.value.is_empty());
        assert_eq!((e.value_inum, e.value_size), (42, 70_000));
        assert_ne!(e, XattrEntry::new("user.big", Vec::new()));
    }

    /// Build a minimal in-inode region with two `user.*` entries, then remove
    /// one by name. Verify the other survives and a readback decodes cleanly.
    #[test]
    fn remove_in_inode_roundtrips_one_of_two() {
        // 96-byte region is plenty for two short entries (each ~24 bytes
        // header+name + a handful of value bytes).
        let mut region = vec![0u8; 96];
        let entries = vec![
            DecodedEntry {
                name_index: 1,
                name_bytes: b"color".to_vec(),
                value: b"red".to_vec(),
                value_inum: 0,
            },
            DecodedEntry {
                name_index: 1,
                name_bytes: b"mood".to_vec(),
                value: b"happy".to_vec(),
                value_inum: 0,
            },
        ];
        encode_in_inode_entries(&mut region, &entries);

        // Sanity: before-remove decode returns both entries.
        let decoded = decode_in_inode_entries(&region[4..]).unwrap();
        assert_eq!(decoded.len(), 2);

        let outcome = plan_remove_in_inode_region(&mut region, "user.color").unwrap();
        assert_eq!(outcome, RemoveOutcome::Removed);

        let after = decode_in_inode_entries(&region[4..]).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].name_bytes, b"mood");
        assert_eq!(after[0].value, b"happy");
    }

    #[test]
    fn remove_in_inode_returns_not_found_for_missing() {
        let mut region = vec![0u8; 64];
        let entries = vec![DecodedEntry {
            name_index: 1,
            name_bytes: b"color".to_vec(),
            value: b"red".to_vec(),
            value_inum: 0,
        }];
        encode_in_inode_entries(&mut region, &entries);
        let outcome = plan_remove_in_inode_region(&mut region, "user.mood").unwrap();
        assert_eq!(outcome, RemoveOutcome::NotFound);
    }

    #[test]
    fn remove_in_inode_unknown_prefix_is_einval() {
        let mut region = vec![0u8; 64];
        let err = plan_remove_in_inode_region(&mut region, "nope.name").unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    #[test]
    fn remove_in_inode_missing_magic_is_not_found() {
        let mut region = vec![0u8; 64];
        // all zeros → no magic
        let outcome = plan_remove_in_inode_region(&mut region, "user.x").unwrap();
        assert_eq!(outcome, RemoveOutcome::NotFound);
    }

    #[test]
    fn set_in_inode_inserts_new_into_empty_region() {
        let mut region = vec![0u8; 64];
        let outcome = plan_set_in_inode_region(&mut region, "user.color", b"red").unwrap();
        assert_eq!(outcome, SetOutcome::Inserted);
        let decoded = decode_in_inode_entries(&region[4..]).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].name_bytes, b"color");
        assert_eq!(decoded[0].value, b"red");
    }

    #[test]
    fn set_in_inode_replaces_existing_value() {
        let mut region = vec![0u8; 96];
        plan_set_in_inode_region(&mut region, "user.color", b"red").unwrap();
        let outcome = plan_set_in_inode_region(&mut region, "user.color", b"emerald").unwrap();
        assert_eq!(outcome, SetOutcome::Replaced);
        let decoded = decode_in_inode_entries(&region[4..]).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].value, b"emerald");
    }

    #[test]
    fn set_in_inode_preserves_other_entries() {
        let mut region = vec![0u8; 128];
        plan_set_in_inode_region(&mut region, "user.color", b"red").unwrap();
        plan_set_in_inode_region(&mut region, "user.mood", b"happy").unwrap();
        plan_set_in_inode_region(&mut region, "user.color", b"blue").unwrap();
        let decoded = decode_in_inode_entries(&region[4..]).unwrap();
        let by_name: std::collections::BTreeMap<_, _> = decoded
            .into_iter()
            .map(|e| (e.name_bytes.clone(), e.value))
            .collect();
        assert_eq!(by_name.get(b"color".as_slice()).unwrap(), b"blue");
        assert_eq!(by_name.get(b"mood".as_slice()).unwrap(), b"happy");
    }

    #[test]
    fn set_in_inode_enospc_on_overflow() {
        let mut region = vec![0u8; 32];
        let err =
            plan_set_in_inode_region(&mut region, "user.x", b"this_is_20_bytes_xx!").unwrap_err();
        assert!(matches!(err, Error::NoSpaceLeftOnDevice));
    }

    #[test]
    fn set_in_inode_unknown_prefix_is_einval() {
        let mut region = vec![0u8; 64];
        let err = plan_set_in_inode_region(&mut region, "weird.key", b"v").unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    /// `raw-\xff`: Linux takes any bytes but NUL in an attribute name, so
    /// a volume can hold `user.raw-\xff`, which is not UTF-8.
    const RAW: &[u8] = b"raw-\xff";

    fn entry(name_index: u8, name_bytes: &[u8], value: &[u8]) -> DecodedEntry {
        DecodedEntry {
            name_index,
            name_bytes: name_bytes.to_vec(),
            value: value.to_vec(),
            value_inum: 0,
        }
    }

    fn names_and_values(parsed: &[XattrEntry]) -> Vec<(&str, &[u8])> {
        parsed
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_slice()))
            .collect()
    }

    /// A name that is not UTF-8 is not corruption: the in-inode parser
    /// reads it, shown lossily, and every attribute beside it (#541).
    #[test]
    fn a_name_that_is_not_utf8_is_read_with_the_others_in_the_inode() {
        let mut region = vec![0u8; 96];
        encode_in_inode_entries(
            &mut region,
            &[entry(1, RAW, b"raw"), entry(1, b"ok", b"fine")],
        );
        let mut parsed = Vec::new();
        parse_entries(&region[4..], region.len() - 4, &mut parsed)
            .expect("a name that is not UTF-8 is not corruption");
        assert_eq!(
            names_and_values(&parsed),
            [
                ("user.ok", b"fine".as_slice()),
                ("user.raw-\u{fffd}", b"raw".as_slice()),
            ]
        );
    }

    /// The same in an external block, whose parser is a separate one.
    #[test]
    fn a_name_that_is_not_utf8_is_read_with_the_others_in_a_block() {
        let mut block = vec![0u8; 4096];
        encode_external_block(
            &mut block,
            &[entry(1, RAW, b"raw"), entry(4, b"ok", b"fine")],
            1,
        );
        let mut parsed = Vec::new();
        parse_entries_block(&block, &mut parsed)
            .expect("a name that is not UTF-8 is not corruption");
        assert_eq!(
            names_and_values(&parsed),
            [
                ("user.raw-\u{fffd}", b"raw".as_slice()),
                ("trusted.ok", b"fine".as_slice()),
            ]
        );
    }

    /// A device nothing is read from: these inodes have no external block.
    struct NoDev;

    impl BlockDevice for NoDev {
        fn read_at(&self, _: u64, _: &mut [u8]) -> Result<()> {
            unreachable!("no external attribute block")
        }
        fn write_at(&self, _: u64, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        fn size_bytes(&self) -> u64 {
            0
        }
    }

    /// An inline file's bytes past the first 60 are its `system.data`
    /// attribute, found among the inode's attributes by name. A name
    /// beside it that is not UTF-8 leaves the file readable, and a lookup
    /// by its lossy spelling finds nothing: names are matched as bytes
    /// (#541).
    #[test]
    fn an_inline_file_beside_a_name_that_is_not_utf8_reads_back() {
        let content: Vec<u8> = (0..80u8).collect();
        let mut raw = vec![0u8; 256];
        raw[0x00..0x02].copy_from_slice(&0o100_644u16.to_le_bytes()); // i_mode
        raw[0x04..0x08].copy_from_slice(&80u32.to_le_bytes()); // i_size_lo
        raw[0x28..0x28 + 60].copy_from_slice(&content[..60]); // i_block
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes()); // i_extra_isize
        encode_in_inode_entries(
            &mut raw[160..],
            &[entry(7, b"data", &content[60..]), entry(1, RAW, b"raw")],
        );
        let inode = Inode::parse(&raw).unwrap();
        assert_eq!(
            crate::inline_data::read_all(&NoDev, &inode, &raw, 256, 4096)
                .expect("the inline file reads"),
            content
        );
        assert_eq!(
            get(&NoDev, &inode, &raw, 256, 4096, "user.raw-\u{fffd}").unwrap(),
            None,
            "the lossy spelling is not the name"
        );
    }

    /// What the parsers read keeps each name's exact bytes, prefix
    /// included, and equals what the constructors build: `with_raw_name`
    /// for a name that is not UTF-8, `new` for one that is, an ACL's
    /// implied name among them (#541).
    #[test]
    fn a_parsed_name_keeps_its_exact_bytes() {
        let entries = [
            entry(1, RAW, b"raw"),
            entry(1, b"ok", b"fine"),
            entry(2, b"", b"acl"),
        ];
        let mut region = vec![0u8; 128];
        encode_in_inode_entries(&mut region, &entries);
        let mut in_inode = Vec::new();
        parse_entries(&region[4..], region.len() - 4, &mut in_inode).unwrap();
        let raw = XattrEntry::with_raw_name(b"user.raw-\xff".to_vec(), b"raw".to_vec());
        let ok = XattrEntry::new("user.ok", b"fine".to_vec());
        let acl = XattrEntry::new("system.posix_acl_access", b"acl".to_vec());
        assert_eq!(in_inode, [ok.clone(), raw.clone(), acl.clone()]);

        let mut block = vec![0u8; 4096];
        encode_external_block(&mut block, &entries, 1);
        let mut in_block = Vec::new();
        parse_entries_block(&block, &mut in_block).unwrap();
        assert_eq!(in_block, [ok, raw.clone(), acl]);

        assert_eq!(raw.raw_name, b"user.raw-\xff");
        assert_eq!(raw.name, "user.raw-\u{fffd}");
        assert_eq!(
            XattrEntry::new("user.ok", Vec::new()).raw_name,
            b"user.ok",
            "new keeps a UTF-8 name's bytes"
        );
        assert_eq!(
            XattrEntry::in_ea_inode("user.big", 42, 70_000).raw_name,
            b"user.big"
        );
    }

    /// `get` matches the exact bytes of a UTF-8 name beside one that is
    /// not, in the inode and in the external block alike.
    #[test]
    fn get_finds_a_name_beside_one_that_is_not_utf8() {
        let mut raw = vec![0u8; 256];
        raw[0x80..0x82].copy_from_slice(&32u16.to_le_bytes()); // i_extra_isize
        encode_in_inode_entries(
            &mut raw[160..],
            &[entry(1, RAW, b"raw"), entry(1, b"ok", b"fine")],
        );
        let inode = Inode::parse(&raw).unwrap();
        assert_eq!(
            get(&NoDev, &inode, &raw, 256, 4096, "user.ok").unwrap(),
            Some(b"fine".to_vec())
        );
        assert_eq!(
            get(&NoDev, &inode, &raw, 256, 4096, "user.raw-").unwrap(),
            None
        );
    }

    /// attributes.html: external-block entries are sorted by
    /// `(name_index, name_len, name)`. The kernel's lookup stops at the
    /// first entry that sorts at or after the target, so the block must be
    /// written in that order. Sorted by name alone,
    /// `user.abc` goes before `user.zz`, and the kernel's lookup of `zz`
    /// stops at `abc` and answers ENODATA (#379).
    #[test]
    fn external_block_entries_are_sorted_by_name_length_before_name() {
        let mut block = vec![0u8; 4096];
        plan_set_in_external_block(&mut block, "user.abc", b"1", 1).unwrap();
        plan_set_in_external_block(&mut block, "user.zz", b"2", 1).unwrap();
        assert_eq!(
            block[0x20],
            2,
            "the first entry must be the shorter name (zz), not {:?}",
            String::from_utf8_lossy(&block[0x30..0x30 + block[0x20] as usize])
        );
        // And the namespace still comes first: a longer name in a lower
        // index sorts before a shorter one in a higher index.
        plan_set_in_external_block(&mut block, "trusted.a", b"3", 1).unwrap();
        let order: Vec<(u8, Vec<u8>)> = decode_external_block_entries(&block)
            .unwrap()
            .into_iter()
            .map(|e| (e.name_index, e.name_bytes))
            .collect();
        assert_eq!(
            order,
            vec![
                (1, b"zz".to_vec()),
                (1, b"abc".to_vec()),
                (4, b"a".to_vec()),
            ]
        );
    }
}

/// The entry and block hashes, against words the kernel and debugfs
/// wrote in the harness VM (see the comment above [`entry_hash`]).
#[cfg(test)]
mod hash_tests {
    use super::{block_hash, entry_hash, NAME_BYTE_ROTATION, WORD_ROTATION};

    /// `(name suffix, value, e_hash)` as found in kernel- and
    /// debugfs-written blocks.
    const OBSERVED: &[(&[u8], &[u8], u32)] = &[
        (b"a", b"x", 0x0061_0078),
        (b"bb", b"hello", 0x6568_6021),
        (b"empty", b"", 0x0667_4EF9),
        (b"z", b"0123456789", 0x067C_3F3E),
        (b"q", b"1", 0x0071_0031),
        (b"a", b"hi", 0x0061_6968),
        ("q\u{e9}".as_bytes(), b"val", 0xDCA5_6177),
        // Values chosen so the hash comes out zero.
        (b"a", &[0x00, 0x00, 0x61, 0x00], 0),
        (b"zzz", &[0x01, 0x00, 0x3A, 0xE7], 0),
    ];

    #[test]
    fn entry_hashes_match_the_observed_words() {
        for &(name, value, want) in OBSERVED {
            assert_eq!(
                entry_hash(name, value),
                want,
                "{:?}={value:?}",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// The kernel's block for `user.a`, `user.bb`, `user.empty` and
    /// `trusted.z`, in its on-disk order.
    #[test]
    fn the_block_hash_folds_the_entry_hashes_in_order() {
        let hashes = [0x0061_0078, 0x6568_6021, 0x0667_4EF9, 0x067C_3F3E];
        assert_eq!(block_hash(&hashes), 0x2D95_5919);
        assert_eq!(
            block_hash(&hashes[..1]),
            hashes[0],
            "one entry is its own hash"
        );
        assert_eq!(block_hash(&[]), 0);
    }

    #[test]
    fn a_zero_entry_hash_anywhere_zeroes_the_block_hash() {
        assert_eq!(block_hash(&[0, 0x6568_6021]), 0);
        assert_eq!(block_hash(&[0x0061_6968, 0]), 0);
    }

    /// [`entry_hash`] with each name byte above 0x7F sign-extended
    /// instead, the reading the x86_64 guest's kernel writes.
    fn sign_extending_entry_hash(name: &[u8], value: &[u8]) -> u32 {
        let after_name = name.iter().fold(0u32, |acc, &byte| {
            acc.rotate_left(NAME_BYTE_ROTATION) ^ (i32::from(byte as i8) as u32)
        });
        value.chunks(4).fold(after_name, |acc, piece| {
            let mut word = [0u8; 4];
            word[..piece.len()].copy_from_slice(piece);
            acc.rotate_left(WORD_ROTATION) ^ u32::from_le_bytes(word)
        })
    }

    /// The two readings differ only for a name byte above 0x7F, and the
    /// sign-extending one is the word the x86_64 guest's kernel stored for
    /// `user.qé` = `val`.
    #[test]
    fn a_high_name_byte_has_two_readings() {
        for &(name, value, _) in OBSERVED {
            if name.iter().all(|&byte| byte <= 0x7F) {
                assert_eq!(
                    sign_extending_entry_hash(name, value),
                    entry_hash(name, value)
                );
            }
        }
        let name = "q\u{e9}".as_bytes();
        assert_eq!(entry_hash(name, b"val"), 0xDCA5_6177);
        assert_eq!(sign_extending_entry_hash(name, b"val"), 0xC3BA_6177);
    }

    /// Tests that ask the kernel and e2fsprogs in the harness VM.
    mod needs_host {
        use super::super::{
            block_hash, decode_external_block_entries, encode_external_block, entry_hash,
        };
        use super::sign_extending_entry_hash;
        use crate::block_io::FileDevice;
        use crate::fs::Filesystem;
        use std::sync::Arc;

        /// Two attribute sets: one ordinary (with an empty value, a value
        /// that is not a whole number of words, and a name byte above 0x7F),
        /// and one whose last attribute hashes to zero.
        /// A file name and the `(attribute, value)` pairs set on it.
        type AttributeSet = (&'static str, &'static [(&'static str, &'static [u8])]);

        const FILES: &[AttributeSet] = &[
            (
                "plain",
                &[
                    ("user.a", b"x"),
                    ("user.bb", b"hello"),
                    ("user.empty", b""),
                    ("trusted.z", b"0123456789"),
                    ("user.q\u{e9}", b"val"),
                ],
            ),
            (
                "zero",
                &[("user.a", b"hi"), ("user.zzz", &[0x01, 0x00, 0x3A, 0xE7])],
            ),
        ];

        fn volume(tag: &str) -> String {
            let image = fs_ext4_test_support::temp_path!(
                "fs_ext4_xattr_hash_{tag}_{}.img",
                std::process::id()
            );
            std::fs::File::create(&image)
                .and_then(|f| f.set_len(8 << 20))
                .unwrap();
            // 128-byte inodes leave no in-inode room: every attribute goes
            // to the external block.
            let out = fs_ext4_test_support::oracle("mkfs.ext4")
                .args(["-q", "-F", "-b", "1024", "-I", "128", &image])
                .output();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            image
        }

        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        }

        /// `(e_hash of each entry in on-disk order, h_hash)` of `path`'s
        /// external block.
        fn block_hashes(fs: &Filesystem, path: &str) -> (Vec<u32>, u32, Vec<u8>) {
            let ino = fs.lookup_path_bytes(path.as_bytes()).unwrap();
            let inode = fs.read_inode_verified(ino).unwrap().0;
            assert_ne!(inode.file_acl, 0, "{path} has no external block");
            let block = fs.read_block(inode.file_acl).unwrap();
            let mut hashes = Vec::new();
            let mut at = 0x20;
            while u32::from_le_bytes(block[at..at + 4].try_into().unwrap()) != 0 {
                hashes.push(u32::from_le_bytes(
                    block[at + 12..at + 16].try_into().unwrap(),
                ));
                at += (16 + block[at] as usize + 3) & !3;
            }
            let h_hash = u32::from_le_bytes(block[0x0C..0x10].try_into().unwrap());
            (hashes, h_hash, block)
        }

        /// The kernel writes each attribute set. Every hash it stored is the
        /// one computed here, except that for a name with a byte above 0x7F
        /// it may be the sign-extending reading instead (the x86_64 guest's
        /// kernel writes that one), and its block hash folds the hashes it
        /// stored. Re-encoding its entries reproduces its block hash
        /// whenever its entry hashes are this crate's.
        #[test]
        fn hashes_in_kernel_written_blocks_are_the_ones_computed_here() {
            let image = volume("kernel");
            let mut script = String::from("set -e\n");
            for (file, attrs) in FILES {
                script.push_str(&format!("touch \"$MNT/{file}\"\n"));
                for (name, value) in *attrs {
                    script.push_str(&format!(
                        "setfattr -n '{name}' -v 0x{} \"$MNT/{file}\"\n",
                        hex(value)
                    ));
                }
            }
            let script = script.replace("-v 0x \"", "\"");
            let out = fs_ext4_test_support::guest_kernel_write(&image, &script);
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );

            let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
            for (file, attrs) in FILES {
                let path = format!("/{file}");
                let (hashes, h_hash, block) = block_hashes(&fs, &path);
                assert_eq!(hashes.len(), attrs.len(), "{path}");
                let entries = decode_external_block_entries(&block).unwrap();
                assert_eq!(entries.len(), hashes.len(), "{path}");
                let computed: Vec<u32> = entries
                    .iter()
                    .map(|entry| entry_hash(&entry.name_bytes, &entry.value))
                    .collect();
                for ((entry, &stored), &ours) in entries.iter().zip(&hashes).zip(&computed) {
                    let name = String::from_utf8_lossy(&entry.name_bytes);
                    if stored == ours {
                        continue;
                    }
                    assert!(
                        entry.name_bytes.iter().any(|&byte| byte > 0x7F),
                        "{path} {name}: the kernel stored {stored:#010x}, and this crate \
                         computes {ours:#010x}"
                    );
                    assert_eq!(
                        sign_extending_entry_hash(&entry.name_bytes, &entry.value),
                        stored,
                        "{path} {name}: the kernel's hash is neither reading"
                    );
                }
                assert_eq!(block_hash(&hashes), h_hash, "{path}'s h_hash");

                let mut ours = vec![0u8; block.len()];
                encode_external_block(&mut ours, &entries, 1);
                assert_eq!(
                    ours[0x0C..0x10],
                    block_hash(&computed).to_le_bytes(),
                    "{path}: h_hash re-encoded"
                );
                if computed == hashes {
                    assert_eq!(
                        ours[0x0C..0x10],
                        block[0x0C..0x10],
                        "{path}: h_hash re-encoded"
                    );
                }
            }
            drop(fs);
            fs_ext4_test_support::assert_e2fsck_clean(&image, "the kernel's blocks");
            let _ = std::fs::remove_file(&image);
        }

        /// This crate writes the same attribute sets: e2fsck accepts every
        /// e_hash, and the hashes are the unsigned readings, the ones debugfs
        /// and the aarch64 guest's kernel store for the same attributes.
        #[test]
        fn blocks_this_crate_writes_carry_the_kernels_hashes() {
            let image = volume("ours");
            {
                let fs = Filesystem::mount(Arc::new(FileDevice::open_rw(&image).unwrap())).unwrap();
                for (file, attrs) in FILES {
                    let path = format!("/{file}");
                    fs.apply_create(&path, 0o644).unwrap();
                    for (name, value) in *attrs {
                        fs.apply_setxattr(&path, name, value).unwrap();
                    }
                }
            }
            fs_ext4_test_support::assert_e2fsck_clean(&image, "this crate's blocks");

            // The unsigned readings for these sets, as observed.
            let expected: &[(&str, &[u32], u32)] = &[
                (
                    "/plain",
                    &[
                        0x0061_0078,
                        0x6568_6021,
                        0xDCA5_6177,
                        0x0667_4EF9,
                        0x067C_3F3E,
                    ],
                    block_hash(&[
                        0x0061_0078,
                        0x6568_6021,
                        0xDCA5_6177,
                        0x0667_4EF9,
                        0x067C_3F3E,
                    ]),
                ),
                ("/zero", &[0x0061_6968, 0], 0),
            ];
            let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
            for &(path, hashes, h_hash) in expected {
                let (got, got_h, _) = block_hashes(&fs, path);
                assert_eq!(got, hashes, "{path}");
                assert_eq!(got_h, h_hash, "{path}");
            }
            drop(fs);
            let _ = std::fs::remove_file(&image);
        }
    }
}
