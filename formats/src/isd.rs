// Anno 2070's serialized documents, the container of `.isd` islands and
// `.www` worlds: parsed into a flat node list and written back byte for
// byte from that list alone, so the writer is tested against every shipped
// file and not against a copy of it.
//
// The format, as every shipped `.isd` and `.www` has it (2026-11-18 census):
// a sequence of `<Name>` elements, each holding text, nested elements, or a
// binary value `CDATA[<u32 length><bytes>]`. Every close tag, and nothing
// else, is followed by CRLF; there are no attributes, declarations,
// comments or self-closing tags. Most close as `</Name>`, but the terrain
// chunk map closes its tags with a bare `</>` (and `Width` and `Height`
// close both ways), so each node keeps how it closed. Anything else fails
// the parse instead of being normalised away.

pub mod grids;
pub mod objects;
pub mod terrain;
pub mod world;

use anyhow::{Result, bail, ensure};
use std::collections::HashMap;

/// A parsed document. Nodes are in document order: a node's children follow
/// it, and its subtree ends where its next sibling starts.
pub struct Doc {
    nodes: Vec<Node>,
    names: Vec<Box<[u8]>>,
    /// Every text and binary value, back to back. A changed value is
    /// appended and the old bytes are left behind; documents are edited a
    /// handful of values at a time.
    values: Vec<u8>,
}

/// A node's index in its [`Doc`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeId(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    /// `<Name></Name>`: no text, no children.
    Empty,
    Text(&'a [u8]),
    /// A `CDATA[<u32 length><bytes>]` value, without its length.
    Blob(&'a [u8]),
    /// Nested elements; see [`Doc::children`].
    Children,
}

#[derive(Clone, Copy)]
struct Node {
    /// Name index in the low 24 bits; [`Kind`] and [`SHORT_CLOSE`] above.
    name: u32,
    /// The node after this one's subtree: its next sibling, or its parent's
    /// end.
    end: u32,
    value: u32,
    len: u32,
}

const NAME_BITS: u32 = 24;
const NAME_MASK: u32 = (1 << NAME_BITS) - 1;
const KIND_SHIFT: u32 = NAME_BITS;
const KIND_MASK: u32 = 0b11;
/// Closes with a bare `</>`.
const SHORT_CLOSE: u32 = 1 << 31;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum Kind {
    Empty = 0,
    Text = 1,
    Blob = 2,
    Children = 3,
}

impl Kind {
    // By their bits.
    const ALL: [Kind; 4] = [Kind::Empty, Kind::Text, Kind::Blob, Kind::Children];
}

const CDATA: &[u8] = b"CDATA[";
// A binary value's length, before its bytes.
const CDATA_LEN: usize = size_of::<u32>();
// Fixed-point metres, as positions, polygon points and the submarine
// blocker's level and radius are stored: 4096 per metre.
const FIXED: i64 = 4096;
const CRLF: &[u8] = b"\r\n";

impl Node {
    fn kind(self) -> Kind {
        let bits = (self.name >> KIND_SHIFT) & KIND_MASK;
        Kind::ALL[bits as usize]
    }

    fn set_kind(&mut self, kind: Kind) {
        self.name = (self.name & !(KIND_MASK << KIND_SHIFT)) | ((kind as u32) << KIND_SHIFT);
    }
}

impl Doc {
    pub fn parse(bytes: &[u8]) -> Result<Doc> {
        // Offsets are u32; the largest shipped island is 5.3 MB.
        ensure!(bytes.len() < u32::MAX as usize, "document over 4 GB");
        let mut doc = Doc {
            nodes: Vec::with_capacity(bytes.len() / 40),
            names: Vec::new(),
            values: Vec::with_capacity(bytes.len()),
        };
        let mut index: HashMap<&[u8], u32, foldhash::fast::RandomState> = HashMap::default();
        // Open elements, innermost last.
        let mut open: Vec<u32> = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let rest = &bytes[at..];
            ensure!(rest[0] == b'<', "byte {at}: expected a tag");
            if let Some(after) = rest.strip_prefix(b"</") {
                let Some(close) = after.iter().position(|&c| c == b'>') else {
                    bail!("byte {at}: unterminated close tag");
                };
                let Some(id) = open.pop() else {
                    bail!("byte {at}: close tag with nothing open");
                };
                let end = doc.nodes.len() as u32;
                let node = &mut doc.nodes[id as usize];
                let name = &after[..close];
                if name.is_empty() {
                    node.name |= SHORT_CLOSE;
                } else {
                    let own = &doc.names[(node.name & NAME_MASK) as usize];
                    ensure!(
                        **own == *name,
                        "byte {at}: </{}> closes <{}>",
                        String::from_utf8_lossy(name),
                        String::from_utf8_lossy(own)
                    );
                }
                node.end = end;
                at += 2 + close + 1;
                ensure!(
                    bytes[at..].starts_with(CRLF),
                    "byte {at}: close tag not followed by CRLF"
                );
                at += CRLF.len();
                continue;
            }

            let Some(close) = rest.iter().position(|&c| c == b'>') else {
                bail!("byte {at}: unterminated tag");
            };
            let name = &rest[1..close];
            ensure!(
                !name.is_empty() && name.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_'),
                "byte {at}: unexpected tag <{}>",
                String::from_utf8_lossy(name)
            );
            let next = doc.names.len() as u32;
            let name_id = *index.entry(name).or_insert_with(|| {
                doc.names.push(name.into());
                next
            });
            ensure!(name_id <= NAME_MASK, "too many tag names");
            if let Some(&parent) = open.last() {
                doc.nodes[parent as usize].set_kind(Kind::Children);
            }
            let id = doc.nodes.len() as u32;
            ensure!(id < u32::MAX, "too many nodes");
            open.push(id);
            at += close + 1;

            let body = &bytes[at..];
            let mut node = Node {
                name: name_id,
                end: id + 1,
                value: doc.values.len() as u32,
                len: 0,
            };
            if let Some(blob) = body.strip_prefix(CDATA) {
                let Some(len) = (blob.first_chunk::<CDATA_LEN>()).map(|n| u32::from_le_bytes(*n))
                else {
                    bail!("byte {at}: CDATA without a length");
                };
                let end = CDATA_LEN + len as usize;
                ensure!(
                    blob.get(end) == Some(&b']') && blob[end + 1..].starts_with(b"</"),
                    "byte {at}: CDATA of {len} bytes does not end in `]</`"
                );
                doc.values.extend_from_slice(&blob[CDATA_LEN..end]);
                node.len = len;
                node.set_kind(Kind::Blob);
                at += CDATA.len() + end + 1;
            } else {
                let text = body.iter().position(|&c| c == b'<').unwrap_or(body.len());
                if body[text..].starts_with(b"</") {
                    doc.values.extend_from_slice(&body[..text]);
                    node.len = text as u32;
                    node.set_kind(if text == 0 { Kind::Empty } else { Kind::Text });
                    at += text;
                } else {
                    // Children follow at once; the parent turns `Children`
                    // when the first one opens.
                    ensure!(text == 0, "byte {at}: text before a nested element");
                }
            }
            doc.nodes.push(node);
        }
        ensure!(open.is_empty(), "document ends inside an element");
        Ok(doc)
    }

    /// The document's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.values.len() + self.nodes.len() * 24);
        // Close tags still due: (node, where its subtree ends).
        let mut open: Vec<(u32, u32)> = Vec::new();
        for (id, node) in self.nodes.iter().enumerate() {
            let id = id as u32;
            while let Some(&(parent, end)) = open.last()
                && end == id
            {
                self.close(parent, &mut out);
                open.pop();
            }
            out.push(b'<');
            out.extend_from_slice(&self.names[(node.name & NAME_MASK) as usize]);
            out.push(b'>');
            let value = &self.values[node.value as usize..][..node.len as usize];
            match node.kind() {
                Kind::Empty => {}
                Kind::Text => out.extend_from_slice(value),
                Kind::Blob => {
                    out.extend_from_slice(CDATA);
                    out.extend_from_slice(&node.len.to_le_bytes());
                    out.extend_from_slice(value);
                    out.push(b']');
                }
                Kind::Children => {
                    open.push((id, node.end));
                    continue;
                }
            }
            self.close(id, &mut out);
        }
        while let Some((parent, _)) = open.pop() {
            self.close(parent, &mut out);
        }
        out
    }

    fn close(&self, id: u32, out: &mut Vec<u8>) {
        let node = self.nodes[id as usize];
        out.extend_from_slice(b"</");
        if node.name & SHORT_CLOSE == 0 {
            out.extend_from_slice(&self.names[(node.name & NAME_MASK) as usize]);
        }
        out.extend_from_slice(b">\r\n");
    }

    /// The top-level elements.
    pub fn roots(&self) -> Children<'_> {
        Children {
            doc: self,
            next: 0,
            end: self.nodes.len() as u32,
        }
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        let node = self.nodes[id.0 as usize];
        let next = if node.kind() == Kind::Children {
            id.0 + 1
        } else {
            node.end
        };
        Children {
            doc: self,
            next,
            end: node.end,
        }
    }

    /// The first child named `name`.
    pub fn child(&self, id: NodeId, name: &str) -> Option<NodeId> {
        self.children(id).find(|&c| self.name(c) == name.as_bytes())
    }

    /// The first top-level element named `name`.
    pub fn root(&self, name: &str) -> Option<NodeId> {
        self.roots().find(|&c| self.name(c) == name.as_bytes())
    }

    pub fn name(&self, id: NodeId) -> &[u8] {
        &self.names[(self.nodes[id.0 as usize].name & NAME_MASK) as usize]
    }

    pub fn value(&self, id: NodeId) -> Value<'_> {
        let node = self.nodes[id.0 as usize];
        let bytes = &self.values[node.value as usize..][..node.len as usize];
        match node.kind() {
            Kind::Empty => Value::Empty,
            Kind::Text => Value::Text(bytes),
            Kind::Blob => Value::Blob(bytes),
            Kind::Children => Value::Children,
        }
    }

    /// Replaces a value: text, or a binary value when `blob`. Fails on a
    /// node with children.
    pub fn set(&mut self, id: NodeId, bytes: &[u8], blob: bool) -> Result<()> {
        ensure!(
            self.nodes[id.0 as usize].kind() != Kind::Children,
            "<{}> holds elements, not a value",
            String::from_utf8_lossy(self.name(id))
        );
        ensure!(
            blob || !(bytes.contains(&b'<') || bytes.starts_with(CDATA)),
            "text values cannot hold `<` or start like a binary value"
        );
        ensure!(
            self.values.len() + bytes.len() < u32::MAX as usize,
            "values over 4 GB"
        );
        let node = &mut self.nodes[id.0 as usize];
        node.value = self.values.len() as u32;
        node.len = bytes.len() as u32;
        node.set_kind(match (blob, bytes.is_empty()) {
            (true, _) => Kind::Blob,
            (false, true) => Kind::Empty,
            (false, false) => Kind::Text,
        });
        self.values.extend_from_slice(bytes);
        Ok(())
    }

    /// Renames a node; its value and children stay.
    fn rename(&mut self, id: NodeId, name: &str) -> Result<()> {
        let index = self.name_index(name)?;
        let node = &mut self.nodes[id.0 as usize];
        node.name = (node.name & !NAME_MASK) | index;
        Ok(())
    }

    /// Copies `src`'s subtree in as a child of `parent`, before its child
    /// `before`, or last. The copy shares `src`'s values until one is
    /// [`set`](Self::set). Ids from the insertion point on move; look them
    /// up again.
    fn insert_copy(
        &mut self,
        src: NodeId,
        parent: NodeId,
        before: Option<NodeId>,
    ) -> Result<NodeId> {
        let at = self.insertion_point(parent, before)?;
        let (s, e) = (src.0, self.nodes[src.0 as usize].end);
        let copy: Vec<Node> = self.nodes[s as usize..e as usize]
            .iter()
            .map(|&node| Node {
                end: node.end - s + at,
                ..node
            })
            .collect();
        self.splice(parent, at, &copy)
    }

    /// Copies the subtrees of `srcs` in as `parent`'s last children, in
    /// order, in one pass over the nodes, where copying them one by one
    /// walks them all once per copy; returns the copies. Each `src` must
    /// stand before `parent`'s end. Ids from that end on move.
    pub fn append_copies(&mut self, parent: NodeId, srcs: &[NodeId]) -> Result<Vec<NodeId>> {
        let at = self.insertion_point(parent, None)?;
        let mut copies = Vec::new();
        let mut ids = Vec::with_capacity(srcs.len());
        for &src in srcs {
            ensure!(src.0 < at, "a copied subtree after the insertion point");
            let (s, e) = (src.0, self.nodes[src.0 as usize].end);
            let base = at + copies.len() as u32;
            ids.push(NodeId(base));
            copies.extend(self.nodes[s as usize..e as usize].iter().map(|&node| Node {
                end: node.end - s + base,
                ..node
            }));
        }
        if !copies.is_empty() {
            self.splice(parent, at, &copies)?;
        }
        Ok(ids)
    }

    /// Adds a new element `name` as a child of `parent`, before its child
    /// `before`, or last, closing with its full name. It starts empty;
    /// [`set`](Self::set) gives it a value, and inserting into it gives it
    /// children. Ids from the insertion point on move; look them up again.
    pub fn insert(&mut self, parent: NodeId, before: Option<NodeId>, name: &str) -> Result<NodeId> {
        let at = self.insertion_point(parent, before)?;
        let index = self.name_index(name)?;
        let node = Node {
            name: index,
            end: at + 1,
            value: 0,
            len: 0,
        };
        self.splice(parent, at, &[node])
    }

    fn insertion_point(&self, parent: NodeId, before: Option<NodeId>) -> Result<u32> {
        let p = self.nodes[parent.0 as usize];
        ensure!(
            p.kind() == Kind::Children || p.kind() == Kind::Empty,
            "<{}> holds a value, not elements",
            String::from_utf8_lossy(self.name(parent))
        );
        match before {
            Some(b) => {
                ensure!(
                    self.children(parent).any(|c| c == b),
                    "insertion point is not a child of its parent"
                );
                Ok(b.0)
            }
            None => Ok(p.end),
        }
    }

    // Puts `nodes`, a subtree whose ends already count from `at`, in as
    // `parent`'s child at `at`.
    fn splice(&mut self, parent: NodeId, at: u32, nodes: &[Node]) -> Result<NodeId> {
        let n = nodes.len() as u32;
        ensure!(
            (self.nodes.len() as u64 + n as u64) < u32::MAX as u64,
            "too many nodes"
        );
        // `parent` and its ancestors grow around the insert; their subtrees
        // start at or before `parent` and end after it. Every other node
        // before the insertion point ends at or before it, and so stays.
        for (id, node) in self.nodes.iter_mut().enumerate() {
            let id = id as u32;
            if id >= at || (id <= parent.0 && node.end > parent.0) {
                node.end += n;
            }
        }
        self.nodes
            .splice(at as usize..at as usize, nodes.iter().copied());
        let p = &mut self.nodes[parent.0 as usize];
        p.set_kind(Kind::Children);
        p.len = 0;
        Ok(NodeId(at))
    }

    fn name_index(&mut self, name: &str) -> Result<u32> {
        ensure!(
            !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "<{name}> is not a tag name"
        );
        let found = self.names.iter().position(|n| **n == *name.as_bytes());
        let index = found.unwrap_or_else(|| {
            self.names.push(name.as_bytes().into());
            self.names.len() - 1
        }) as u32;
        ensure!(index <= NAME_MASK, "too many tag names");
        Ok(index)
    }

    /// Removes `id`'s subtree. Ids after it move; look them up again.
    pub fn remove(&mut self, id: NodeId) {
        let (s, e) = (id.0, self.nodes[id.0 as usize].end);
        let n = e - s;
        self.nodes.drain(s as usize..e as usize);
        // Ancestors end at or after the removed subtree; earlier siblings
        // and their subtrees end at its start and stay.
        for node in &mut self.nodes {
            if node.end >= e {
                node.end -= n;
            }
        }
        // A parent left without children writes as `<P></P>`, which reads
        // back as empty.
        for i in 0..self.nodes.len() {
            let node = self.nodes[i];
            if node.kind() == Kind::Children && node.end == i as u32 + 1 {
                self.nodes[i].set_kind(Kind::Empty);
                self.nodes[i].len = 0;
            }
        }
    }

    /// Removes the subtrees of `ids` in one pass over the nodes, where
    /// [`remove`](Self::remove) walks them all once per subtree: a save
    /// that drops thousands of records stays linear. A node inside another
    /// one's subtree goes with it. Ids after the first removed node move.
    pub fn remove_all(&mut self, ids: &[NodeId]) {
        let mut spans: Vec<(u32, u32)> = (ids.iter())
            .map(|id| (id.0, self.nodes[id.0 as usize].end))
            .collect();
        spans.sort_unstable();
        // `before[i]`: removed nodes ahead of node `i`. Every kept node's
        // end is a sibling's or an ancestor's end, never inside a removed
        // subtree, so it moves back by exactly that many.
        let n = self.nodes.len();
        let mut before = Vec::with_capacity(n + 1);
        let (mut gone, mut span) = (0u32, 0);
        let mut until = 0u32;
        for i in 0..n as u32 {
            before.push(gone);
            while span < spans.len() && spans[span].0 <= i {
                until = until.max(spans[span].1);
                span += 1;
            }
            gone += u32::from(i < until);
        }
        before.push(gone);
        if gone == 0 {
            return;
        }
        let mut w = 0;
        for i in 0..n {
            if before[i + 1] != before[i] {
                continue;
            }
            let mut node = self.nodes[i];
            node.end -= before[node.end as usize];
            // A parent left without children writes as `<P></P>`, which
            // reads back as empty.
            if node.kind() == Kind::Children && node.end == w as u32 + 1 {
                node.set_kind(Kind::Empty);
                node.len = 0;
            }
            self.nodes[w] = node;
            w += 1;
        }
        self.nodes.truncate(w);
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }
}

/// Sibling elements, in document order.
pub struct Children<'a> {
    doc: &'a Doc,
    next: u32,
    end: u32,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next >= self.end {
            return None;
        }
        let id = NodeId(self.next);
        self.next = self.doc.nodes[self.next as usize].end;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A document in the shipped layout, written out by hand: the header's
    // full close tags, a bitgrid's binary value, an empty element, and a
    // terrain chunk closed with bare `</>` tags.
    fn sample() -> Vec<u8> {
        sample_with(b"32", &[1.5, -2.25])
    }

    fn sample_with(width: &[u8], heights: &[f32]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(b"<Width>");
        b.extend(width);
        b.extend(b"</Width>\r\n<Height>32</Height>\r\n");
        b.extend(b"<UsedChunks><m_XSize>2</m_XSize>\r\n<m_BitGrid>CDATA[");
        b.extend(3u32.to_le_bytes());
        b.extend(b"\x0f]\r]</m_BitGrid>\r\n</UsedChunks>\r\n");
        b.extend(b"<Name></Name>\r\n");
        b.extend(b"<Terrain><Version>3</>\r\n<ChunkMap><Width>2</>\r\n<Element>");
        b.extend(b"<VertexResolution>4</>\r\n<HeightMap><Width>17</>\r\n<Data>CDATA[");
        b.extend((heights.len() as u32 * 4).to_le_bytes());
        for h in heights {
            b.extend(h.to_le_bytes());
        }
        b.extend(b"]</>\r\n</>\r\n</>\r\n</>\r\n</Terrain>\r\n");
        b
    }

    #[test]
    fn writes_back_what_it_read() {
        let bytes = sample();
        let doc = Doc::parse(&bytes).unwrap();
        assert_eq!(doc.encode(), bytes);
    }

    #[test]
    fn tree_matches_the_text() {
        let doc = Doc::parse(&sample()).unwrap();
        let roots: Vec<&[u8]> = doc.roots().map(|r| doc.name(r)).collect();
        assert_eq!(
            roots,
            [&b"Width"[..], b"Height", b"UsedChunks", b"Name", b"Terrain"]
        );
        let width = doc.root("Width").unwrap();
        assert_eq!(doc.value(width), Value::Text(b"32"));
        assert_eq!(doc.value(doc.root("Name").unwrap()), Value::Empty);
        let chunks = doc.root("UsedChunks").unwrap();
        let grid = doc.child(chunks, "m_BitGrid").unwrap();
        // The value holds `]` and CR: only the length says where it ends.
        assert_eq!(doc.value(grid), Value::Blob(b"\x0f]\r"));
        let terrain = doc.root("Terrain").unwrap();
        let map = doc.child(terrain, "ChunkMap").unwrap();
        let element = doc.child(map, "Element").unwrap();
        let heights = doc.child(element, "HeightMap").unwrap();
        let Value::Blob(data) = doc.value(doc.child(heights, "Data").unwrap()) else {
            panic!("heights are binary");
        };
        assert_eq!(f32::from_le_bytes(data[4..8].try_into().unwrap()), -2.25);
        assert_eq!(doc.children(heights).count(), 2);
    }

    // An edit with independently known bytes: the expected document is
    // spelled out, not produced by the writer.
    #[test]
    fn an_edited_value_writes_where_it_was() {
        let mut doc = Doc::parse(&sample()).unwrap();
        let terrain = doc.root("Terrain").unwrap();
        let map = doc.child(terrain, "ChunkMap").unwrap();
        let element = doc.child(map, "Element").unwrap();
        let heights = doc.child(element, "HeightMap").unwrap();
        let data = doc.child(heights, "Data").unwrap();
        let raised: Vec<u8> = [1.5f32, 4., 0.5]
            .iter()
            .flat_map(|h| h.to_le_bytes())
            .collect();
        doc.set(data, &raised, true).unwrap();
        let width = doc.root("Width").unwrap();
        doc.set(width, b"48", false).unwrap();
        assert_eq!(doc.encode(), sample_with(b"48", &[1.5, 4., 0.5]));
    }

    // Inserted and removed subtrees, against a document spelled out by hand.
    #[test]
    fn inserted_and_removed_subtrees_write_where_they_were_put() {
        let mut doc = Doc::parse(&sample_with(b"32", &[])).unwrap();
        let element = |doc: &Doc| {
            let map = doc.child(doc.root("Terrain").unwrap(), "ChunkMap").unwrap();
            doc.child(map, "Element").unwrap()
        };
        // Into an empty element, and before a sibling.
        let size = doc
            .child(doc.root("UsedChunks").unwrap(), "m_XSize")
            .unwrap();
        doc.insert_copy(size, doc.root("Name").unwrap(), None)
            .unwrap();
        let e = element(&doc);
        let (heights, resolution) = (
            doc.child(e, "HeightMap").unwrap(),
            doc.child(e, "VertexResolution").unwrap(),
        );
        doc.insert_copy(heights, e, Some(resolution)).unwrap();
        let e = element(&doc);
        let original = doc
            .children(e)
            .filter(|&c| doc.name(c) == b"HeightMap")
            .nth(1)
            .unwrap();
        doc.remove(original);

        let want = |name: &[u8]| {
            let mut b = Vec::new();
            b.extend(b"<Width>32</Width>\r\n<Height>32</Height>\r\n");
            b.extend(b"<UsedChunks><m_XSize>2</m_XSize>\r\n<m_BitGrid>CDATA[");
            b.extend(3u32.to_le_bytes());
            b.extend(b"\x0f]\r]</m_BitGrid>\r\n</UsedChunks>\r\n");
            b.extend(name);
            b.extend(b"<Terrain><Version>3</>\r\n<ChunkMap><Width>2</>\r\n<Element>");
            b.extend(b"<HeightMap><Width>17</>\r\n<Data>CDATA[");
            b.extend(0u32.to_le_bytes());
            b.extend(b"]</>\r\n</>\r\n<VertexResolution>4</>\r\n</>\r\n</>\r\n</Terrain>\r\n");
            b
        };
        let full = want(b"<Name><m_XSize>2</m_XSize>\r\n</Name>\r\n");
        assert_eq!(doc.encode(), full);
        assert_eq!(Doc::parse(&full).unwrap().len(), doc.len());

        // A rename to a new name and back to one the document holds.
        let size = doc.child(doc.root("Name").unwrap(), "m_XSize").unwrap();
        doc.rename(size, "m_YSize").unwrap();
        assert_eq!(
            doc.encode(),
            want(b"<Name><m_YSize>2</m_YSize>\r\n</Name>\r\n")
        );
        doc.rename(size, "m_XSize").unwrap();
        assert_eq!(doc.encode(), full);
        assert!(doc.rename(size, "a b").is_err());

        // Removing the last child leaves the element empty, as it reads back.
        let name = doc.root("Name").unwrap();
        doc.remove(doc.child(name, "m_XSize").unwrap());
        assert_eq!(doc.value(doc.root("Name").unwrap()), Value::Empty);
        assert_eq!(doc.encode(), want(b"<Name></Name>\r\n"));

        // New elements: a text value into an element with children, and
        // a nested one into an empty element.
        let name = doc.root("Name").unwrap();
        let size = doc.insert(name, None, "m_XSize").unwrap();
        doc.set(size, b"2", false).unwrap();
        let empty = doc.insert(name, Some(size), "Box").unwrap();
        doc.insert(empty, None, "In").unwrap();
        let name = doc.root("Name").unwrap();
        let note = doc.insert(name, None, "Note").unwrap();
        doc.set(note, b"hi", false).unwrap();
        assert_eq!(
            doc.encode(),
            want(b"<Name><Box><In></In>\r\n</Box>\r\n<m_XSize>2</m_XSize>\r\n<Note>hi</Note>\r\n</Name>\r\n")
        );
        assert!(doc.insert(note, None, "X").is_err());
    }

    // Many subtrees at once, given out of order, one twice and one inside
    // another: what is left writes as spelled out, a list left without
    // children reads back empty, and the nodes after the removals are
    // still found. Copies appended in one pass write where they were put
    // and take their own values.
    #[test]
    fn many_subtrees_remove_and_copy_in_one_pass() {
        let text = b"<List><A>1</A>\r\n<B><C>2</C>\r\n</B>\r\n<D>3</D>\r\n</List>\r\n\
            <Pairs><k>1</k>\r\n<v>x</v>\r\n</Pairs>\r\n<Tail>t</Tail>\r\n";
        let mut doc = Doc::parse(text).unwrap();
        let list = doc.root("List").unwrap();
        let pairs = doc.root("Pairs").unwrap();
        let b = doc.child(list, "B").unwrap();
        let gone = [
            doc.child(pairs, "v").unwrap(),
            doc.child(list, "D").unwrap(),
            doc.child(b, "C").unwrap(),
            b,
            doc.child(pairs, "k").unwrap(),
            b,
        ];
        doc.remove_all(&gone);
        let want = b"<List><A>1</A>\r\n</List>\r\n<Pairs></Pairs>\r\n<Tail>t</Tail>\r\n";
        assert_eq!(doc.encode(), want);
        assert_eq!(doc.len(), Doc::parse(want).unwrap().len());
        assert_eq!(doc.value(doc.root("Pairs").unwrap()), Value::Empty);
        assert_eq!(doc.value(doc.root("Tail").unwrap()), Value::Text(b"t"));
        doc.remove_all(&[]);
        assert_eq!(doc.encode(), want);

        // Copies of a nested and a plain element, at a list's end.
        let text = b"<List><A>1</A>\r\n<B><C>2</C>\r\n</B>\r\n</List>\r\n<Tail>t</Tail>\r\n";
        let mut doc = Doc::parse(text).unwrap();
        let list = doc.root("List").unwrap();
        let (a, b) = (doc.child(list, "A").unwrap(), doc.child(list, "B").unwrap());
        let copies = doc.append_copies(list, &[b, a, b]).unwrap();
        doc.set(doc.child(copies[0], "C").unwrap(), b"3", false)
            .unwrap();
        doc.set(copies[1], b"4", false).unwrap();
        assert_eq!(
            doc.encode(),
            b"<List><A>1</A>\r\n<B><C>2</C>\r\n</B>\r\n<B><C>3</C>\r\n</B>\r\n<A>4</A>\r\n\
              <B><C>2</C>\r\n</B>\r\n</List>\r\n<Tail>t</Tail>\r\n"
        );
        assert_eq!(doc.value(doc.root("Tail").unwrap()), Value::Text(b"t"));
        let tail = doc.root("Tail").unwrap();
        assert!(doc.append_copies(list, &[tail]).is_err());
    }

    #[test]
    fn anything_off_layout_fails() {
        let bad: &[&[u8]] = &[
            b"<A>1</A>",                             // no CRLF after a close
            b"<A>1</A>\n",                           // LF only
            b"<A>1</B>\r\n",                         // wrong close
            b"<A>1\r\n",                             // unclosed
            b"<A x=\"1\">1</A>\r\n",                 // attribute
            b"<A/>\r\n",                             // self-closing
            b"<?xml?>\r\n",                          // declaration
            b"<A>x<B>1</B>\r\n</A>\r\n",             // text before an element
            b"<A>\r\n<B>1</B>\r\n</A>\r\n",          // CRLF after an open tag
            b"<A>CDATA[\x09\x00\x00\x00ab]</A>\r\n", // length past the value
            b"</A>\r\n",
        ];
        for b in bad {
            assert!(Doc::parse(b).is_err(), "{}", String::from_utf8_lossy(b));
        }
    }
}
