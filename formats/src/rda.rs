// RDA v2.2 archives ("Resource File V2.2"): walking the block chain, reading
// entry payloads, and packing new archives for mods and nested `.a7m` islands.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::DeflateEncoder;

const MAGIC: &[u8; 18] = b"Resource File V2.2";
/// Size of the top-level header; the first block payload starts here.
pub const HEADER_SIZE: u64 = 792;
const FIRST_PTR_OFF: usize = 784;
const BLOCK_HEADER: u64 = 32;
const FILE_HEADER: usize = 560;
const PATH_BYTES: usize = 520;
// [u64 cmpSize][u64 rawSize] between the header table and the block header of
// a memory-resident block.
const MEMRES_INFO: u64 = 16;
// The engine's chain walker rejects blocks above this.
const MAX_FILES_PER_BLOCK: u32 = 10_000_000;
// 520 bytes of UTF-16 including the NUL terminator.
const MAX_PATH_UNITS: usize = 259;
// Deflate cannot expand more than ~1032:1; caps the up-front allocation when
// a header claims a huge raw size.
const MAX_ZLIB_RATIO: u64 = 1032;

/// Cheap discovery check; indexing and decompression remain in `Archive`.
pub(crate) fn has_header(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut head = [0; HEADER_SIZE as usize];
    if file.read_exact(&mut head).is_err() || &head[..MAGIC.len()] != MAGIC {
        return false;
    }
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let first = u64_at(&head, FIRST_PTR_OFF);
    first >= HEADER_SIZE
        && (first == meta.len()
            || first
                .checked_add(BLOCK_HEADER)
                .is_some_and(|end| end <= meta.len()))
}

/// Block flags as stored in the 32-byte block header. Every entry carries the
/// flags of the block it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockFlags(pub u32);

impl BlockFlags {
    pub const ZLIB: u32 = 1;
    pub const ENCRYPTED: u32 = 2;
    pub const MEMRES: u32 = 4;
    pub const DELETED: u32 = 8;

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
    pub fn deleted(self) -> bool {
        self.0 & Self::DELETED != 0
    }
}

/// One file header from the chain, after last-wins resolution.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path exactly as stored (UTF-16 decoded). Use [`Entry::key`] to match.
    pub path: Box<str>,
    /// Absolute archive offset, or relative to the decoded section for memres.
    pub offset: u64,
    pub compressed_size: u64,
    pub raw_size: u64,
    /// Unix seconds; 0 for tombstones and island members.
    pub timestamp: u64,
    pub flags: BlockFlags,
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

impl Source {
    fn len(&self) -> Result<u64> {
        Ok(match self {
            Source::File(f) => f.metadata()?.len(),
            Source::Memory(b) => b.len() as u64,
        })
    }

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
/// Opening reads only the 792-byte header, block headers and header tables.
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
    pub fn open(path: impl AsRef<Path>) -> Result<Archive> {
        let path = path.as_ref();
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        Archive::load(Source::File(file), path.display().to_string())
    }

    /// Archive held in memory, e.g. a nested `.a7m` island read out of a
    /// province RDA. `name` only labels errors.
    pub fn from_bytes(bytes: Vec<u8>, name: impl Into<String>) -> Result<Archive> {
        Archive::load(Source::Memory(bytes), name.into())
    }

    /// Path or label given at open time.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    /// Live entries in chain order. Last-wins per key is already applied and
    /// paths tombstoned later in the chain are gone.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Paths this archive deletes (final state per key is a tombstone). They
    /// also hide the path in every archive mounted earlier.
    pub fn tombstones(&self) -> &[Entry] {
        &self.tombstones
    }

    /// Linear scan; the install VFS keeps its own index for the big archives.
    pub fn find(&self, path: &str) -> Option<&Entry> {
        let key = normalize_path(path);
        self.entries.iter().find(|e| e.key() == key)
    }

    /// Decoded payload of `entry`, which must come from this archive.
    /// Up to `len` bytes of `entry` from `start`. Stored entries read only
    /// those bytes; compressed ones inflate whole first.
    pub fn read_range(&self, entry: &Entry, start: u64, len: u64) -> Result<Vec<u8>> {
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
        if flags.encrypted() {
            bail!("{}: encrypted blocks are not supported", ctx());
        }
        if flags.memres() {
            let Some(m) = self.memres.get(entry.memres as usize) else {
                bail!("{}: memres entry without a section", ctx());
            };
            let stored = self
                .source
                .read_at(m.start, m.compressed_size)
                .with_context(ctx)?;
            let section = if m.zlib {
                inflate(&stored, m.raw_size).with_context(ctx)?
            } else {
                stored
            };
            // Bounds checked at open time.
            let start = entry.offset as usize;
            return Ok(section[start..start + entry.raw_size as usize].to_vec());
        }
        let stored = self
            .source
            .read_at(entry.offset, entry.compressed_size)
            .with_context(ctx)?;
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

    /// Moves the entry and tombstone lists out; the install index keeps its
    /// own compact copy and reads through [`Archive::read`] with rebuilt
    /// entries.
    pub(crate) fn take_index(&mut self) -> (Vec<Entry>, Vec<Entry>) {
        (
            std::mem::take(&mut self.entries),
            std::mem::take(&mut self.tombstones),
        )
    }

    pub fn read_path(&self, path: &str) -> Result<Vec<u8>> {
        let Some(entry) = self.find(path) else {
            bail!("{}: no entry {path}", self.name);
        };
        self.read(entry)
    }

    // Walks the chain strictly: the engine silently ends the chain on a bogus
    // pointer, we fail so a broken mod archive is noticed before shipping.
    fn load(source: Source, name: String) -> Result<Archive> {
        let size = source.len()?;
        ensure!(
            size >= HEADER_SIZE,
            "{name}: {size} B is shorter than the RDA header"
        );
        let head = source.read_at(0, HEADER_SIZE)?;
        ensure!(
            head.starts_with(MAGIC),
            "{name}: not an RDA v2.2 archive (magic {:?})",
            String::from_utf8_lossy(&head[..MAGIC.len()])
        );
        let first = u64_at(&head, FIRST_PTR_OFF);

        let mut all = Vec::new();
        let mut memres = Vec::new();
        // Every block costs at least its 32-byte header, so a longer chain
        // must contain a cycle.
        let max_blocks = (size - HEADER_SIZE) / BLOCK_HEADER;
        let mut ptr = first;
        let mut n = 0u64;
        while ptr != size {
            ensure!(
                ptr >= HEADER_SIZE && ptr.checked_add(BLOCK_HEADER).is_some_and(|e| e <= size),
                "{name}: block pointer {ptr:#x} outside the file after {n} block(s)"
            );
            ensure!(n < max_blocks, "{name}: block chain loops at {ptr:#x}");
            let h = source.read_at(ptr, BLOCK_HEADER)?;
            let flags = BlockFlags(u32_at(&h, 0));
            let nfiles = u32_at(&h, 4);
            let cmpfh = u64_at(&h, 8);
            let rawfh = u64_at(&h, 16);
            let next = u64_at(&h, 24);
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
                rawfh == nfiles as u64 * FILE_HEADER as u64,
                "{}: rawFH {rawfh} != nFiles {nfiles} * 560",
                at()
            );
            ensure!(
                next != 0 && next != ptr && next <= size,
                "{}: bogus nextBlockPtr {next:#x}",
                at()
            );
            if nfiles > 0 {
                ensure!(
                    !flags.encrypted() || flags.deleted(),
                    "{}: encrypted block (flags {:#x}); Anno 117 ships none and this reader \
                     does not decrypt",
                    at(),
                    flags.0
                );
                let extra = if flags.memres() { MEMRES_INFO } else { 0 };
                let table_start = ptr.checked_sub(cmpfh + extra).filter(|&s| s >= HEADER_SIZE);
                let Some(table_start) = table_start else {
                    bail!(
                        "{}: header table of {cmpfh} B overlaps the archive header",
                        at()
                    );
                };
                let stored = source.read_at(table_start, cmpfh)?;
                let table = if cmpfh != rawfh {
                    inflate(&stored, rawfh).with_context(|| format!("{}: header table", at()))?
                } else {
                    stored
                };
                let mut section = None;
                if flags.memres() && !flags.deleted() {
                    let info = source.read_at(ptr - MEMRES_INFO, MEMRES_INFO)?;
                    let m = Memres {
                        start: 0,
                        compressed_size: u64_at(&info, 0),
                        raw_size: u64_at(&info, 8),
                        zlib: flags.compressed(),
                    };
                    let start = table_start
                        .checked_sub(m.compressed_size)
                        .filter(|&s| s >= HEADER_SIZE);
                    let Some(start) = start else {
                        bail!("{}: memres section overlaps the archive header", at());
                    };
                    section = Some(memres.len() as u32);
                    memres.push(Memres { start, ..m });
                }
                for (i, rec) in table.as_chunks::<FILE_HEADER>().0.iter().enumerate() {
                    let path = decode_path(&rec[..PATH_BYTES]);
                    ensure!(!path.is_empty(), "{}: entry {i} has an empty path", at());
                    let e = Entry {
                        path,
                        offset: u64_at(rec, PATH_BYTES),
                        compressed_size: u64_at(rec, PATH_BYTES + 8),
                        raw_size: u64_at(rec, PATH_BYTES + 16),
                        timestamp: u64_at(rec, PATH_BYTES + 24),
                        flags,
                        memres: section.unwrap_or(u32::MAX),
                    };
                    if !flags.deleted() {
                        let (len, limit) = match section {
                            Some(s) => (e.raw_size, memres[s as usize].raw_size),
                            None => (e.compressed_size, size),
                        };
                        let in_bounds = e.offset.checked_add(len).is_some_and(|end| end <= limit)
                            && (section.is_some() || len == 0 || e.offset >= HEADER_SIZE);
                        ensure!(
                            in_bounds,
                            "{}: {} data {:#x}+{len} outside {limit:#x}",
                            at(),
                            e.path,
                            e.offset
                        );
                    }
                    all.push(e);
                }
            }
            n += 1;
            ptr = next;
        }

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

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
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
pub enum ZlibMode {
    /// Per file, only when it shrinks (rda_pack.py default).
    IfSmaller,
    /// Every member, like the shipped `.a7m` islands.
    Always,
    Never,
}

#[derive(Debug, Clone)]
pub struct PackOptions {
    pub level: u32,
    pub zlib: ZlibMode,
    /// Uncompressed bytes per block before starting a new one. Only the header
    /// table has to fit u32; 1 GiB keeps blocks a sane size to stream.
    pub max_block_raw: u64,
    pub max_block_files: u32,
    /// Unix seconds stamped on every member.
    pub timestamp: u64,
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
        }
    }
}

impl PackOptions {
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
/// writes a v2.2 archive. Zero-length files are refused: the engine reports
/// them NOT FOUND, so shipping one is always a mistake; delete with
/// [`Packer::tombstone`] instead.
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
        let zlib = match self.opts.zlib {
            ZlibMode::Never => None,
            mode => {
                let z = deflate(data, self.opts.level)?;
                (mode == ZlibMode::Always || z.len() < data.len()).then_some(z)
            }
        };
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
    pub fn tombstone(&mut self, path: &str) -> Result<()> {
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

    /// Serialized archive, for nested `.a7m` islands.
    pub fn to_vec(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.write_to(&mut out)?;
        Ok(out)
    }

    /// Writes atomically: temp file next to `out`, fsync, rename.
    pub fn write(&self, out: &Path) -> Result<()> {
        let dir = match out.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let Some(file_name) = out.file_name() else {
            bail!("{}: not a file path", out.display());
        };
        let tmp = dir.join(format!(
            ".{}.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let result = (|| -> Result<()> {
            let file = File::options()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .with_context(|| format!("create {}", tmp.display()))?;
            let mut w = BufWriter::with_capacity(1 << 20, file);
            self.write_to(&mut w)?;
            let file = w.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            std::fs::rename(&tmp, out)
                .with_context(|| format!("rename {} -> {}", tmp.display(), out.display()))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    pub fn write_to(&self, w: &mut impl Write) -> Result<()> {
        ensure!(
            !self.files.is_empty() || !self.tombstones.is_empty(),
            "nothing to pack"
        );
        // Blocks are uniform (all zlib or all raw), like every shipped block.
        // zlib members go first, then raw ones; input order inside each.
        struct Plan<'a> {
            flags: u32,
            files: Vec<&'a PackedFile>,
            tombstone: Option<&'a str>,
            table: Vec<u8>,
            hdr_off: u64,
        }
        let mut plans: Vec<Plan> = Vec::new();
        for zlib in [true, false] {
            let mut cur: Vec<&PackedFile> = Vec::new();
            let mut raw = 0u64;
            for f in self.files.iter().filter(|f| f.zlib == zlib) {
                let full = cur.len() as u32 >= self.opts.max_block_files.max(1)
                    || raw + f.raw_size > self.opts.max_block_raw;
                if !cur.is_empty() && full {
                    plans.push(plan_block(zlib, std::mem::take(&mut cur)));
                    raw = 0;
                }
                raw += f.raw_size;
                cur.push(f);
            }
            if !cur.is_empty() {
                plans.push(plan_block(zlib, cur));
            }
        }
        fn plan_block(zlib: bool, files: Vec<&PackedFile>) -> Plan<'_> {
            Plan {
                flags: if zlib { BlockFlags::ZLIB } else { 0 },
                files,
                tombstone: None,
                table: Vec::new(),
                hdr_off: 0,
            }
        }
        plans.extend(self.tombstones.iter().map(|t| Plan {
            flags: BlockFlags::DELETED,
            files: Vec::new(),
            tombstone: Some(t),
            table: Vec::new(),
            hdr_off: 0,
        }));

        let mut pos = HEADER_SIZE;
        for p in &mut plans {
            let mut table = Vec::with_capacity(FILE_HEADER * p.files.len().max(1));
            for f in &p.files {
                push_file_header(
                    &mut table,
                    &f.path,
                    [
                        pos,
                        f.stored.len() as u64,
                        f.raw_size,
                        self.opts.timestamp,
                        0,
                    ],
                );
                pos += f.stored.len() as u64;
            }
            if let Some(t) = p.tombstone {
                push_file_header(&mut table, t, [0; 5]);
            }
            p.table = if p.flags & BlockFlags::ZLIB != 0 {
                deflate(&table, self.opts.level)?
            } else {
                table
            };
            let raw_len = (p.files.len() + p.tombstone.is_some() as usize) * FILE_HEADER;
            ensure!(
                raw_len <= u32::MAX as usize,
                "header table of {raw_len} B does not fit u32; lower max_block_files"
            );
            pos += p.table.len() as u64;
            p.hdr_off = pos;
            pos += BLOCK_HEADER;
        }
        let total = pos;

        let mut head = vec![0u8; HEADER_SIZE as usize];
        head[..MAGIC.len()].copy_from_slice(MAGIC);
        head[FIRST_PTR_OFF..].copy_from_slice(&plans[0].hdr_off.to_le_bytes());
        w.write_all(&head)?;
        for (i, p) in plans.iter().enumerate() {
            for f in &p.files {
                w.write_all(&f.stored)?;
            }
            w.write_all(&p.table)?;
            let nfiles = p.files.len() as u32 + p.tombstone.is_some() as u32;
            let next = plans.get(i + 1).map_or(total, |n| n.hdr_off);
            let mut h = [0u8; BLOCK_HEADER as usize];
            h[0..4].copy_from_slice(&p.flags.to_le_bytes());
            h[4..8].copy_from_slice(&nfiles.to_le_bytes());
            h[8..16].copy_from_slice(&(p.table.len() as u64).to_le_bytes());
            h[16..24].copy_from_slice(&(nfiles as u64 * FILE_HEADER as u64).to_le_bytes());
            h[24..32].copy_from_slice(&next.to_le_bytes());
            w.write_all(&h)?;
        }
        w.flush()?;
        Ok(())
    }
}

// [path utf16 520][dataOff][csize][rsize][timestamp][unknown]
fn push_file_header(table: &mut Vec<u8>, path: &str, fields: [u64; 5]) {
    let start = table.len();
    for u in path.encode_utf16() {
        table.extend_from_slice(&u.to_le_bytes());
    }
    table.resize(start + PATH_BYTES, 0);
    for v in fields {
        table.extend_from_slice(&v.to_le_bytes());
    }
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
        "{out}: {units} UTF-16 units, RDA v2.2 room is {MAX_PATH_UNITS}"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn roundtrip_raw_and_zlib() {
        let mut p = Packer::new(PackOptions::default());
        p.add("data/a.xml", &text(10_000)).unwrap();
        p.add("Data\\B.bin", &noise(4096)).unwrap();
        p.add("./data/c.txt", b"x").unwrap();
        p.tombstone("data/gone.txt").unwrap();
        let bytes = p.to_vec().unwrap();
        let a = Archive::from_bytes(bytes, "mem").unwrap();

        assert_eq!(a.entries().len(), 3);
        let e = a.find("DATA/A.XML").unwrap();
        assert!(e.flags.compressed());
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

    #[test]
    fn block_caps_split_blocks() {
        let opts = PackOptions {
            max_block_raw: 3000,
            zlib: ZlibMode::Never,
            ..PackOptions::default()
        };
        let mut p = Packer::new(opts);
        for i in 0..5 {
            p.add(&format!("f{i}"), &text(1000 + i)).unwrap();
        }
        let a = Archive::from_bytes(p.to_vec().unwrap(), "mem").unwrap();
        for i in 0..5 {
            assert_eq!(a.read_path(&format!("F{i}")).unwrap(), text(1000 + i));
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
        assert!(p.add(&"a".repeat(260), b"1").is_err());
        assert!(p.add(&"a".repeat(259), b"1").is_ok());
        assert!(Packer::new(PackOptions::default()).to_vec().is_err());
    }

    // Hand-built chain: two live blocks repeating a path, then a tombstone
    // block, then a block that brings a tombstoned path back.
    #[test]
    fn last_wins_and_tombstones_within_one_chain() {
        let mut out = vec![0u8; HEADER_SIZE as usize];
        out[..18].copy_from_slice(MAGIC);
        let mut hdrs = Vec::new();
        let mut block = |out: &mut Vec<u8>, flags: u32, files: &[(&str, &[u8])]| {
            let mut table = Vec::new();
            for (name, data) in files {
                let off = out.len() as u64;
                out.extend_from_slice(data);
                let n = data.len() as u64;
                let fields = if flags & 8 != 0 {
                    [0; 5]
                } else {
                    [off, n, n, 0, 0]
                };
                push_file_header(&mut table, name, fields);
            }
            out.extend_from_slice(&table);
            hdrs.push(out.len());
            let mut h = Vec::new();
            h.extend_from_slice(&flags.to_le_bytes());
            h.extend_from_slice(&(files.len() as u32).to_le_bytes());
            h.extend_from_slice(&(table.len() as u64).to_le_bytes());
            h.extend_from_slice(&(table.len() as u64).to_le_bytes());
            h.extend_from_slice(&0u64.to_le_bytes()); // patched below
            out.extend_from_slice(&h);
        };
        block(&mut out, 0, &[]); // graphics_ui.rda opens with an empty block
        block(&mut out, 0, &[("a", b"old"), ("b", b"bee"), ("c", b"sea")]);
        block(&mut out, 0, &[("A", b"new")]);
        block(&mut out, 8, &[("b", b""), ("c", b"")]);
        block(&mut out, 0, &[("c", b"back")]);
        let total = out.len() as u64;
        out[FIRST_PTR_OFF..FIRST_PTR_OFF + 8].copy_from_slice(&(hdrs[0] as u64).to_le_bytes());
        for i in 0..hdrs.len() {
            let next = hdrs.get(i + 1).map_or(total, |&h| h as u64);
            out[hdrs[i] + 24..hdrs[i] + 32].copy_from_slice(&next.to_le_bytes());
        }

        let a = Archive::from_bytes(out.clone(), "chain").unwrap();
        assert_eq!(a.read_path("a").unwrap(), b"new");
        assert!(a.find("b").is_none());
        assert_eq!(a.read_path("c").unwrap(), b"back");
        let tombs: Vec<_> = a.tombstones().iter().map(|e| &*e.path).collect();
        assert_eq!(tombs, ["b"]);

        // A pointer past EOF must fail, not end the chain quietly.
        let mut bad = out.clone();
        bad[hdrs[1] + 24..hdrs[1] + 32].copy_from_slice(&(total + 1).to_le_bytes());
        assert!(Archive::from_bytes(bad, "bad").is_err());
        // So must a cycle.
        let mut cyc = out;
        cyc[hdrs[2] + 24..hdrs[2] + 32].copy_from_slice(&(hdrs[1] as u64).to_le_bytes());
        assert!(Archive::from_bytes(cyc, "cycle").is_err());
    }

    #[test]
    fn nested_island_shape_from_bytes() {
        let mut p = Packer::new(PackOptions::island());
        p.add("gamedata.data", &text(50_000)).unwrap();
        p.add("rd3d.data", &noise(1000)).unwrap(); // forced zlib even if it grows
        let a = Archive::from_bytes(p.to_vec().unwrap(), "island.a7m").unwrap();
        let names: Vec<_> = a.entries().iter().map(|e| &*e.path).collect();
        assert_eq!(names, ["gamedata.data", "rd3d.data"]);
        assert!(
            a.entries()
                .iter()
                .all(|e| e.flags.compressed() && e.timestamp == 0)
        );
        assert_eq!(a.read_path("rd3d.data").unwrap(), noise(1000));
    }

    #[test]
    fn zlib_members_carry_the_header_the_engine_accepts() {
        // The engine fails any read whose stream does not start `78 01`;
        // standard encoders write `78 9c` at default level.
        let mut p = Packer::new(PackOptions::island());
        let data = text(200_000);
        p.add("gamedata.data", &data).unwrap();
        let bytes = p.to_vec().unwrap();
        let a = Archive::from_bytes(bytes.clone(), "island.a7m").unwrap();
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
        assert!(Archive::from_bytes(b"Resource File V2.2".to_vec(), "short").is_err());
        assert!(Archive::from_bytes(vec![0; 900], "zero").is_err());
        let mut p = Packer::new(PackOptions::default());
        p.add("x", &text(5000)).unwrap();
        let mut bytes = p.to_vec().unwrap();
        bytes.truncate(bytes.len() - 1);
        assert!(Archive::from_bytes(bytes, "cut").is_err());
    }

    #[test]
    fn atomic_write_and_python_verify() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("zz_patchfiles_999_test.rda");
        let mut p = Packer::new(PackOptions::default());
        p.add("data/ui/a.json", &text(20_000)).unwrap();
        p.add("data/ui/b.dds", &noise(3000)).unwrap();
        p.tombstone("data/ui/old.json").unwrap();
        p.write(&out).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(names.len(), 1, "temp file left behind");
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
        std::env::var_os("ANNO117_DIR").map(PathBuf::from)
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
        let orig = Archive::from_bytes(bytes, src).unwrap();
        let mut p = Packer::new(PackOptions::island());
        let mut members = Vec::new();
        for e in orig.entries() {
            let data = orig.read(e).unwrap();
            p.add(&e.path, &data).unwrap();
            members.push((e.path.clone(), data));
        }
        assert_eq!(members.len(), 2);
        let again = Archive::from_bytes(p.to_vec().unwrap(), "repacked").unwrap();
        assert_eq!(again.entries().len(), 2);
        for (path, data) in &members {
            let e = again.find(path).unwrap();
            assert!(e.flags.compressed());
            assert_eq!(&again.read(e).unwrap(), data, "{path}");
        }
    }
}
