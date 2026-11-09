// ADF record trees: `.a7minfo`, the `gamedata.data` / `rd3d.data` members of
// `.a7m` islands and the install's archive map. Decodes canonical (`-3`/`-2`
// trailer) and legacy packed files into an editable tree, keeping their wire format.
// Records only hold ids and payload spans into one byte arena. Names live in
// the per-file dictionaries, so a 90 MB `file_h.db` with ~600k nodes decodes
// without a String per record.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};

/// Handle to a node or value inside one [`Adf`]. Handles stay valid across
/// edits and clones of the same tree; removed items simply become unreachable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ItemId(u32);

/// Record ids at and above this mark value leaves; below are node openers.
pub const VALUE_ID_BASE: u16 = 0x8000;

// Unnamed nodes and values use the first id of each space (never listed in
// the dictionaries), e.g. `Instances` entries are id 1 and `FileNames`
// entries id 0x8000. The engine seeds its allocators at the same values.
const UNNAMED_NODE_ID: u16 = 1;
const UNNAMED_VALUE_ID: u16 = VALUE_ID_BASE;

const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trailer {
    // [u32 w0][u32 items][u32 dictAOff][u32 dictBOff][u32 w5][i32 -3]
    V3 { w0: u32, w5: u32 },
    // [u32 dictAOff][u32 dictBOff][u32 word][i32 -2]. 1800 stores 8,
    // not an item count, in `word`; its meaning is not established.
    V2 { word: u32 },
    // Packed u16-id records and varint dictionaries, then [u32 dictAOff].
    Packed,
}

fn read_varint(data: &[u8], pos: &mut usize) -> Result<u32> {
    let mut value = 0;
    for shift in (0..35).step_by(7) {
        let byte = *data.get(*pos).context("ADF: truncated varint")?;
        *pos += 1;
        ensure!(shift < 28 || byte <= 15, "ADF: varint overflows u32");
        value |= u32::from(byte & 127) << shift;
        if byte & 128 == 0 {
            ensure!(shift == 0 || byte != 0, "ADF: noncanonical varint");
            return Ok(value);
        }
    }
    unreachable!("the fifth byte must fit four bits")
}

fn write_varint(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        out.push(byte | if value != 0 { 128 } else { 0 });
        if value == 0 {
            break;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Item {
    // Payload span in `Adf::data`.
    off: u32,
    len: u32,
    // Close record payload span; `close_off == NONE` means the stream ended
    // before this node was closed.
    close_off: u32,
    close_len: u32,
    first: u32,
    last: u32,
    next: u32,
    id: u16,
}

impl Item {
    fn new(id: u16, off: u32, len: u32) -> Item {
        Item {
            off,
            len,
            close_off: NONE,
            close_len: 0,
            first: NONE,
            last: NONE,
            next: NONE,
            id,
        }
    }
}

// Format A dictionary: [u32 count][count x u16 id][NUL-terminated names].
#[derive(Debug, Clone, Default)]
struct Dict {
    entries: Vec<(u16, Box<[u8]>)>,
    by_id: HashMap<u16, u32>,
    by_name: HashMap<Box<[u8]>, u16>,
    // Zero fill after the names. Every shipped file aligns the dictionary end
    // to 8; the decoded value is kept until the dictionary grows.
    pad: Option<usize>,
}

impl Dict {
    fn parse_packed(data: &[u8], pos: &mut usize) -> Result<Dict> {
        let count = read_varint(data, pos)? as usize;
        ensure!(
            count <= (data.len() - *pos) / 3,
            "ADF: packed dictionary count overruns"
        );
        let mut dict = Dict::default();
        for _ in 0..count {
            let len = data[*pos..]
                .iter()
                .position(|b| *b == 0)
                .context("ADF: unterminated packed dictionary name")?;
            let end = *pos + len;
            ensure!(end + 3 <= data.len(), "ADF: truncated packed dictionary id");
            let id = u16::from_le_bytes(data[end + 1..end + 3].try_into().unwrap());
            dict.push(id, &data[*pos..end]);
            *pos = end + 3;
        }
        Ok(dict)
    }

    fn write_packed(&self, out: &mut Vec<u8>) {
        write_varint(out, self.entries.len() as u32);
        for (id, name) in &self.entries {
            out.extend_from_slice(name);
            out.push(0);
            out.extend_from_slice(&id.to_le_bytes());
        }
    }

    fn parse(data: &[u8], off: usize, end: usize) -> Result<Dict> {
        ensure!(off + 4 <= end, "dictionary at {off:#x} truncated");
        let count = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        ensure!(
            off + 4 + count * 2 <= end,
            "dictionary at {off:#x}: count {count} overruns"
        );
        let mut dict = Dict::default();
        let mut p = off + 4 + count * 2;
        for i in 0..count {
            let id_at = off + 4 + i * 2;
            let id = u16::from_le_bytes([data[id_at], data[id_at + 1]]);
            let nul = data[p..end]
                .iter()
                .position(|&b| b == 0)
                .with_context(|| format!("dictionary at {off:#x}: unterminated name {i}"))?;
            dict.push(id, &data[p..p + nul]);
            p += nul + 1;
        }
        let pad = end - p;
        ensure!(
            data[p..end].iter().all(|&b| b == 0),
            "dictionary at {off:#x}: non-zero fill"
        );
        dict.pad = Some(pad);
        Ok(dict)
    }

    fn push(&mut self, id: u16, name: &[u8]) {
        let index = self.entries.len() as u32;
        self.by_id.entry(id).or_insert(index);
        self.by_name.entry(name.into()).or_insert(id);
        self.entries.push((id, name.into()));
    }

    fn name(&self, id: u16) -> Option<&[u8]> {
        self.by_id.get(&id).map(|&i| &*self.entries[i as usize].1)
    }

    fn write(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (id, _) in &self.entries {
            out.extend_from_slice(&id.to_le_bytes());
        }
        for (_, name) in &self.entries {
            out.extend_from_slice(name);
            out.push(0);
        }
        let pad = self
            .pad
            .unwrap_or_else(|| (8 - (out.len() - start) % 8) % 8);
        out.resize(out.len() + pad, 0);
    }
}

/// One decoded ADF file. The implicit root node is [`Adf::root`]; its
/// children are the top-level records.
#[derive(Debug, Clone)]
pub struct Adf {
    // Payload arena: the original record stream, then bytes appended by edits.
    data: Vec<u8>,
    items: Vec<Item>,
    nodes: Dict,
    values: Dict,
    trailer: Trailer,
    max_node_id: u16,
    max_value_id: u16,
}

impl Adf {
    /// Decodes canonical `-3`/`-2` streams and 1800's older packed streams.
    pub fn decode(bytes: &[u8]) -> Result<Adf> {
        let size = bytes.len();
        ensure!(size < u32::MAX as usize, "ADF: file exceeds 4 GiB");
        ensure!(size >= 6, "ADF: {size} bytes is too small");
        let word = |off: usize| u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        let marker = word(size - 4) as i32;
        let (trailer, trailer_len, dict_a, dict_b) = match marker {
            -3 => {
                ensure!(
                    size >= 24,
                    "ADF: {size} bytes is too small for a -3 trailer"
                );
                let t = Trailer::V3 {
                    w0: word(size - 24),
                    w5: word(size - 8),
                };
                (t, 24, word(size - 16) as usize, word(size - 12) as usize)
            }
            -2 => {
                ensure!(size >= 16, "ADF: file too short for a -2 trailer");
                (
                    Trailer::V2 {
                        word: word(size - 8),
                    },
                    16,
                    word(size - 16) as usize,
                    word(size - 12) as usize,
                )
            }
            m if (m as u32 as usize) <= size - 4 => {
                (Trailer::Packed, 4, m as u32 as usize, size - 4)
            }
            m => bail!("ADF: unknown trailer marker {m:#x}"),
        };
        let dict_end = size - trailer_len;
        ensure!(
            (matches!(trailer, Trailer::Packed) || dict_a % 8 == 0)
                && dict_a <= dict_b
                && dict_b <= dict_end,
            "ADF: bad dictionary offsets {dict_a:#x}/{dict_b:#x} (size {size:#x})"
        );
        let (nodes, values) = if matches!(trailer, Trailer::Packed) {
            let mut pos = dict_a;
            let nodes = Dict::parse_packed(&bytes[..dict_end], &mut pos)?;
            let values = Dict::parse_packed(&bytes[..dict_end], &mut pos)?;
            ensure!(
                pos == dict_end,
                "ADF: bytes remain after packed dictionaries"
            );
            (nodes, values)
        } else {
            (
                Dict::parse(bytes, dict_a, dict_b)?,
                Dict::parse(bytes, dict_b, dict_end)?,
            )
        };

        let mut adf = Adf {
            data: bytes[..dict_a].to_vec(),
            items: Vec::new(),
            nodes,
            values,
            trailer,
            max_node_id: UNNAMED_NODE_ID,
            max_value_id: UNNAMED_VALUE_ID,
        };
        adf.items.push(Item::new(0, 0, 0));
        adf.parse_records()?;
        for &(id, _) in &adf.nodes.entries {
            adf.max_node_id = adf.max_node_id.max(id);
        }
        for &(id, _) in &adf.values.entries {
            adf.max_value_id = adf.max_value_id.max(id);
        }
        Ok(adf)
    }

    fn parse_records(&mut self) -> Result<()> {
        let end = self.data.len();
        let mut stack: Vec<u32> = vec![0];
        let mut pos = 0usize;
        while pos < end {
            let (id, off, len, next) = if matches!(self.trailer, Trailer::Packed) {
                ensure!(pos + 2 <= end, "ADF: truncated packed record id");
                let id = u16::from_le_bytes(self.data[pos..pos + 2].try_into().unwrap());
                let mut payload = pos + 2;
                let len = if id >= VALUE_ID_BASE {
                    read_varint(&self.data, &mut payload)?
                } else {
                    0
                };
                let next = payload + len as usize;
                ensure!(next <= end, "ADF: packed record at {pos:#x} overruns");
                (id, payload as u32, len, next)
            } else {
                ensure!(pos + 8 <= end, "ADF: record header at {pos:#x} overruns");
                let size = u32::from_le_bytes(self.data[pos..pos + 4].try_into().unwrap()) as usize;
                let tag = u32::from_le_bytes(self.data[pos + 4..pos + 8].try_into().unwrap());
                ensure!(
                    tag >> 16 == 0,
                    "ADF: record at {pos:#x} has reserved id bits {tag:#x}"
                );
                let padded = (size + 7) & !7;
                ensure!(
                    pos + 8 + padded <= end,
                    "ADF: record at {pos:#x} (size {size}) overruns the stream"
                );
                ensure!(
                    self.data[pos + 8 + size..pos + 8 + padded]
                        .iter()
                        .all(|&b| b == 0),
                    "ADF: record at {pos:#x} has non-zero padding"
                );
                (tag as u16, (pos + 8) as u32, size as u32, pos + 8 + padded)
            };
            let Some(&cur) = stack.last() else {
                bail!("ADF: record at {pos:#x} follows the root close");
            };
            if id == 0 {
                let item = &mut self.items[cur as usize];
                item.close_off = off;
                item.close_len = len;
                stack.pop();
            } else {
                let new = self.push_item(cur, Item::new(id, off, len));
                if id < VALUE_ID_BASE {
                    self.max_node_id = self.max_node_id.max(id);
                    stack.push(new.0);
                } else {
                    self.max_value_id = self.max_value_id.max(id);
                }
            }
            pos = next;
        }
        Ok(())
    }

    fn push_item(&mut self, parent: u32, item: Item) -> ItemId {
        let index = self.items.len() as u32;
        assert!(index != NONE, "ADF: item count overflow");
        self.items.push(item);
        let p = &mut self.items[parent as usize];
        let last = p.last;
        p.last = index;
        if last == NONE {
            p.first = index;
        } else {
            self.items[last as usize].next = index;
        }
        ItemId(index)
    }

    /// Encodes the tree. Unedited trees come back byte-identical; the
    /// v3 trailer's item count is recomputed; other trailer words are retained.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + 4096);
        let mut count = 0u32;
        // Iterative walk; `true` entries emit the close of a finished node.
        let mut stack: Vec<(u32, bool)> = Vec::new();
        let root = &self.items[0];
        if root.close_off != NONE {
            stack.push((0, true));
        }
        self.push_children_rev(0, &mut stack);
        while let Some((index, closing)) = stack.pop() {
            let item = &self.items[index as usize];
            if closing {
                self.write_record(&mut out, 0, item.close_off, item.close_len);
                continue;
            }
            count += 1;
            self.write_record(&mut out, item.id, item.off, item.len);
            if item.id < VALUE_ID_BASE {
                if item.close_off != NONE {
                    stack.push((index, true));
                }
                self.push_children_rev(index, &mut stack);
            }
        }
        let dict_a = out.len() as u32;
        if matches!(self.trailer, Trailer::Packed) {
            self.nodes.write_packed(&mut out);
        } else {
            self.nodes.write(&mut out);
        }
        let dict_b = out.len() as u32;
        if matches!(self.trailer, Trailer::Packed) {
            self.values.write_packed(&mut out);
        } else {
            self.values.write(&mut out);
        }
        let items = count + 1;
        let mut put = |v: u32| out.extend_from_slice(&v.to_le_bytes());
        match self.trailer {
            Trailer::V3 { w0, w5 } => {
                put(w0);
                put(items);
                put(dict_a);
                put(dict_b);
                put(w5);
                put(-3i32 as u32);
            }
            Trailer::V2 { word } => {
                put(dict_a);
                put(dict_b);
                put(word);
                put(-2i32 as u32);
            }
            Trailer::Packed => put(dict_a),
        }
        out
    }

    fn push_children_rev(&self, parent: u32, stack: &mut Vec<(u32, bool)>) {
        let start = stack.len();
        let mut c = self.items[parent as usize].first;
        while c != NONE {
            stack.push((c, false));
            c = self.items[c as usize].next;
        }
        stack[start..].reverse();
    }

    fn write_record(&self, out: &mut Vec<u8>, id: u16, off: u32, len: u32) {
        if matches!(self.trailer, Trailer::Packed) {
            out.extend_from_slice(&id.to_le_bytes());
            if id >= VALUE_ID_BASE {
                write_varint(out, len);
                out.extend_from_slice(&self.data[off as usize..off as usize + len as usize]);
            } else {
                assert!(len == 0, "packed ADF nodes cannot carry payloads");
            }
            return;
        }
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&u32::from(id).to_le_bytes());
        let (off, len) = (off as usize, len as usize);
        out.extend_from_slice(&self.data[off..off + len]);
        out.resize(out.len() + ((8 - len % 8) % 8), 0);
    }

    pub fn root(&self) -> ItemId {
        ItemId(0)
    }

    /// New terrain files must use the selected game's ADF generation.
    pub(crate) fn encode_like(&self, template: &Adf) -> Vec<u8> {
        let mut tree = self.clone();
        tree.trailer = template.trailer;
        tree.encode()
    }

    pub fn children(&self, node: ItemId) -> impl Iterator<Item = ItemId> + '_ {
        let mut c = self.items[node.0 as usize].first;
        std::iter::from_fn(move || {
            (c != NONE).then(|| {
                let id = ItemId(c);
                c = self.items[c as usize].next;
                id
            })
        })
    }

    /// Raw record id: below [`VALUE_ID_BASE`] for nodes, at or above for values.
    pub fn record_id(&self, item: ItemId) -> u16 {
        self.items[item.0 as usize].id
    }

    pub fn is_node(&self, item: ItemId) -> bool {
        self.record_id(item) < VALUE_ID_BASE
    }

    /// Field name from the file's dictionaries; `None` for unnamed records
    /// and the root.
    pub fn name(&self, item: ItemId) -> Option<&str> {
        let id = self.record_id(item);
        let dict = if id < VALUE_ID_BASE {
            &self.nodes
        } else {
            &self.values
        };
        dict.name(id).and_then(|n| std::str::from_utf8(n).ok())
    }

    fn lookup_id(&self, name: &str, value: bool) -> Option<u16> {
        let dict = if value { &self.values } else { &self.nodes };
        dict.by_name.get(name.as_bytes()).copied()
    }

    fn find_child(&self, parent: ItemId, id: u16) -> Option<ItemId> {
        self.children(parent).find(|&c| self.record_id(c) == id)
    }

    /// First child node with this name.
    pub fn child_node(&self, parent: ItemId, name: &str) -> Option<ItemId> {
        self.find_child(parent, self.lookup_id(name, false)?)
    }

    /// First child value with this name.
    pub fn child_value(&self, parent: ItemId, name: &str) -> Option<ItemId> {
        self.find_child(parent, self.lookup_id(name, true)?)
    }

    /// All children (nodes or values) sharing `name`, in file order.
    pub fn children_named<'a>(
        &'a self,
        parent: ItemId,
        name: &str,
        value: bool,
    ) -> impl Iterator<Item = ItemId> + 'a {
        let id = self.lookup_id(name, value);
        self.children(parent)
            .filter(move |&c| Some(self.record_id(c)) == id)
    }

    /// Follows a `/`-separated path of node names from `from`.
    pub fn node(&self, from: ItemId, path: &str) -> Option<ItemId> {
        path.split('/')
            .filter(|s| !s.is_empty())
            .try_fold(from, |at, seg| self.child_node(at, seg))
    }

    /// Like [`Adf::node`], but the last segment names a value.
    pub fn value(&self, from: ItemId, path: &str) -> Option<ItemId> {
        let (nodes, leaf) = path.rsplit_once('/').unwrap_or(("", path));
        self.child_value(self.node(from, nodes)?, leaf)
    }

    pub fn payload(&self, item: ItemId) -> &[u8] {
        let it = &self.items[item.0 as usize];
        &self.data[it.off as usize..(it.off + it.len) as usize]
    }

    fn fixed<const N: usize>(&self, item: ItemId) -> Result<[u8; N]> {
        let p = self.payload(item);
        p.try_into().with_context(|| {
            format!(
                "ADF field {}: payload is {} bytes, expected {N}",
                self.name(item).unwrap_or("<unnamed>"),
                p.len()
            )
        })
    }

    pub fn read_u8(&self, item: ItemId) -> Result<u8> {
        Ok(self.fixed::<1>(item)?[0])
    }

    pub fn read_u16(&self, item: ItemId) -> Result<u16> {
        Ok(u16::from_le_bytes(self.fixed(item)?))
    }

    pub fn read_u32(&self, item: ItemId) -> Result<u32> {
        Ok(u32::from_le_bytes(self.fixed(item)?))
    }

    pub fn read_f32(&self, item: ItemId) -> Result<f32> {
        Ok(f32::from_le_bytes(self.fixed(item)?))
    }

    pub fn read_u64(&self, item: ItemId) -> Result<u64> {
        Ok(u64::from_le_bytes(self.fixed(item)?))
    }

    /// Fixed-width f32 vector (Position = 3, Rotation/Color = 4, ...).
    pub fn read_f32s<const N: usize>(&self, item: ItemId) -> Result<[f32; N]> {
        let p = self.payload(item);
        ensure!(
            p.len() == N * 4,
            "ADF field {}: payload is {} bytes, expected {}",
            self.name(item).unwrap_or("<unnamed>"),
            p.len(),
            N * 4
        );
        Ok(std::array::from_fn(|i| {
            f32::from_le_bytes(p[i * 4..i * 4 + 4].try_into().unwrap())
        }))
    }

    /// UTF-16LE text payload (paths). Strings carry no NUL on disk.
    pub fn read_utf16(&self, item: ItemId) -> Result<String> {
        let p = self.payload(item);
        ensure!(p.len().is_multiple_of(2), "ADF: odd-length UTF-16 payload");
        let units: Vec<u16> = p
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        String::from_utf16(&units).context("ADF: invalid UTF-16 payload")
    }

    /// Replaces a payload. Same-length writes overwrite in place, so
    /// repeatedly saving large maps does not grow the arena.
    pub fn set_payload(&mut self, item: ItemId, bytes: &[u8]) {
        let it = self.items[item.0 as usize];
        if it.len as usize == bytes.len() {
            let off = it.off as usize;
            self.data[off..off + bytes.len()].copy_from_slice(bytes);
            return;
        }
        let off = self.append(bytes);
        let it = &mut self.items[item.0 as usize];
        it.off = off;
        it.len = bytes.len() as u32;
    }

    fn append(&mut self, bytes: &[u8]) -> u32 {
        let off = self.data.len();
        assert!(
            off + bytes.len() < NONE as usize,
            "ADF: payload arena exceeds 4 GiB"
        );
        self.data.extend_from_slice(bytes);
        off as u32
    }

    // Engine writer semantics: reuse the name's id, else take the next free
    // id in the space and append the binding to the dictionary.
    fn intern(&mut self, name: Option<&str>, value: bool) -> Result<u16> {
        let Some(name) = name else {
            let (id, dict) = if value {
                (UNNAMED_VALUE_ID, &self.values)
            } else {
                (UNNAMED_NODE_ID, &self.nodes)
            };
            ensure!(
                dict.name(id).is_none(),
                "ADF: unnamed id {id:#x} is bound to a name in this file"
            );
            return Ok(id);
        };
        ensure!(
            !name.is_empty() && !name.contains('\0'),
            "ADF: invalid field name {name:?}"
        );
        if let Some(id) = self.lookup_id(name, value) {
            return Ok(id);
        }
        let (max, limit, dict) = if value {
            (&mut self.max_value_id, u16::MAX, &mut self.values)
        } else {
            (&mut self.max_node_id, VALUE_ID_BASE - 1, &mut self.nodes)
        };
        ensure!(*max < limit, "ADF: id space exhausted adding {name:?}");
        *max += 1;
        dict.push(*max, name.as_bytes());
        dict.pad = None;
        Ok(*max)
    }

    /// Appends a value leaf to `parent`. `None` adds an unnamed entry (array
    /// element), as in `FileNames`.
    pub fn add_value(
        &mut self,
        parent: ItemId,
        name: Option<&str>,
        payload: &[u8],
    ) -> Result<ItemId> {
        ensure!(
            self.is_node(parent),
            "ADF: values can only be added to nodes"
        );
        let id = self.intern(name, true)?;
        let off = self.append(payload);
        Ok(self.push_item(parent.0, Item::new(id, off, payload.len() as u32)))
    }

    /// Appends an empty node (opener + close) to `parent`.
    pub fn add_node(&mut self, parent: ItemId, name: Option<&str>) -> Result<ItemId> {
        ensure!(
            self.is_node(parent),
            "ADF: nodes can only be added to nodes"
        );
        let id = self.intern(name, false)?;
        let mut item = Item::new(id, 0, 0);
        item.close_off = 0;
        Ok(self.push_item(parent.0, item))
    }

    /// Unlinks `item` from `parent`. Returns false when it is not a child.
    pub fn remove(&mut self, parent: ItemId, item: ItemId) -> bool {
        let mut prev = NONE;
        let mut c = self.items[parent.0 as usize].first;
        while c != NONE && c != item.0 {
            prev = c;
            c = self.items[c as usize].next;
        }
        if c == NONE {
            return false;
        }
        let next = self.items[c as usize].next;
        if prev == NONE {
            self.items[parent.0 as usize].first = next;
        } else {
            self.items[prev as usize].next = next;
        }
        if self.items[parent.0 as usize].last == c {
            self.items[parent.0 as usize].last = prev;
        }
        true
    }

    /// Drops every child of `node`, keeping the node itself.
    pub fn clear_children(&mut self, node: ItemId) {
        let it = &mut self.items[node.0 as usize];
        it.first = NONE;
        it.last = NONE;
    }
}

/// FNV-1a 32-bit, the engine's field-name hash (matching is hash-only).
pub fn fnv1a32(name: &[u8]) -> u32 {
    name.iter().fold(0x811C_9DC5u32, |h, &b| {
        // The engine hashes signed chars; bytes >= 0x80 sign-extend.
        (h ^ (b as i8 as i32 as u32)).wrapping_mul(0x0100_0193)
    })
}

/// UTF-16LE payload bytes for a path or name, without terminator.
pub fn utf16_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    fn extract_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extract")
    }

    fn files_with_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                files_with_ext(&p, ext, out);
            } else if p.extension().is_some_and(|e| e == ext) {
                out.push(p);
            }
        }
    }

    fn roundtrip(bytes: &[u8]) -> Adf {
        let adf = Adf::decode(bytes).unwrap();
        let out = adf.encode();
        assert_eq!(out.len(), bytes.len());
        assert!(out == bytes, "re-encode differs");
        adf
    }

    #[test]
    fn anno1800_v2_trailer_word_is_preserved() {
        let mut bytes = Vec::new();
        for word in [4u32, 0x8001, 42, 0, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&0x8001u16.to_le_bytes());
        bytes.extend_from_slice(b"Value\0\0\0\0\0");
        for word in [16u32, 24, 8, -2i32 as u32] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        roundtrip(&bytes);
    }

    #[test]
    fn anno1800_packed_records_and_dictionaries_roundtrip() {
        // Hand-written legacy file: Root { Value=42 }, root close, two
        // interleaved name/id dictionaries, then the dictionary offset.
        let bytes =
            b"\x02\0\x01\x80\x04\x2a\0\0\0\0\0\0\0\x01Root\0\x02\0\x01Value\0\x01\x80\x0d\0\0\0";
        let mut adf = roundtrip(bytes);
        let item = adf.value(adf.root(), "Root/Value").unwrap();
        assert_eq!(adf.read_u32(item).unwrap(), 42);
        adf.set_payload(item, &99u32.to_le_bytes());
        let edited = adf.encode();
        assert_eq!(&edited[..5], &bytes[..5]);
        assert_eq!(&edited[9..], &bytes[9..]);
        let back = Adf::decode(&edited).unwrap();
        assert_eq!(
            back.read_u32(back.value(back.root(), "Root/Value").unwrap())
                .unwrap(),
            99
        );
        adf.set_payload(item, &[7; 140]);
        let root = adf.node(adf.root(), "Root").unwrap();
        adf.add_value(root, Some("Extra"), b"abc").unwrap();
        let back = Adf::decode(&adf.encode()).unwrap();
        assert_eq!(
            back.payload(back.value(back.root(), "Root/Value").unwrap()),
            &[7; 140]
        );
        assert_eq!(
            back.payload(back.value(back.root(), "Root/Extra").unwrap()),
            b"abc"
        );
    }

    #[test]
    fn empty_packed_file_and_truncated_trailers() {
        roundtrip(&[0; 6]);
        for len in 0..6 {
            assert!(Adf::decode(&vec![0; len]).is_err());
        }
        let mut invalid = [0u8; 8];
        for marker in [-3i32, -2] {
            invalid[4..].copy_from_slice(&marker.to_le_bytes());
            assert!(Adf::decode(&invalid).is_err());
        }
    }

    #[test]
    fn installed_1800_island_adf_roundtrips() {
        let Some(dir) = std::env::var_os("ANNO1800_DIR") else {
            return;
        };
        let install = crate::vfs::Install::open(dir).unwrap();
        let mut checked = 0;
        for id in install.search(".a7m", 100_000) {
            let path = install.file_path(id);
            let bytes = install.read_file(id).unwrap();
            let members = if path.ends_with(".a7m") {
                let archive = crate::rda::Archive::from_bytes(bytes, path).unwrap();
                vec![
                    ("gamedata.data", archive.read_path("gamedata.data").unwrap()),
                    ("rd3d.data", archive.read_path("rd3d.data").unwrap()),
                ]
            } else if path.ends_with(".a7minfo") {
                vec![("info", bytes)]
            } else {
                continue;
            };
            for (member, bytes) in members {
                let adf = Adf::decode(&bytes).unwrap_or_else(|e| panic!("{path}/{member}: {e:#}"));
                assert_eq!(adf.encode(), bytes, "{path}/{member}");
                checked += 1;
            }
        }
        eprintln!("1800 island serialization: {checked} exact roundtrips");
        assert!(checked > 500);
    }

    #[test]
    fn roundtrips_every_a7minfo() {
        let mut files = Vec::new();
        files_with_ext(
            &extract_dir().join("provinces_roman"),
            "a7minfo",
            &mut files,
        );
        if files.is_empty() {
            eprintln!("skip: no extracted .a7minfo files");
            return;
        }
        for f in &files {
            let adf = roundtrip(&std::fs::read(f).unwrap());
            let map_size = adf.value(adf.root(), "MapSize").unwrap();
            let size = adf.read_u64(map_size).unwrap();
            assert!(size as u32 > 0, "{}", f.display());
        }
        assert!(files.len() >= 38, "found {} .a7minfo", files.len());
    }

    #[test]
    fn roundtrips_island_members() {
        let dir = extract_dir().join("a7m_test");
        for name in ["gamedata.data", "rd3d.data"] {
            let Ok(bytes) = std::fs::read(dir.join(name)) else {
                eprintln!("skip: no {name}");
                return;
            };
            roundtrip(&bytes);
        }
    }

    #[test]
    fn reads_known_fields() {
        let Ok(bytes) = std::fs::read(extract_dir().join("a7m_test/rd3d.data")) else {
            return;
        };
        let adf = Adf::decode(&bytes).unwrap();
        let t = adf.child_node(adf.root(), "Terrain").unwrap();
        assert_eq!(
            adf.read_u32(adf.child_value(t, "GridWidth").unwrap())
                .unwrap(),
            10240
        );
        assert_eq!(
            adf.read_f32(adf.child_value(t, "UnitScale").unwrap())
                .unwrap(),
            0.03125
        );
        assert_eq!(
            adf.read_u8(adf.child_value(t, "UsingSplitImageHeightMaps").unwrap())
                .unwrap(),
            1
        );
        let set = adf
            .read_utf16(adf.child_value(t, "MaterialSetFileName").unwrap())
            .unwrap();
        assert!(
            set.ends_with("roman_materialset_02_3rdparty_pirate.xml"),
            "{set}"
        );
        let w = adf.value(t, "CoarseHeightMap/width").unwrap();
        assert_eq!(adf.read_u32(w).unwrap(), 641);
        assert!(adf.read_u16(w).is_err(), "width mismatch must fail");
        let files = adf.node(adf.root(), "PropGrid/FileNames").unwrap();
        let first = adf.children(files).next().unwrap();
        assert_eq!(adf.name(first), None);
        assert!(adf.read_utf16(first).unwrap().ends_with(".cfg"));
    }

    #[test]
    fn edits_read_back() {
        let mut files = Vec::new();
        files_with_ext(
            &extract_dir().join("provinces_roman"),
            "a7minfo",
            &mut files,
        );
        let Some(f) = files.first() else {
            return;
        };
        let mut adf = Adf::decode(&std::fs::read(f).unwrap()).unwrap();
        let root = adf.root();
        let map_size = adf.child_value(root, "MapSize").unwrap();
        adf.set_payload(map_size, &((7u64 << 32) | 9).to_le_bytes());
        let grid = adf.child_node(root, "ActiveMapGrid").unwrap();
        let x = adf.child_value(grid, "x").unwrap();
        adf.set_payload(x, &[1, 2, 3]); // different length -> appended
        // Existing name: reuses its id. New names: fresh ids + dict entries.
        adf.add_value(grid, Some("y"), &5u32.to_le_bytes()).unwrap();
        let node = adf.add_node(root, Some("EditorExtra")).unwrap();
        adf.add_value(node, Some("Note"), &utf16_bytes("hi"))
            .unwrap();
        adf.add_value(node, None, &[0xAA]).unwrap();
        let unnamed = adf.add_node(node, None).unwrap();
        adf.add_value(unnamed, Some("Flag"), &[1]).unwrap();
        assert!(adf.remove(root, adf.child_node(root, "MapSource").unwrap()));

        let bytes = adf.encode();
        let back = Adf::decode(&bytes).unwrap();
        let r = back.root();
        assert_eq!(
            back.read_u64(back.child_value(r, "MapSize").unwrap())
                .unwrap(),
            (7 << 32) | 9
        );
        assert_eq!(
            back.payload(back.value(r, "ActiveMapGrid/x").unwrap()),
            &[1, 2, 3]
        );
        let ys: Vec<_> = back
            .children_named(back.node(r, "ActiveMapGrid").unwrap(), "y", true)
            .collect();
        assert_eq!(ys.len(), 2);
        assert_eq!(back.read_u32(ys[1]).unwrap(), 5);
        let extra = back.child_node(r, "EditorExtra").unwrap();
        assert_eq!(
            back.read_utf16(back.child_value(extra, "Note").unwrap())
                .unwrap(),
            "hi"
        );
        let kids: Vec<_> = back.children(extra).collect();
        assert_eq!(back.record_id(kids[1]), VALUE_ID_BASE);
        assert_eq!(back.record_id(kids[2]), 1);
        assert_eq!(
            back.read_u8(back.child_value(kids[2], "Flag").unwrap())
                .unwrap(),
            1
        );
        assert!(back.child_node(r, "MapSource").is_none());
        // Trailer items = 1 + every opener and value; re-encode is stable.
        assert_eq!(back.encode(), bytes);
        let items = u32::from_le_bytes(
            bytes[bytes.len() - 20..bytes.len() - 16]
                .try_into()
                .unwrap(),
        );
        let mut n = 0;
        let mut stack = vec![r];
        while let Some(i) = stack.pop() {
            for c in back.children(i) {
                n += 1;
                stack.push(c);
            }
        }
        assert_eq!(items, n + 1);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Adf::decode(&[0; 8]).is_err());
        let mut b = vec![0u8; 24];
        b[20..].copy_from_slice(&(-1i32).to_le_bytes());
        assert!(
            Adf::decode(&b)
                .unwrap_err()
                .to_string()
                .contains("unknown trailer marker")
        );
        b[20..].copy_from_slice(&7i32.to_le_bytes());
        assert!(Adf::decode(&b).is_err());
    }

    #[test]
    fn hashes_like_the_engine() {
        assert_eq!(fnv1a32(b"Terrain"), 0xA7FF_0CAE);
        assert_eq!(fnv1a32(b"ArchiveMap"), 0xAD97_8431);
        assert_eq!(fnv1a32(b""), 0x811C_9DC5);
    }

    // Flattened pre-order (kind, id, name, payload) of nodes and values.
    type Flat = Vec<(bool, u16, Option<String>, Vec<u8>)>;

    fn flatten_ours(adf: &Adf, node: ItemId, out: &mut Flat) {
        for c in adf.children(node) {
            out.push((
                adf.is_node(c),
                adf.record_id(c),
                adf.name(c).map(str::to_owned),
                adf.payload(c).to_vec(),
            ));
            if adf.is_node(c) {
                flatten_ours(adf, c, out);
            }
        }
    }

    fn flatten_python(tree: &serde_json::Value, out: &mut Flat) {
        for rec in tree.as_array().unwrap() {
            let t = rec["t"].as_str().unwrap();
            if t == "close" {
                continue;
            }
            let id = rec["id"].as_u64().unwrap() as u16;
            let name = rec["name"].as_str().map(str::to_owned);
            let w = rec["w"].as_u64().unwrap_or(0) as usize;
            let payload = if let Some(raw) = rec["raw"].as_str() {
                (0..raw.len() / 2)
                    .map(|i| u8::from_str_radix(&raw[2 * i..2 * i + 2], 16).unwrap())
                    .collect()
            } else if let Some(v) = rec["v"].as_u64() {
                v.to_le_bytes()[..w].to_vec()
            } else {
                Vec::new()
            };
            out.push((t == "node", id, name, payload));
            if t == "node" {
                flatten_python(&rec["children"], out);
            }
        }
    }

    // Independent check against scripts/adf.py.
    #[test]
    fn matches_python_reference() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/adf.py");
        let mut files = Vec::new();
        files_with_ext(
            &extract_dir().join("provinces_roman"),
            "a7minfo",
            &mut files,
        );
        let Some(file) = files
            .iter()
            .find(|f| f.to_string_lossy().contains("pirate_01"))
            .or(files.first())
        else {
            return;
        };
        let out = std::process::Command::new("python3")
            .arg("-I")
            .arg(&script)
            .arg("decode")
            .arg(file)
            .arg("--compact")
            .output();
        let Some(out) = out.ok().filter(|o| o.status.success()) else {
            eprintln!("skip: python3 or scripts/adf.py unavailable");
            return;
        };
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let mut theirs = Flat::new();
        flatten_python(&doc["tree"], &mut theirs);
        let adf = Adf::decode(&std::fs::read(file).unwrap()).unwrap();
        let mut ours = Flat::new();
        flatten_ours(&adf, adf.root(), &mut ours);
        assert!(!ours.is_empty());
        assert_eq!(ours, theirs);
    }

    // file_h.db: ~90 MB, ~600k nodes. Needs the installed game.
    #[test]
    fn roundtrips_file_h_db() {
        let Ok(dir) = std::env::var("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let bytes = std::fs::read(Path::new(&dir).join("maindata/file_h.db")).unwrap();
        let t = std::time::Instant::now();
        let adf = Adf::decode(&bytes).unwrap();
        eprintln!(
            "file_h.db: decoded {} MB in {:?}",
            bytes.len() >> 20,
            t.elapsed()
        );
        let files = adf
            .node(adf.root(), "ArchiveMap/FileMap")
            .or_else(|| adf.node(adf.root(), "FileMap"));
        assert!(files.is_some_and(|f| adf.children(f).count() > 100_000));
        assert!(adf.encode() == bytes);
    }
}
