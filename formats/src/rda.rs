// RDA archives: walking the block chain and reading entry payloads of v2.2
// ("Resource File V2.2", Anno 1800 and 117) and v2.0 (Anno 2070, encrypted),
// and packing new archives in either version for mods and nested `.a7m`
// islands.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::DeflateEncoder;

// ------------------------------------------------------------------ layout
//
// Both versions: a header ending in the first block's offset, then blocks
// chained back to front of each other. A block's member bytes (or a memres
// block's one section) come first, then its header table of file headers,
// a memres block's two section sizes, and last its block header, which
// points at the next block header; the last points at the end of the file.
// Paths are 520 bytes of UTF-16LE. What differs is in each `Layout`.

/// A block header's fixed start in both versions: flags and file count as
/// u32. Three words follow: the header table's stored and raw size and the
/// next block header's offset.
const BLOCK_FLAGS_AT: usize = 0;
const BLOCK_FILES_AT: usize = 4;
const BLOCK_COUNTS: usize = 8;
const BLOCK_WORDS: usize = 3;
/// A file header in both versions: the path, then five words: data offset,
/// stored size, raw size, timestamp and a word every shipped archive leaves 0.
const PATH_BYTES: usize = 520;
const FILE_WORDS: usize = 5;
/// A memres block's section sizes, stored then raw, before its block header.
const MEMRES_WORDS: usize = 2;
// 520 bytes of UTF-16 including the NUL terminator.
const MAX_PATH_UNITS: usize = 259;

// Reader policy.
// The engine's chain walker rejects blocks above this.
const MAX_FILES_PER_BLOCK: u32 = 10_000_000;
// Deflate cannot expand more than ~1032:1; caps the up-front allocation when
// a header claims a huge raw size.
const MAX_ZLIB_RATIO: u64 = 1032;

/// The archive versions: v2.0 is Anno 2070's, v2.2 Anno 1800's and 117's.
/// A reader takes either; [`PackOptions::version`] picks what a packer
/// writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V20,
    V22,
}

impl Version {
    const ALL: [Version; 2] = [Version::V20, Version::V22];

    /// The version `head` (the file's first bytes) starts with.
    fn of(head: &[u8]) -> Option<Version> {
        Version::ALL
            .into_iter()
            .find(|v| head.starts_with(v.layout().magic))
    }

    pub(crate) fn layout(self) -> &'static Layout {
        match self {
            Version::V20 => &Layout::V20,
            Version::V22 => &Layout::V22,
        }
    }
}

/// Where one version's bytes differ from the other's.
#[derive(Debug)]
pub(crate) struct Layout {
    /// `Resource File V2.x` as stored: UTF-16LE in v2.0, ASCII in v2.2.
    magic: &'static [u8],
    /// The archive header; the first block's bytes start here.
    header: u64,
    /// Where the header keeps the first block header's offset, its last word.
    first_block_at: usize,
    /// Every size and offset: u32 in v2.0, u64 in v2.2.
    word: usize,
    block_header: u64,
    file_header: usize,
    memres_info: u64,
    /// v2.0 blocks may be XOR-encrypted (see [`crypt`]); 2070's own zlib
    /// blocks all are. v2.2's scheme is not known here, and 1800 and 117
    /// ship no encrypted block.
    encrypts: bool,
}

impl Layout {
    /// v2.0's magic: its text in UTF-16LE.
    const V20_TEXT: &[u8] = b"Resource File V2.0";
    const V20_MAGIC: [u8; 2 * Layout::V20_TEXT.len()] = {
        let text = Layout::V20_TEXT;
        let mut wide = [0; 2 * Layout::V20_TEXT.len()];
        let mut i = 0;
        while i < text.len() {
            wide[2 * i] = text[i];
            i += 1;
        }
        wide
    };
    const V20: Layout = Layout::with(&Layout::V20_MAGIC, 1048, size_of::<u32>(), true);
    const V22: Layout = Layout::with(b"Resource File V2.2", 792, size_of::<u64>(), false);

    /// The longer header of the two: enough of a file to tell its version
    /// and find its first block.
    const LONGEST_HEADER: usize = if Layout::V20.header > Layout::V22.header {
        Layout::V20.header as usize
    } else {
        Layout::V22.header as usize
    };

    // The record sizes follow from the word width; the header's size and
    // magic are each version's own.
    const fn with(magic: &'static [u8], header: u64, word: usize, encrypts: bool) -> Layout {
        Layout {
            magic,
            header,
            first_block_at: header as usize - word,
            word,
            block_header: (BLOCK_COUNTS + BLOCK_WORDS * word) as u64,
            file_header: PATH_BYTES + FILE_WORDS * word,
            memres_info: (MEMRES_WORDS * word) as u64,
            encrypts,
        }
    }

    fn word(&self, b: &[u8]) -> u64 {
        match self.word == size_of::<u32>() {
            true => u32_at(b, 0) as u64,
            false => u64::from_le_bytes(b[..size_of::<u64>()].try_into().unwrap()),
        }
    }

    /// Writes `v` as the first word of `b`; v2.0 has no room above 4 GiB.
    fn put_word(&self, b: &mut [u8], v: u64) -> Result<()> {
        if self.word == size_of::<u32>() {
            let v = u32::try_from(v).with_context(|| format!("{v} does not fit RDA v2.0"))?;
            b[..self.word].copy_from_slice(&v.to_le_bytes());
        } else {
            b[..self.word].copy_from_slice(&v.to_le_bytes());
        }
        Ok(())
    }

    fn push_word(&self, out: &mut Vec<u8>, v: u64) -> Result<()> {
        let at = out.len();
        out.resize(at + self.word, 0);
        self.put_word(&mut out[at..], v)
    }

    /// The first block header's offset, from the archive header.
    fn first_block(&self, head: &[u8]) -> u64 {
        self.word(&head[self.first_block_at..])
    }

    /// An archive header pointing at `first_block`.
    fn encode_header(&self, first_block: u64) -> Result<Vec<u8>> {
        let mut head = vec![0; self.header as usize];
        head[..self.magic.len()].copy_from_slice(self.magic);
        self.put_word(&mut head[self.first_block_at..], first_block)?;
        Ok(head)
    }

    fn block(&self, h: &[u8]) -> BlockHeader {
        let [table_stored, table_raw, next] =
            std::array::from_fn(|k| self.word(&h[BLOCK_COUNTS + k * self.word..]));
        BlockHeader {
            flags: BlockFlags(u32_at(h, BLOCK_FLAGS_AT)),
            files: u32_at(h, BLOCK_FILES_AT),
            table_stored,
            table_raw,
            next,
        }
    }

    /// Writes `h` over the block header at the start of `b`.
    fn put_block(&self, b: &mut [u8], h: &BlockHeader) -> Result<()> {
        b[BLOCK_FLAGS_AT..BLOCK_FILES_AT].copy_from_slice(&h.flags.0.to_le_bytes());
        b[BLOCK_FILES_AT..BLOCK_COUNTS].copy_from_slice(&h.files.to_le_bytes());
        for (k, v) in [h.table_stored, h.table_raw, h.next]
            .into_iter()
            .enumerate()
        {
            self.put_word(&mut b[BLOCK_COUNTS + k * self.word..], v)?;
        }
        Ok(())
    }

    fn file<'a>(&self, rec: &'a [u8]) -> Record<'a> {
        let [offset, compressed_size, raw_size, timestamp, unknown] =
            std::array::from_fn(|k| self.word(&rec[PATH_BYTES + k * self.word..]));
        Record {
            path: &rec[..PATH_BYTES],
            offset,
            compressed_size,
            raw_size,
            timestamp,
            unknown,
        }
    }

    /// Appends a file header: `path` NUL-padded, then the five words.
    fn push_file(&self, table: &mut Vec<u8>, path: &str, words: [u64; FILE_WORDS]) -> Result<()> {
        let start = table.len();
        for u in path.encode_utf16() {
            table.extend_from_slice(&u.to_le_bytes());
        }
        ensure!(
            table.len() - start < PATH_BYTES,
            "{path}: path too long for RDA"
        );
        table.resize(start + PATH_BYTES, 0);
        for v in words {
            self.push_word(table, v)?;
        }
        Ok(())
    }

    /// A memres block's (stored, raw) section sizes from the bytes before
    /// its block header.
    fn memres(&self, info: &[u8]) -> (u64, u64) {
        (self.word(info), self.word(&info[self.word..]))
    }
}

/// A block header in either version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockHeader {
    flags: BlockFlags,
    files: u32,
    table_stored: u64,
    table_raw: u64,
    next: u64,
}

/// v2.0's block encryption, its own inverse: each little-endian 16-bit word
/// XORed with the next key of an MSVC `rand()` sequence seeded with 666666
/// (RDAExplorer's reading, checked against Anno 2070's archives). A trailing
/// odd byte is stored as is.
fn crypt(data: &mut [u8]) {
    let mut x: u32 = 666_666;
    for w in data.as_chunks_mut::<2>().0 {
        x = x.wrapping_mul(214_013).wrapping_add(2_531_011);
        let key = ((x >> 16) & 0x7fff) as u16;
        *w = (u16::from_le_bytes(*w) ^ key).to_le_bytes();
    }
}

/// Block flags as stored in the block header. Every entry carries the flags
/// of the block it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockFlags(pub(crate) u32);

impl BlockFlags {
    const ZLIB: u32 = 1;
    const ENCRYPTED: u32 = 2;
    const MEMRES: u32 = 4;
    pub(crate) const DELETED: u32 = 8;

    pub fn compressed(self) -> bool {
        self.0 & Self::ZLIB != 0
    }
    pub fn encrypted(self) -> bool {
        self.0 & Self::ENCRYPTED != 0
    }
    pub fn memres(self) -> bool {
        self.0 & Self::MEMRES != 0
    }
    /// Tombstone: the path is deleted from this archive and from every
    /// archive mounted before it.
    pub(crate) fn deleted(self) -> bool {
        self.0 & Self::DELETED != 0
    }
}

/// One file header from the chain, after last-wins resolution.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path exactly as stored (UTF-16 decoded). Use [`Entry::key`] to match.
    pub(crate) path: Box<str>,
    /// Absolute archive offset, or relative to the decoded section for memres.
    pub(crate) offset: u64,
    pub(crate) compressed_size: u64,
    pub(crate) raw_size: u64,
    pub(crate) flags: BlockFlags,
    // Index into Archive::memres, u32::MAX when the block is not memres.
    pub(crate) memres: u32,
}

impl Entry {
    /// Engine lookup key: `/` separators, ASCII lowercase, no `.` segments.
    pub fn key(&self) -> Cow<'_, str> {
        normalize_path(&self.path)
    }
}

/// Engine-equivalent path key (rdsdk_VFS_rda_entry_find): `\` becomes `/`,
/// empty and `.` segments vanish, ASCII letters fold to lowercase. Borrows
/// when the path is already in that form, which is true for every shipped
/// entry.
pub fn normalize_path(path: &str) -> Cow<'_, str> {
    let clean = !path.bytes().any(|b| b == b'\\' || b.is_ascii_uppercase())
        && !path.starts_with('/')
        && !path.ends_with('/')
        && !path.contains("//")
        && !path.split('/').any(|s| s == ".");
    if clean {
        return Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len());
    for seg in path.split(['/', '\\']) {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(seg);
    }
    out.make_ascii_lowercase();
    Cow::Owned(out)
}

/// Appends the [`normalize_path`] key of a stored path (520 bytes of
/// UTF-16LE up to a NUL) to `out` and returns where its last '/' is, if it
/// has one. Plain ASCII paths, which is every shipped one, go straight from
/// the record; the rest through `normalize_path`.
pub(crate) fn push_key(raw: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    let start = out.len();
    if let Some(cut) = push_plain(raw, out) {
        return cut;
    }
    out.truncate(start);
    out.extend_from_slice(normalize_path(&decode_path(raw)).as_bytes());
    out[start..].iter().rposition(|&b| b == b'/')
}

// `push_key` for keys that are ASCII and have no empty or `.` segments,
// which normalizing leaves as they are but for case and `\`. None for the
// rest, and for a few clean ones (".x", "a/.x") the slow path handles too.
// 16 units a step; 1800's and 117's paths average 70.
#[cfg(target_arch = "x86_64")]
fn push_plain(raw: &[u8], out: &mut Vec<u8>) -> Option<Option<usize>> {
    use std::arch::x86_64::*;
    assert!(raw.len() >= 512);
    out.reserve(256 + 16);
    let start = out.len();
    // (n, last '/' + 1, slash or dot pairs, whether the previous step
    // ended in '/')
    let (mut n, mut cut, mut odd, mut prev_slash) = (0usize, 0usize, 0u32, 0u32);
    // SAFETY: SSE2 is part of x86_64. Loads stay inside the 512 bytes
    // asserted; stores of 16 bytes at `n` < 256 stay inside the 272
    // reserved, and only the `n` bytes written are kept.
    unsafe {
        let dst = out.as_mut_ptr().add(start);
        let zero = _mm_setzero_si128();
        for step in 0..16 {
            let p = raw.as_ptr().add(step * 32) as *const __m128i;
            let (a, b) = (_mm_loadu_si128(p), _mm_loadu_si128(p.add(1)));
            let nul = _mm_movemask_epi8(_mm_packs_epi16(
                _mm_cmpeq_epi16(a, zero),
                _mm_cmpeq_epi16(b, zero),
            )) as u32;
            let high = _mm_set1_epi16(0xff80u16 as i16);
            let ascii = _mm_movemask_epi8(_mm_packs_epi16(
                _mm_cmpeq_epi16(_mm_and_si128(a, high), zero),
                _mm_cmpeq_epi16(_mm_and_si128(b, high), zero),
            )) as u32;
            let valid = if nul != 0 {
                (1u32 << nul.trailing_zeros()) - 1
            } else {
                0xffff
            };
            if !ascii & valid != 0 {
                return None;
            }
            let mut bytes = _mm_packus_epi16(a, b);
            let back = _mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'\\' as i8));
            bytes = _mm_xor_si128(
                bytes,
                _mm_and_si128(back, _mm_set1_epi8((b'\\' ^ b'/') as i8)),
            );
            // 'A'..='Z' moved to the bottom of the signed range.
            let shifted = _mm_add_epi8(bytes, _mm_set1_epi8((0x80u8 - b'A') as i8));
            let upper = _mm_cmplt_epi8(shifted, _mm_set1_epi8((0x80u8 + 26) as i8));
            bytes = _mm_or_si128(bytes, _mm_and_si128(upper, _mm_set1_epi8(0x20)));
            _mm_storeu_si128(dst.add(n) as *mut __m128i, bytes);
            let slash =
                _mm_movemask_epi8(_mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'/' as i8))) as u32 & valid;
            let dot =
                _mm_movemask_epi8(_mm_cmpeq_epi8(bytes, _mm_set1_epi8(b'.' as i8))) as u32 & valid;
            // A '/' followed by '/' or '.', also across steps; a key that
            // starts with either counts as following one.
            let next = slash | dot;
            odd |= slash & next >> 1 | next & if step == 0 { 1 } else { prev_slash };
            if slash != 0 {
                cut = n + 32 - slash.leading_zeros() as usize;
            }
            let k = valid.count_ones() as usize;
            prev_slash = slash >> 15 & 1;
            n += k;
            if nul != 0 {
                if n == 0 || cut == n || odd != 0 {
                    return None;
                }
                out.set_len(start + n);
                return Some(cut.checked_sub(1));
            }
        }
    }
    // 256 units without a NUL: the slow way.
    None
}

#[cfg(not(target_arch = "x86_64"))]
use push_plain_scalar as push_plain;

#[cfg(any(test, not(target_arch = "x86_64")))]
fn push_plain_scalar(raw: &[u8], out: &mut Vec<u8>) -> Option<Option<usize>> {
    let units = raw.as_chunks::<2>().0;
    let n = units
        .iter()
        .position(|u| *u == [0, 0])
        .unwrap_or(units.len());
    let units = &units[..n];
    if units.iter().fold(0u16, |a, u| a | u16::from_le_bytes(*u)) >= 0x80 {
        return None;
    }
    let start = out.len();
    out.extend(units.iter().map(|&[b, _]| {
        let b = if b == b'\\' { b'/' } else { b };
        b + (b.wrapping_sub(b'A') < 26) as u8 * 32
    }));
    let key = &out[start..];
    let odd = key.first().is_none_or(|&b| b == b'/' || b == b'.')
        || key.last() == Some(&b'/')
        || (key.iter().zip(&key[1..])).any(|(&a, &b)| a == b'/' && (b == b'/' || b == b'.'));
    (!odd).then(|| key.iter().rposition(|&b| b == b'/'))
}

/// Payload section of a memory-resident block: one stream holding every
/// member back to back, entry offsets are relative to its decoded bytes.
#[derive(Debug, Clone, Copy)]
struct Memres {
    start: u64,
    compressed_size: u64,
    raw_size: u64,
    zlib: bool,
}

enum Source {
    File(File),
    Memory(Vec<u8>),
}

/// A byte range of an archive, read as a stream.
struct Section<'a> {
    source: &'a Source,
    at: u64,
    end: u64,
}

impl Read for Section<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.end - self.at) as usize;
        if n == 0 {
            return Ok(0);
        }
        match self.source {
            Source::Memory(b) => {
                let at = self.at as usize;
                let Some(bytes) = b.get(at..at + n) else {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                };
                buf[..n].copy_from_slice(bytes);
            }
            Source::File(f) => read_exact_at(f, &mut buf[..n], self.at)?,
        }
        self.at += n as u64;
        Ok(n)
    }
}

impl Source {
    fn read_at(&self, off: u64, len: u64) -> Result<Vec<u8>> {
        match self {
            Source::Memory(b) => {
                let end = off.checked_add(len).filter(|&e| e <= b.len() as u64);
                let Some(end) = end else {
                    bail!("read {len} B at {off:#x} past end ({:#x})", b.len());
                };
                Ok(b[off as usize..end as usize].to_vec())
            }
            Source::File(f) => {
                let mut buf = vec![0; usize::try_from(len)?];
                read_exact_at(f, &mut buf, off)
                    .with_context(|| format!("read {len} B at {off:#x}"))?;
                Ok(buf)
            }
        }
    }
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// An opened archive: the resolved entry list and a handle for payload reads.
/// Opening reads only the archive header, block headers and header tables.
pub struct Archive {
    name: String,
    source: Source,
    size: u64,
    entries: Vec<Entry>,
    tombstones: Vec<Entry>,
    memres: Vec<Memres>,
}

impl std::fmt::Debug for Archive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Archive")
            .field("name", &self.name)
            .field("size", &self.size)
            .field("entries", &self.entries.len())
            .field("tombstones", &self.tombstones.len())
            .finish()
    }
}

impl Archive {
    /// Whether `path` starts like an archive of either version: a cheap
    /// check for discovery, which reads only the header.
    pub fn sniff(path: &Path) -> bool {
        let Ok(mut file) = File::open(path) else {
            return false;
        };
        let Ok(meta) = file.metadata() else {
            return false;
        };
        let mut head = vec![0; (meta.len() as usize).min(Layout::LONGEST_HEADER)];
        if file.read_exact(&mut head).is_err() {
            return false;
        }
        let Some(l) = Version::of(&head).map(Version::layout) else {
            return false;
        };
        if (head.len() as u64) < l.header {
            return false;
        }
        let first = l.first_block(&head);
        first >= l.header
            && (first == meta.len()
                || first
                    .checked_add(l.block_header)
                    .is_some_and(|end| end <= meta.len()))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Archive> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        Archive::load(Source::File(file), path.display().to_string())
    }

    /// Archive held in memory, e.g. a nested `.a7m` island read out of a
    /// province RDA. `name` only labels errors.
    pub fn parse(bytes: Vec<u8>, name: impl Into<String>) -> Result<Archive> {
        Archive::load(Source::Memory(bytes), name.into())
    }

    /// Path or label given at open time.
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// Live entries in chain order. Last-wins per key is already applied and
    /// paths tombstoned later in the chain are gone.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Paths this archive deletes (final state per key is a tombstone). They
    /// also hide the path in every archive mounted earlier.
    #[cfg(test)]
    pub(crate) fn tombstones(&self) -> &[Entry] {
        &self.tombstones
    }

    /// Linear scan; the install VFS keeps its own index for the big archives.
    fn find(&self, path: &str) -> Option<&Entry> {
        let key = normalize_path(path);
        self.entries.iter().find(|e| e.key() == key)
    }

    /// Decoded payload of `entry`, which must come from this archive.
    /// Up to `len` bytes of `entry` from `start`. Stored entries read only
    /// those bytes; compressed ones inflate whole first.
    pub(crate) fn read_range(&self, entry: &Entry, start: u64, len: u64) -> Result<Vec<u8>> {
        let len = len.min(entry.raw_size.saturating_sub(start));
        let flags = entry.flags;
        if flags.memres() || flags.compressed() || flags.encrypted() || flags.deleted() {
            let all = self.read(entry)?;
            return Ok(all[start as usize..(start + len) as usize].to_vec());
        }
        self.source
            .read_at(entry.offset + start, len)
            .with_context(|| format!("{}: {}", self.name, entry.path))
    }

    pub fn read(&self, entry: &Entry) -> Result<Vec<u8>> {
        let flags = entry.flags;
        let ctx = || format!("{}: {}", self.name, entry.path);
        ensure!(!flags.deleted(), "{}: tombstone has no data", ctx());
        // Encrypted v2.2 blocks were refused at open.
        let read = |off: u64, len: u64| -> Result<Vec<u8>> {
            let mut stored = self.source.read_at(off, len).with_context(ctx)?;
            if flags.encrypted() {
                crypt(&mut stored);
            }
            Ok(stored)
        };
        if flags.memres() {
            let Some(m) = self.memres.get(entry.memres as usize) else {
                bail!("{}: memres entry without a section", ctx());
            };
            // Bounds checked at open time.
            let (start, len) = (entry.offset, entry.raw_size);
            if !flags.encrypted() {
                if !m.zlib {
                    return read(m.start + start, len);
                }
                // A memres section packs many small files in one zlib
                // stream. Inflating it whole for each file held a section
                // per loader thread at once (1.1 GB loading an 1800
                // island's models); streamed, a read holds its file and
                // inflates only up to it.
                let section = Section {
                    source: &self.source,
                    at: m.start,
                    end: m.start + m.compressed_size,
                };
                let mut z = ZlibDecoder::new(section);
                std::io::copy(&mut (&mut z).take(start), &mut std::io::sink()).with_context(ctx)?;
                let mut out = Vec::with_capacity(len as usize);
                (&mut z).take(len).read_to_end(&mut out).with_context(ctx)?;
                ensure!(
                    out.len() as u64 == len,
                    "{}: memres section ends {} B into a {len} B file",
                    ctx(),
                    out.len()
                );
                return Ok(out);
            }
            let stored = read(m.start, m.compressed_size)?;
            let section = if m.zlib {
                inflate(&stored, m.raw_size).with_context(ctx)?
            } else {
                stored
            };
            let start = start as usize;
            return Ok(section[start..start + len as usize].to_vec());
        }
        let stored = read(entry.offset, entry.compressed_size)?;
        if flags.compressed() {
            return inflate(&stored, entry.raw_size).with_context(ctx);
        }
        ensure!(
            entry.compressed_size == entry.raw_size,
            "{}: raw block entry with csize {} != rsize {}",
            ctx(),
            entry.compressed_size,
            entry.raw_size
        );
        Ok(stored)
    }

    pub fn read_path(&self, path: &str) -> Result<Vec<u8>> {
        let Some(entry) = self.find(path) else {
            bail!("{}: no entry {path}", self.name);
        };
        self.read(entry)
    }

    fn load(source: Source, name: String) -> Result<Archive> {
        let mapped;
        let bytes: &[u8] = match &source {
            Source::Memory(b) => b,
            Source::File(f) => {
                mapped = map(f).with_context(|| format!("map {name}"))?;
                &mapped
            }
        };
        let size = bytes.len() as u64;
        let mut all = Vec::new();
        let memres = walk(bytes, &name, |t| {
            all.reserve(t.len());
            for i in 0..t.len() {
                let r = t.record(i)?;
                all.push(Entry {
                    path: decode_path(r.path),
                    offset: r.offset,
                    compressed_size: r.compressed_size,
                    raw_size: r.raw_size,
                    flags: t.flags,
                    memres: t.memres,
                });
            }
            Ok(())
        })?;

        // The engine keeps one map slot per key; later blocks overwrite
        // earlier ones, tombstone blocks included.
        let mut keep = vec![false; all.len()];
        {
            let mut last: HashMap<Cow<str>, usize> = HashMap::with_capacity(all.len());
            for (i, e) in all.iter().enumerate() {
                last.insert(e.key(), i);
            }
            for i in last.into_values() {
                keep[i] = true;
            }
        }
        let mut entries = Vec::new();
        let mut tombstones = Vec::new();
        for (e, keep) in all.into_iter().zip(keep) {
            match (keep, e.flags.deleted()) {
                (false, _) => {}
                (true, false) => entries.push(e),
                (true, true) => tombstones.push(e),
            }
        }
        entries.shrink_to_fit();
        Ok(Archive {
            name,
            source,
            size,
            entries,
            tombstones,
            memres,
        })
    }
}

// Archives are mapped to walk their chains, not read: 1800's patch archives
// split their header tables over 600k blocks, and a read per block header
// and per table costs about a microsecond each on Linux's ntfs3 (1.1 s for
// 1800's chains read, 74 ms mapped, one thread). The map lives only while
// the chain is walked. A file cut short meanwhile (a game update during the
// open) faults the process instead of failing the open; that risk is taken.
fn map(file: &File) -> std::io::Result<memmap2::Mmap> {
    // SAFETY: the map is read-only and dropped after the walk; see above
    // for what a concurrent truncation does.
    unsafe { memmap2::Mmap::map(file) }
}

/// An archive mapped to read its header tables in place, for the install
/// index, which parses them into its own arrays.
pub(crate) struct Mapped {
    file: File,
    map: memmap2::Mmap,
    name: String,
}

/// The memres sections a walk found; [`Table::memres`] indexes them.
pub(crate) struct Sections(Vec<Memres>);

impl Mapped {
    pub(crate) fn open(path: &Path) -> Result<Mapped> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let map = map(&file).with_context(|| format!("map {}", path.display()))?;
        Ok(Mapped {
            file,
            map,
            name: path.display().to_string(),
        })
    }

    /// Hands every block's header table to `each`, in chain order.
    pub(crate) fn walk(&self, each: impl FnMut(Table<'_>) -> Result<()>) -> Result<Sections> {
        Ok(Sections(walk(&self.map, &self.name, each)?))
    }

    /// The archive for payload reads, without an entry list; the map goes.
    pub(crate) fn into_archive(self, sections: Sections) -> Archive {
        Archive {
            size: self.map.len() as u64,
            name: self.name,
            source: Source::File(self.file),
            entries: Vec::new(),
            tombstones: Vec::new(),
            memres: sections.0,
        }
    }
}

/// One block's header table from the chain walk.
pub(crate) struct Table<'a> {
    pub flags: BlockFlags,
    /// Index into the archive's memres sections, `u32::MAX` outside one.
    pub memres: u32,
    layout: &'static Layout,
    // nfiles records in stored layout, decrypted and inflated.
    records: Cow<'a, [u8]>,
    // Entry data must end by here: the section's raw size, else the file's.
    limit: u64,
    header_size: u64,
    name: &'a str,
    ptr: u64,
}

/// A header table record (a file header), its data range checked.
pub(crate) struct Record<'a> {
    /// The path as stored: UTF-16LE up to a NUL, 520 bytes.
    pub path: &'a [u8],
    pub offset: u64,
    pub compressed_size: u64,
    pub raw_size: u64,
    /// Unix seconds. Only the tests that rewrite shipped archives byte for
    /// byte read these two.
    #[cfg_attr(not(test), expect(dead_code))]
    pub timestamp: u64,
    #[cfg_attr(not(test), expect(dead_code))]
    pub unknown: u64,
}

impl Table<'_> {
    pub(crate) fn len(&self) -> usize {
        self.records.len() / self.layout.file_header
    }

    pub(crate) fn record(&self, i: usize) -> Result<Record<'_>> {
        let size = self.layout.file_header;
        let rec = &self.records[i * size..][..size];
        let r = self.layout.file(rec);
        let at = || format!("{}: block@{:#x}", self.name, self.ptr);
        ensure!(rec[..2] != [0, 0], "{}: entry {i} has an empty path", at());
        if !self.flags.deleted() {
            let memres = self.memres != u32::MAX;
            let len = if memres {
                r.raw_size
            } else {
                r.compressed_size
            };
            let in_bounds = r
                .offset
                .checked_add(len)
                .is_some_and(|end| end <= self.limit)
                && (memres || len == 0 || r.offset >= self.header_size);
            ensure!(
                in_bounds,
                "{}: {} data {:#x}+{len} outside {:#x}",
                at(),
                decode_path(r.path),
                r.offset,
                self.limit
            );
        }
        Ok(r)
    }
}

// Walks the chain strictly: the engine silently ends the chain on a bogus
// pointer, we fail so a broken mod archive is noticed before shipping.
// Returns the memres sections the tables' `memres` index.
fn walk<'a>(
    bytes: &'a [u8],
    name: &'a str,
    mut each: impl FnMut(Table<'a>) -> Result<()>,
) -> Result<Vec<Memres>> {
    let size = bytes.len() as u64;
    let head = &bytes[..bytes.len().min(Layout::LONGEST_HEADER)];
    let Some(version) = Version::of(head) else {
        bail!(
            "{name}: not an RDA v2.0 or v2.2 archive (magic {:?})",
            String::from_utf8_lossy(&head[..head.len().min(Layout::V22.magic.len())])
        );
    };
    let layout = version.layout();
    let (header_size, block_header, file_header) =
        (layout.header, layout.block_header, layout.file_header);
    ensure!(
        size >= header_size,
        "{name}: {size} B is shorter than the RDA header"
    );
    let first = layout.first_block(head);
    // Bounds are checked before every slice below.
    let at_range = |off: u64, len: u64| &bytes[off as usize..(off + len) as usize];

    let mut memres = Vec::new();
    // Every block costs at least its header, so a longer chain must
    // contain a cycle.
    let max_blocks = (size - header_size) / block_header;
    let mut ptr = first;
    let mut n = 0u64;
    while ptr != size {
        ensure!(
            ptr >= header_size && ptr.checked_add(block_header).is_some_and(|e| e <= size),
            "{name}: block pointer {ptr:#x} outside the file after {n} block(s)"
        );
        ensure!(n < max_blocks, "{name}: block chain loops at {ptr:#x}");
        let BlockHeader {
            flags,
            files: nfiles,
            table_stored: cmpfh,
            table_raw: rawfh,
            next,
        } = layout.block(at_range(ptr, block_header));
        let at = || format!("{name}: block@{ptr:#x}");
        ensure!(
            nfiles <= MAX_FILES_PER_BLOCK,
            "{}: nFiles {nfiles} over engine cap",
            at()
        );
        ensure!(
            cmpfh <= u32::MAX as u64 && rawfh <= u32::MAX as u64,
            "{}: header table sizes {cmpfh}/{rawfh} do not fit u32",
            at()
        );
        ensure!(
            rawfh == nfiles as u64 * file_header as u64,
            "{}: rawFH {rawfh} != nFiles {nfiles} * {file_header}",
            at()
        );
        ensure!(
            next != 0 && next != ptr && next <= size,
            "{}: bogus nextBlockPtr {next:#x}",
            at()
        );
        if nfiles > 0 {
            ensure!(
                !flags.encrypted() || flags.deleted() || layout.encrypts,
                "{}: encrypted v2.2 block (flags {:#x}); Anno 1800 and 117 ship none \
                 and this reader decrypts only v2.0",
                at(),
                flags.0
            );
            let extra = if flags.memres() {
                layout.memres_info
            } else {
                0
            };
            let table_start = ptr.checked_sub(cmpfh + extra).filter(|&s| s >= header_size);
            let Some(table_start) = table_start else {
                bail!(
                    "{}: header table of {cmpfh} B overlaps the archive header",
                    at()
                );
            };
            let stored = at_range(table_start, cmpfh);
            let records = if flags.encrypted() || cmpfh != rawfh {
                let mut stored = stored.to_vec();
                if flags.encrypted() {
                    crypt(&mut stored);
                }
                if cmpfh != rawfh {
                    stored = inflate(&stored, rawfh)
                        .with_context(|| format!("{}: header table", at()))?;
                }
                Cow::Owned(stored)
            } else {
                Cow::Borrowed(stored)
            };
            let (mut section, mut limit) = (u32::MAX, size);
            if flags.memres() && !flags.deleted() {
                let (compressed_size, raw_size) = layout.memres(at_range(ptr - extra, extra));
                let m = Memres {
                    start: 0,
                    compressed_size,
                    raw_size,
                    zlib: flags.compressed(),
                };
                let start = table_start
                    .checked_sub(m.compressed_size)
                    .filter(|&s| s >= header_size);
                let Some(start) = start else {
                    bail!("{}: memres section overlaps the archive header", at());
                };
                (section, limit) = (memres.len() as u32, m.raw_size);
                memres.push(Memres { start, ..m });
            }
            each(Table {
                flags,
                memres: section,
                layout,
                records,
                limit,
                header_size,
                name,
                ptr,
            })?;
        }
        n += 1;
        ptr = next;
    }
    Ok(memres)
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

// UTF-16LE up to the first NUL; invalid surrogates become U+FFFD.
fn decode_path(raw: &[u8]) -> Box<str> {
    let units = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&u| u != 0);
    char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect::<String>()
        .into_boxed_str()
}

fn inflate(stored: &[u8], raw_size: u64) -> Result<Vec<u8>> {
    let cap = raw_size.min(stored.len() as u64 * MAX_ZLIB_RATIO + 64);
    let mut out = Vec::with_capacity(cap as usize);
    ZlibDecoder::new(stored)
        .take(raw_size.saturating_add(1))
        .read_to_end(&mut out)
        .context("zlib stream")?;
    ensure!(
        out.len() as u64 == raw_size,
        "zlib stream inflated to {} B, header says {raw_size}",
        out.len()
    );
    Ok(out)
}

// ------------------------------------------------------------------ writing

/// When a member is stored as zlib.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZlibMode {
    /// Per file, only when it shrinks (rda_pack.py default).
    IfSmaller,
    /// Every member, like the shipped `.a7m` islands.
    Always,
}

#[derive(Debug, Clone)]
pub struct PackOptions {
    level: u32,
    zlib: ZlibMode,
    /// Uncompressed bytes per block before starting a new one. Only the header
    /// table has to fit u32; 1 GiB keeps blocks a sane size to stream.
    max_block_raw: u64,
    max_block_files: u32,
    /// Unix seconds stamped on every member.
    timestamp: u64,
    version: Version,
}

impl Default for PackOptions {
    fn default() -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        PackOptions {
            level: 6,
            zlib: ZlibMode::IfSmaller,
            max_block_raw: 1 << 30,
            max_block_files: MAX_FILES_PER_BLOCK,
            timestamp: now,
            // The newest version 1800 and 117 read; 2070 reads only v2.0.
            version: Version::V22,
        }
    }
}

impl PackOptions {
    /// Writes archive `version` instead of v2.2: a mod for 2070, which
    /// reads only v2.0.
    pub fn version(mut self, version: Version) -> PackOptions {
        self.version = version;
        self
    }

    /// Shipped island shape: one zlib member per block, timestamp 0.
    pub fn island() -> Self {
        PackOptions {
            zlib: ZlibMode::Always,
            max_block_files: 1,
            timestamp: 0,
            ..PackOptions::default()
        }
    }
}

struct PackedFile {
    path: String,
    stored: Vec<u8>,
    raw_size: u64,
    zlib: bool,
}

/// Collects members (compressed as they are added) and tombstones, then
/// writes an archive of `PackOptions::version`. Zero-length files are
/// refused: the engine reports them NOT FOUND, so shipping one is always a
/// mistake; delete with `Packer::tombstone` instead.
pub struct Packer {
    opts: PackOptions,
    files: Vec<PackedFile>,
    tombstones: Vec<String>,
    // lookup key -> path as added, for case-collision errors
    keys: HashMap<String, String>,
}

impl Packer {
    pub fn new(opts: PackOptions) -> Packer {
        Packer {
            opts,
            files: Vec::new(),
            tombstones: Vec::new(),
            keys: HashMap::new(),
        }
    }

    pub fn add(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let path = self.claim(path)?;
        ensure!(
            !data.is_empty(),
            "{path}: zero-length file; the engine treats it as not found"
        );
        let z = deflate(data, self.opts.level)?;
        let zlib = (self.opts.zlib == ZlibMode::Always || z.len() < data.len()).then_some(z);
        self.files.push(PackedFile {
            path,
            raw_size: data.len() as u64,
            zlib: zlib.is_some(),
            stored: zlib.unwrap_or_else(|| data.to_vec()),
        });
        Ok(())
    }

    /// Delete `path` from every archive mounted before this one. Written in
    /// the shipped shape: one flags=8 block per path, raw table, zero fields.
    /// Only tests write deletions today, to check the readers honour them.
    #[cfg(test)]
    pub(crate) fn tombstone(&mut self, path: &str) -> Result<()> {
        let path = self.claim(path)?;
        self.tombstones.push(path);
        Ok(())
    }

    fn claim(&mut self, path: &str) -> Result<String> {
        let path = clean_pack_path(path)?;
        let key = path.to_ascii_lowercase();
        if let Some(prev) = self.keys.get(&key) {
            bail!("duplicate path (the engine matches case-insensitively): {prev} vs {path}");
        }
        self.keys.insert(key, path.clone());
        Ok(path)
    }

    /// The serialized archive.
    pub fn encode<'a>(&'a self) -> Result<Vec<u8>> {
        ensure!(
            !self.files.is_empty() || !self.tombstones.is_empty(),
            "nothing to pack"
        );
        let layout = self.opts.version.layout();
        let timestamp = self.opts.timestamp;
        let member = |f: &'a PackedFile| Member {
            path: &f.path,
            stored: &f.stored,
            raw_size: f.raw_size,
            timestamp,
            unknown: 0,
        };
        // Blocks are uniform (all zlib or all raw), like every shipped block.
        // zlib members go first, then raw ones; input order inside each.
        let mut blocks: Vec<Block> = Vec::new();
        for zlib in [true, false] {
            let flags = match zlib {
                // As 2070's own packer writes its zlib blocks.
                true if layout.encrypts => BlockFlags::ZLIB | BlockFlags::ENCRYPTED,
                true => BlockFlags::ZLIB,
                false => 0,
            };
            let mut cur: Vec<Member> = Vec::new();
            let mut raw = 0u64;
            for f in self.files.iter().filter(|f| f.zlib == zlib) {
                let full = cur.len() as u32 >= self.opts.max_block_files.max(1)
                    || raw + f.raw_size > self.opts.max_block_raw;
                if !cur.is_empty() && full {
                    blocks.push(Block::new(flags, std::mem::take(&mut cur)));
                    raw = 0;
                }
                raw += f.raw_size;
                cur.push(member(f));
            }
            if !cur.is_empty() {
                blocks.push(Block::new(flags, cur));
            }
        }
        blocks.extend(self.tombstones.iter().map(|t| {
            let gone = Member {
                path: t,
                stored: &[],
                raw_size: 0,
                timestamp: 0,
                unknown: 0,
            };
            Block::new(BlockFlags::DELETED, vec![gone])
        }));
        write(self.opts.version, &blocks, self.opts.level)
    }
}

/// One block as [`write`] lays it out.
pub(crate) struct Block<'a> {
    flags: u32,
    members: Vec<Member<'a>>,
    /// The header table's zlib stream as some archive stores it, written
    /// instead of deflating the table when it inflates to exactly the table
    /// written. Deflaters differ, so this keeps a shipped archive's bytes.
    kept_table: Option<&'a [u8]>,
    /// The same for a memres block's section.
    kept_section: Option<&'a [u8]>,
}

/// One member of a [`Block`]. Its stored bytes are a zlib stream in a zlib
/// block and the file itself otherwise, before any encryption; a memres
/// member's are its file, which the block's section packs.
pub(crate) struct Member<'a> {
    path: &'a str,
    stored: &'a [u8],
    raw_size: u64,
    timestamp: u64,
    unknown: u64,
}

impl<'a> Block<'a> {
    fn new(flags: u32, members: Vec<Member<'a>>) -> Block<'a> {
        Block {
            flags,
            members,
            kept_table: None,
            kept_section: None,
        }
    }
}

/// Writes `blocks` as a `version` archive, each in order: its member bytes
/// (or section), header table, memres sizes, then block header. A tombstone
/// block's members have no bytes and zero offsets and sizes.
pub(crate) fn write(version: Version, blocks: &[Block], level: u32) -> Result<Vec<u8>> {
    let l = version.layout();
    ensure!(!blocks.is_empty(), "an archive needs a block");
    let mut out = l.encode_header(0)?;
    let mut headers: Vec<(usize, BlockHeader)> = Vec::with_capacity(blocks.len());
    for b in blocks {
        let flags = BlockFlags(b.flags);
        ensure!(
            !flags.encrypted() || l.encrypts,
            "{version:?} archives have no encrypted blocks"
        );
        let push = |out: &mut Vec<u8>, stored: &[u8]| {
            let at = out.len();
            out.extend_from_slice(stored);
            if flags.encrypted() {
                crypt(&mut out[at..]);
            }
        };
        // Deflated, unless what was read inflates to the same bytes.
        let zlib = |raw: &[u8], kept: Option<&[u8]>| -> Result<Vec<u8>> {
            match kept {
                Some(k) if inflate(k, raw.len() as u64).is_ok_and(|i| i == raw) => Ok(k.to_vec()),
                _ => deflate(raw, level),
            }
        };
        let mut table = Vec::with_capacity(l.file_header * b.members.len());
        let mut section = None;
        if flags.deleted() {
            for m in &b.members {
                l.push_file(&mut table, m.path, [0, 0, 0, m.timestamp, m.unknown])?;
            }
        } else if flags.memres() && !b.members.is_empty() {
            // Offsets count from the section's start; stored and raw size
            // are both the file's, as 2070's memres records have them. An
            // empty memres block has no section, nor its sizes (2070's
            // addon1.rda has three).
            let mut raw = Vec::new();
            for m in &b.members {
                let (at, len) = (raw.len() as u64, m.stored.len() as u64);
                l.push_file(&mut table, m.path, [at, len, len, m.timestamp, m.unknown])?;
                raw.extend_from_slice(m.stored);
            }
            let stored = match flags.compressed() {
                true => zlib(&raw, b.kept_section)?,
                false => raw.clone(),
            };
            push(&mut out, &stored);
            section = Some((stored.len() as u64, raw.len() as u64));
        } else {
            for m in &b.members {
                let at = out.len() as u64;
                push(&mut out, m.stored);
                let words = [
                    at,
                    m.stored.len() as u64,
                    m.raw_size,
                    m.timestamp,
                    m.unknown,
                ];
                l.push_file(&mut table, m.path, words)?;
            }
        }
        let raw_len = table.len() as u64;
        ensure!(
            raw_len <= u32::MAX as u64,
            "header table of {raw_len} B does not fit u32; lower max_block_files"
        );
        let stored = match flags.compressed() && !table.is_empty() {
            true => zlib(&table, b.kept_table)?,
            false => table,
        };
        push(&mut out, &stored);
        if let Some((stored, raw)) = section {
            l.push_word(&mut out, stored)?;
            l.push_word(&mut out, raw)?;
        }
        let header = BlockHeader {
            flags,
            files: b.members.len() as u32,
            table_stored: stored.len() as u64,
            table_raw: raw_len,
            next: 0,
        };
        headers.push((out.len(), header));
        out.resize(out.len() + l.block_header as usize, 0);
    }
    let end = out.len() as u64;
    l.put_word(&mut out[l.first_block_at..], headers[0].0 as u64)?;
    for i in 0..headers.len() {
        let next = headers.get(i + 1).map_or(end, |h| h.0 as u64);
        let (at, h) = headers[i];
        l.put_block(&mut out[at..], &BlockHeader { next, ..h })?;
    }
    Ok(out)
}

/// A zlib stream with the header bytes `78 01`, whatever the level. The
/// engine's inflater compares the first two bytes against
/// exactly `78 01` and fails the read otherwise; the level bits are only a
/// hint, so `78 01` is valid zlib for any level (0x7801 % 31 == 0).
fn deflate(data: &[u8], level: u32) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 2 + 6);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut z = DeflateEncoder::new(out, Compression::new(level));
    z.write_all(data)?;
    let mut out = z.finish()?;
    out.extend_from_slice(&adler32(data).to_be_bytes());
    Ok(out)
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    // 5552 bytes is the longest run before `b` can overflow u32.
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

// Writer-side path rules: `/` form, no `.`/`..`, no drive letter, fits the
// 520-byte UTF-16 slot. Case is kept; the engine folds it at lookup.
fn clean_pack_path(path: &str) -> Result<String> {
    let p = path.trim().replace('\\', "/");
    ensure!(
        !(p.len() >= 2 && p.as_bytes()[1] == b':'),
        "{path}: drive-qualified path has no place inside an RDA"
    );
    ensure!(!p.contains('\0'), "{path:?}: NUL in path");
    let mut out = String::with_capacity(p.len());
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => bail!("{path}: `..` segments are not allowed"),
            seg => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(seg);
            }
        }
    }
    ensure!(!out.is_empty(), "{path:?}: empty path");
    let units = out.encode_utf16().count();
    ensure!(
        units <= MAX_PATH_UNITS,
        "{out}: {units} UTF-16 units, an RDA path has room for {MAX_PATH_UNITS}"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    const SCRIPTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts");

    fn text(n: usize) -> Vec<u8> {
        (0..n).map(|i| b"abcabcabd"[i % 9]).collect()
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn python() -> bool {
        Command::new("python3").arg("--version").output().is_ok()
    }

    fn opts(version: Version) -> PackOptions {
        PackOptions::default().version(version)
    }

    #[test]
    fn roundtrip_raw_and_zlib() {
        for version in Version::ALL {
            let mut p = Packer::new(opts(version));
            p.add("data/a.xml", &text(10_000)).unwrap();
            p.add("Data\\B.bin", &noise(4096)).unwrap();
            p.add("./data/c.txt", b"x").unwrap();
            p.tombstone("data/gone.txt").unwrap();
            let bytes = p.encode().unwrap();
            assert_eq!(Version::of(&bytes), Some(version));
            let a = Archive::parse(bytes, "mem").unwrap();

            assert_eq!(a.entries().len(), 3);
            let e = a.find("DATA/A.XML").unwrap();
            assert!(e.flags.compressed());
            assert_eq!(e.flags.encrypted(), version == Version::V20);
            assert!(e.compressed_size < e.raw_size);
            assert_eq!(a.read(e).unwrap(), text(10_000));
            let b = a.find("data/b.bin").unwrap();
            assert_eq!(&*b.path, "Data/B.bin");
            assert!(!b.flags.compressed(), "incompressible member must stay raw");
            assert_eq!(a.read(b).unwrap(), noise(4096));
            assert_eq!(a.read_path("data/c.txt").unwrap(), b"x");

            assert_eq!(a.tombstones().len(), 1);
            assert!(a.tombstones()[0].flags.deleted());
            assert!(a.find("data/gone.txt").is_none());
        }
    }

    // Both fast paths against the slow one, on paths built from the
    // characters normalizing cares about, with junk after the NUL.
    #[test]
    fn stored_paths_key_as_normalize_path_does() {
        let alphabet: [u16; 12] = [
            b'a' as u16,
            b'Z' as u16,
            b'/' as u16,
            b'\\' as u16,
            b'.' as u16,
            b'_' as u16,
            b'0' as u16,
            b'-' as u16,
            0xe9,
            0x4e2d,
            0xd800,
            b'A' as u16,
        ];
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut fast_hits = [0usize; 2];
        for round in 0..200_000 {
            let len = match round % 7 {
                0 => 250 + next() as usize % 11,
                _ => next() as usize % 40,
            }
            .min(MAX_PATH_UNITS + 1);
            // Mostly plain paths, so the fast path is what gets tested.
            let plain = round % 3 != 0;
            let mut raw = vec![0u8; PATH_BYTES];
            for i in 0..len {
                let r = next() as usize;
                let u = if plain {
                    b"aqZ/_0"[r % 6] as u16
                } else {
                    alphabet[r % alphabet.len()]
                };
                raw[2 * i..2 * i + 2].copy_from_slice(&u.to_le_bytes());
            }
            for b in raw.iter_mut().skip(2 * len + 2) {
                *b = next() as u8 | 1;
            }
            let want = normalize_path(&decode_path(&raw)).into_owned();
            let want_cut = want.rfind('/');
            let mut out = b"prefix".to_vec();
            let cut = push_key(&raw, &mut out);
            assert_eq!(std::str::from_utf8(&out[6..]).unwrap(), want, "{raw:?}");
            assert_eq!(cut, want_cut, "{want}");
            for (hits, fast) in fast_hits.iter_mut().zip([push_plain, push_plain_scalar]) {
                let mut out = b"prefix".to_vec();
                if let Some(cut) = fast(&raw, &mut out) {
                    *hits += 1;
                    assert_eq!(std::str::from_utf8(&out[6..]).unwrap(), want, "{raw:?}");
                    assert_eq!(cut, want_cut, "{want}");
                }
            }
        }
        assert!(fast_hits.iter().all(|&h| h > 40_000), "{fast_hits:?}");
    }

    // A 2070 mod packed the way a caller outside formats packs it, through
    // the public API alone, reads back as v2.0 with every member intact.
    #[test]
    fn a_mod_packs_as_v20_and_reads_back() {
        let mut p = Packer::new(PackOptions::default().version(Version::V20));
        p.add("data/config/game/assets.xml", &text(20_000)).unwrap();
        p.add("data/graphics/noise.bin", &noise(3000)).unwrap();
        let bytes = p.encode().unwrap();
        assert_eq!(Version::of(&bytes), Some(Version::V20));
        let a = Archive::parse(bytes, "mod.rda").unwrap();
        let read: Vec<(&str, Vec<u8>)> = (a.entries().iter())
            .map(|e| (&*e.path, a.read(e).unwrap()))
            .collect();
        assert_eq!(
            read,
            [
                ("data/config/game/assets.xml", text(20_000)),
                ("data/graphics/noise.bin", noise(3000)),
            ]
        );
    }

    #[test]
    fn block_caps_split_blocks() {
        for version in Version::ALL {
            let mut p = Packer::new(PackOptions {
                max_block_raw: 3000,
                ..opts(version)
            });
            for i in 0..5 {
                p.add(&format!("f{i}"), &text(1000 + i)).unwrap();
            }
            let bytes = p.encode().unwrap();
            assert_eq!(chain(&bytes).1.len(), 3, "{version:?}");
            let a = Archive::parse(bytes, "mem").unwrap();
            for i in 0..5 {
                assert_eq!(a.read_path(&format!("F{i}")).unwrap(), text(1000 + i));
            }
        }
    }

    #[test]
    fn case_collision_and_bad_paths_refused() {
        let mut p = Packer::new(PackOptions::default());
        p.add("data/A.txt", b"1").unwrap();
        assert!(p.add("DATA/a.TXT", b"2").is_err());
        assert!(p.tombstone("data\\a.txt").is_err());
        assert!(p.add("data/empty", b"").is_err());
        assert!(p.add("../x", b"1").is_err());
        assert!(p.add("c:/x", b"1").is_err());
        assert!(p.add(&"a".repeat(MAX_PATH_UNITS + 1), b"1").is_err());
        assert!(p.add(&"a".repeat(MAX_PATH_UNITS), b"1").is_ok());
        assert!(Packer::new(PackOptions::default()).encode().is_err());
    }

    fn member<'a>(path: &'a str, data: &'a [u8]) -> Member<'a> {
        Member {
            path,
            stored: data,
            raw_size: data.len() as u64,
            timestamp: 0,
            unknown: 0,
        }
    }

    /// The archive's version and its block headers in chain order, with
    /// where each sits.
    fn chain(bytes: &[u8]) -> (Version, Vec<(usize, BlockHeader)>) {
        let version = Version::of(bytes).unwrap();
        let l = version.layout();
        let mut out = Vec::new();
        let mut at = l.first_block(bytes);
        while at != bytes.len() as u64 {
            let h = l.block(&bytes[at as usize..]);
            out.push((at as usize, h));
            at = h.next;
        }
        (version, out)
    }

    /// Rewrites the block header at `at` as `edit` changes it.
    fn patch(bytes: &mut [u8], at: usize, edit: impl FnOnce(&mut BlockHeader)) {
        let l = Version::of(bytes).unwrap().layout();
        let mut h = l.block(&bytes[at..]);
        edit(&mut h);
        l.put_block(&mut bytes[at..], &h).unwrap();
    }

    // Two live blocks repeating a path, then a tombstone block, then a block
    // that brings a tombstoned path back.
    #[test]
    fn last_wins_and_tombstones_within_one_chain() {
        for version in Version::ALL {
            let blocks = [
                // graphics_ui.rda opens with an empty block
                Block::new(0, vec![]),
                Block::new(
                    0,
                    vec![
                        member("a", b"old"),
                        member("b", b"bee"),
                        member("c", b"sea"),
                    ],
                ),
                Block::new(0, vec![member("A", b"new")]),
                Block::new(
                    BlockFlags::DELETED,
                    vec![member("b", b""), member("c", b"")],
                ),
                Block::new(0, vec![member("c", b"back")]),
            ];
            let out = write(version, &blocks, 6).unwrap();
            let a = Archive::parse(out.clone(), "chain").unwrap();
            assert_eq!(a.read_path("a").unwrap(), b"new");
            assert!(a.find("b").is_none());
            assert_eq!(a.read_path("c").unwrap(), b"back");
            let tombs: Vec<_> = a.tombstones().iter().map(|e| &*e.path).collect();
            assert_eq!(tombs, ["b"]);

            let (_, headers) = chain(&out);
            assert_eq!(headers.len(), blocks.len());
            // A pointer past EOF must fail, not end the chain quietly.
            let mut bad = out.clone();
            patch(&mut bad, headers[1].0, |h| h.next = out.len() as u64 + 1);
            assert!(Archive::parse(bad, "bad").is_err());
            // So must a cycle.
            let mut cyc = out;
            patch(&mut cyc, headers[2].0, |h| h.next = headers[1].0 as u64);
            assert!(Archive::parse(cyc, "cycle").is_err());
        }
    }

    #[test]
    fn nested_island_shape_parses() {
        let mut p = Packer::new(PackOptions::island());
        p.add("gamedata.data", &text(50_000)).unwrap();
        p.add("rd3d.data", &noise(1000)).unwrap(); // forced zlib even if it grows
        let a = Archive::parse(p.encode().unwrap(), "island.a7m").unwrap();
        let names: Vec<_> = a.entries().iter().map(|e| &*e.path).collect();
        assert_eq!(names, ["gamedata.data", "rd3d.data"]);
        assert!(a.entries().iter().all(|e| e.flags.compressed()));
        assert_eq!(a.read_path("rd3d.data").unwrap(), noise(1000));
    }

    #[test]
    fn zlib_members_carry_the_header_the_engine_accepts() {
        // The engine fails any read whose stream does not start `78 01`;
        // standard encoders write `78 9c` at default level.
        let mut p = Packer::new(PackOptions::island());
        let data = text(200_000);
        p.add("gamedata.data", &data).unwrap();
        let bytes = p.encode().unwrap();
        let a = Archive::parse(bytes.clone(), "island.a7m").unwrap();
        let e = &a.entries()[0];
        let stream = &bytes[e.offset as usize..(e.offset + e.compressed_size) as usize];
        assert_eq!(stream[..2], [0x78, 0x01]);
        assert!(e.compressed_size < e.raw_size / 10, "still compresses");
        // ZlibDecoder checks the adler32 trailer.
        let mut back = Vec::new();
        ZlibDecoder::new(stream).read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
        assert_eq!(super::adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn truncated_and_foreign_input_rejected() {
        for version in Version::ALL {
            let magic = version.layout().magic.to_vec();
            assert!(Archive::parse(magic, "short").is_err());
        }
        assert!(Archive::parse(vec![0; Layout::LONGEST_HEADER], "zero").is_err());
        let mut p = Packer::new(PackOptions::default());
        p.add("x", &text(5000)).unwrap();
        let mut bytes = p.encode().unwrap();
        bytes.truncate(bytes.len() - 1);
        assert!(Archive::parse(bytes, "cut").is_err());
    }

    #[test]
    fn packed_archive_passes_python_verify() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zz_patchfiles_999_test.rda");
        let mut p = Packer::new(PackOptions::default());
        p.add("data/ui/a.json", &text(20_000)).unwrap();
        p.add("data/ui/b.dds", &noise(3000)).unwrap();
        p.tombstone("data/ui/old.json").unwrap();
        std::fs::write(&out, p.encode().unwrap()).unwrap();
        let a = Archive::open(&out).unwrap();
        assert_eq!(a.read_path("data/ui/b.dds").unwrap(), noise(3000));

        if !python() {
            eprintln!("python3 missing; skipping rda_pack.py verify");
            return;
        }
        let r = Command::new("python3")
            .args(["-I", &format!("{SCRIPTS}/rda_pack.py"), "verify"])
            .arg(&out)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&r.stdout);
        assert!(r.status.success() && log.contains("verify: OK"), "{log}");
        assert!(!log.contains("warn"), "{log}");
    }

    fn game_dir() -> Option<PathBuf> {
        crate::vfs::install_dir("ANNO117_DIR")
    }

    // Independent check: bytes must match what rda.py extracts.
    #[test]
    fn real_reads_match_python_extract() {
        let Some(game) = game_dir() else { return };
        if !python() {
            return;
        }
        let path = game.join("maindata/config.rda");
        let a = Archive::open(&path).unwrap();
        let picks: Vec<&Entry> = a.entries().iter().step_by(40).take(5).collect();
        let dir = tempfile::tempdir().unwrap();
        for e in picks {
            let rx = format!("^{}$", regex_escape(&e.path));
            let ok = Command::new("python3")
                .args(["-I", &format!("{SCRIPTS}/rda.py"), "extract"])
                .arg(&path)
                .arg(dir.path())
                .args([&rx, "--overwrite"])
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            assert!(ok);
            let want = std::fs::read(dir.path().join(&*e.path)).unwrap();
            assert_eq!(a.read(e).unwrap(), want, "{}", e.path);
        }
    }

    fn regex_escape(s: &str) -> String {
        s.chars()
            .flat_map(|c| {
                let special = "\\.+*?()|[]{}^$".contains(c);
                special.then_some('\\').into_iter().chain([c])
            })
            .collect()
    }

    #[test]
    fn real_island_repack_roundtrip() {
        if game_dir().is_none() {
            return;
        }
        let src = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../extract/provinces_roman/data/base/provinces/roman/\
             islands/3rdparty/roman_island_3rdparty_pirate_01/\
             roman_island_3rdparty_pirate_01.a7m"
        );
        let Ok(bytes) = std::fs::read(src) else {
            eprintln!("{src} missing; skipping");
            return;
        };
        let orig = Archive::parse(bytes, src).unwrap();
        let mut p = Packer::new(PackOptions::island());
        let mut members = Vec::new();
        for e in orig.entries() {
            let data = orig.read(e).unwrap();
            p.add(&e.path, &data).unwrap();
            members.push((e.path.clone(), data));
        }
        assert_eq!(members.len(), 2);
        let again = Archive::parse(p.encode().unwrap(), "repacked").unwrap();
        assert_eq!(again.entries().len(), 2);
        for (path, data) in &members {
            let e = again.find(path).unwrap();
            assert!(e.flags.compressed());
            assert_eq!(&again.read(e).unwrap(), data, "{path}");
        }
    }

    /// A v2.0 archive as Anno 2070 writes them: one encrypted zlib block of
    /// `plain` files, then one encrypted zlib memres block of `resident`
    /// ones.
    fn v20(plain: &[(&str, &[u8])], resident: &[(&str, &[u8])]) -> Vec<u8> {
        let zlib: Vec<Vec<u8>> = plain.iter().map(|(_, d)| deflate(d, 6).unwrap()).collect();
        let sealed = BlockFlags::ZLIB | BlockFlags::ENCRYPTED;
        let blocks = [
            Block::new(
                sealed,
                plain
                    .iter()
                    .zip(&zlib)
                    .map(|((path, data), z)| Member {
                        stored: z,
                        timestamp: 7,
                        ..member(path, data)
                    })
                    .collect(),
            ),
            Block::new(
                sealed | BlockFlags::MEMRES,
                resident.iter().map(|(p, d)| member(p, d)).collect(),
            ),
        ];
        write(Version::V20, &blocks, 6).unwrap()
    }

    // Anno 2070's archives: UTF-16 magic, u32 fields, encrypted tables and
    // payloads, memres sections; every member reads back as written.
    #[test]
    fn v20_archives_decrypt_and_read() {
        let script = text(3000);
        let plain: &[(&str, &[u8])] = &[
            ("data/ai/roamer.csl", b"module Roamer(radius)"),
            ("data/big.txt", &script),
        ];
        let resident: &[(&str, &[u8])] = &[("data/a.cfg", b"<Config/>"), ("data/b.cfg", b"<B/>")];
        let bytes = v20(plain, resident);
        assert_eq!(Version::of(&bytes), Some(Version::V20));
        let a = Archive::parse(bytes.clone(), "v20").unwrap();
        assert_eq!(a.entries().len(), 4);
        for (path, data) in plain.iter().chain(resident) {
            assert_eq!(&a.read_path(path).unwrap(), data, "{path}");
        }
        let e = a.find("data/big.txt").unwrap();
        assert!(e.flags.encrypted() && e.flags.compressed());
        assert_eq!(a.read_range(e, 10, 5).unwrap(), script[10..15]);
        assert!(a.find("data/b.cfg").unwrap().flags.memres());

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let path = dir.path().join("maindata/Data0.rda");
        std::fs::write(&path, &bytes).unwrap();
        assert!(Archive::sniff(&path));
        // As the install index reads it: the tables decrypted in place, the
        // archive kept for reads with only its section table.
        let install =
            crate::vfs::Install::open(dir.path(), crate::vfs::Install::DATA_FIRST).unwrap();
        for (path, data) in plain.iter().chain(resident) {
            assert_eq!(&install.read(path).unwrap(), data, "{path}");
        }
    }

    #[test]
    fn v20_damage_is_refused() {
        let mut bytes = v20(&[("x.txt", b"hello")], &[("y.txt", b"y")]);
        let first = chain(&bytes).1[0].0;
        // The first block's header table size, past what precedes it.
        patch(&mut bytes, first, |h| h.table_stored = u32::MAX as u64);
        assert!(Archive::parse(bytes, "bad").is_err());
        // v2.0 has no room for a word above 4 GiB, and v2.2 no encryption.
        let mut word = vec![0; Layout::V20.word];
        assert!(
            Layout::V20
                .put_word(&mut word, u32::MAX as u64 + 1)
                .is_err()
        );
        let sealed = Block::new(BlockFlags::ZLIB | BlockFlags::ENCRYPTED, vec![]);
        assert!(write(Version::V22, &[sealed], 6).is_err());
    }

    /// A shipped archive's chain as blocks for [`write`], each member's
    /// stored bytes, table and section as the archive stores them.
    struct Shipped {
        version: Version,
        blocks: Vec<ShippedBlock>,
    }

    struct ShippedBlock {
        flags: u32,
        members: Vec<(String, Vec<u8>, Record<'static>)>,
        kept_table: Option<Vec<u8>>,
        kept_section: Option<Vec<u8>>,
    }

    impl Shipped {
        fn read(bytes: &[u8]) -> Shipped {
            let (version, headers) = chain(bytes);
            let l = version.layout();
            let open = |at: u64, len: u64, flags: BlockFlags| {
                let mut b = bytes[at as usize..(at + len) as usize].to_vec();
                if flags.encrypted() {
                    crypt(&mut b);
                }
                b
            };
            let mut blocks = Vec::new();
            for (at, h) in headers {
                let flags = h.flags;
                let memres = flags.memres() && h.files > 0;
                let extra = if memres { l.memres_info } else { 0 };
                let table_at = at as u64 - extra - h.table_stored;
                let stored = open(table_at, h.table_stored, flags);
                let zlib = h.table_stored != h.table_raw;
                let table = match zlib {
                    true => inflate(&stored, h.table_raw).unwrap(),
                    false => stored.clone(),
                };
                let mut section = None;
                if memres {
                    let (stored, raw) = l.memres(&bytes[at - extra as usize..]);
                    let kept = open(table_at - stored, stored, flags);
                    let raw = match flags.compressed() {
                        true => inflate(&kept, raw).unwrap(),
                        false => kept.clone(),
                    };
                    section = Some((kept, raw));
                }
                let members = table
                    .chunks_exact(l.file_header)
                    .map(|rec| {
                        let r = l.file(rec);
                        let data = match &section {
                            _ if flags.deleted() => Vec::new(),
                            Some((_, raw)) => {
                                raw[r.offset as usize..][..r.raw_size as usize].to_vec()
                            }
                            None => open(r.offset, r.compressed_size, flags),
                        };
                        let r = Record { path: &[], ..r };
                        (decode_path(rec).into(), data, r)
                    })
                    .collect();
                blocks.push(ShippedBlock {
                    flags: flags.0,
                    members,
                    kept_table: zlib.then_some(stored),
                    kept_section: section.map(|s| s.0),
                });
            }
            Shipped { version, blocks }
        }

        fn write(&self) -> Result<Vec<u8>> {
            let blocks: Vec<Block> = (self.blocks.iter())
                .map(|b| Block {
                    flags: b.flags,
                    members: (b.members.iter())
                        .map(|(path, data, r)| Member {
                            path,
                            stored: data,
                            raw_size: r.raw_size,
                            timestamp: r.timestamp,
                            unknown: r.unknown,
                        })
                        .collect(),
                    kept_table: b.kept_table.as_deref(),
                    kept_section: b.kept_section.as_deref(),
                })
                .collect();
            write(self.version, &blocks, 6)
        }
    }

    // Needs the installs: shipped archives of each version, read into blocks
    // and written back, come out byte for byte. 2070's zlib streams are kept
    // as stored (no zlib setting reproduces its packer's), everything around
    // them is written: header, chain, tables, encryption, memres sections and
    // tombstones.
    #[test]
    fn shipped_archives_write_back_byte_exact() {
        let goldens: [(&str, &[&str]); 3] = [
            // empty blocks; tombstones; memres sections and plain blocks
            (
                "ANNO2070_DIR",
                &["addon_ger1.rda", "eng8.rda", "patch8.rda"],
            ),
            // 117's own packer leaves gaps before some blocks (infotips.rda
            // starts its data at 1024, not 792); the writer packs back to
            // back, so its goldens are archives without gaps.
            ("ANNO117_DIR", &["file_browse_patterns.rda"]),
            ("ANNO1800_DIR", &["data5.rda"]),
        ];
        for (var, names) in goldens {
            let Some(dir) = crate::vfs::install_dir(var) else {
                continue;
            };
            for name in names {
                let bytes = std::fs::read(dir.join("maindata").join(name)).unwrap();
                let shipped = Shipped::read(&bytes);
                let again = shipped.write().unwrap();
                let first = (bytes.iter().zip(&again)).position(|(a, b)| a != b);
                assert!(
                    again == bytes,
                    "{name}: {} B written for {} B, first difference at {first:?}",
                    again.len(),
                    bytes.len()
                );
            }
        }
    }

    // Needs Anno 2070 (ANNO2070_DIR): every archive in maindata opens, and
    // files from plain, encrypted and memres blocks read as what they are.
    #[test]
    fn real_2070_archives_read() {
        let Some(dir) = crate::vfs::install_dir("ANNO2070_DIR") else {
            return;
        };
        let maindata = dir.join("maindata");
        let mut opened = 0;
        for item in std::fs::read_dir(&maindata).unwrap() {
            let path = item.unwrap().path();
            if !Archive::sniff(&path) {
                // checksums.rda is a text list, not an archive.
                continue;
            }
            Archive::open(&path).unwrap();
            opened += 1;
        }
        assert!(opened >= 40, "{opened}");
        let data0 = Archive::open(maindata.join("Data0.rda")).unwrap();
        let roamer = data0.read_path("data/ai/ai/roamer.csl").unwrap();
        assert!(roamer.starts_with(b"module Roamer"));
        let data2 = Archive::open(maindata.join("Data2.rda")).unwrap();
        let cfg = (data2.entries().iter())
            .find(|e| e.flags.memres() && e.path.ends_with(".cfg"))
            .unwrap();
        let text = data2.read(cfg).unwrap();
        // 2070's cfgs are its serializer's XML: every element `m_`-named.
        assert!(text.starts_with(b"<m_Config>"), "{}", cfg.path);
        let plain = (data2.entries().iter())
            .find(|e| !e.flags.encrypted() && e.path.ends_with(".dds"))
            .unwrap();
        assert!(
            data2.read(plain).unwrap().starts_with(b"DDS "),
            "{}",
            plain.path
        );
    }

    // Opening an install with its archives' header tables out of the page
    // cache, as after a restart: only the pages of tables and block headers
    // are dropped (`posix_fadvise`), not the rest of the archives. Pages
    // another process maps stay. `cargo test --release -p anno-formats
    // index_build_cold -- --ignored --nocapture`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn index_build_cold() {
        use std::os::fd::AsRawFd;
        let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
        eprintln!(
            "load average {}",
            load.split(' ').take(3).collect::<Vec<_>>().join(" ")
        );
        for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                continue;
            };
            let mut dropped = 0u64;
            for item in std::fs::read_dir(dir.join("maindata")).unwrap() {
                let path = item.unwrap().path();
                if !path.extension().is_some_and(|e| e == "rda") || !Archive::sniff(&path) {
                    continue;
                }
                let file = File::open(&path).unwrap();
                let map = map(&file).unwrap();
                let mut spans = Vec::new();
                walk(&map, "cold", |t| {
                    let end = t.ptr + t.layout.block_header;
                    let table = t.records.len() as u64 + t.layout.memres_info;
                    spans.push((t.ptr.saturating_sub(table), end));
                    Ok(())
                })
                .unwrap();
                drop(map);
                for (from, to) in spans {
                    let from = from & !4095;
                    dropped += to - from;
                    // SAFETY: a plain advisory call on an open descriptor.
                    let r = unsafe {
                        libc::posix_fadvise(
                            file.as_raw_fd(),
                            from as i64,
                            (to - from) as i64,
                            libc::POSIX_FADV_DONTNEED,
                        )
                    };
                    assert_eq!(r, 0);
                }
            }
            let majflt = || {
                let mut u: libc::rusage = unsafe { std::mem::zeroed() };
                // SAFETY: `u` is a valid out pointer.
                unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
                u.ru_majflt
            };
            let (faults, t) = (majflt(), std::time::Instant::now());
            let install = crate::vfs::Install::open(&dir, crate::vfs::Install::DATA_FIRST).unwrap();
            let (cold, faults) = (t.elapsed(), majflt() - faults);
            drop(install);
            let t = std::time::Instant::now();
            drop(crate::vfs::Install::open(&dir, crate::vfs::Install::DATA_FIRST).unwrap());
            eprintln!(
                "{var}: {:.0} MB of tables dropped, cold {:.1} ms ({faults} major faults), then warm {:.1} ms",
                dropped as f64 / 1e6,
                cold.as_secs_f64() * 1e3,
                t.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}
