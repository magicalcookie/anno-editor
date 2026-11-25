// Anno's hand-written engine XML (cfgs, prps, ambient presets, materials,
// globals), which closes tags with a bare `</>` that a real XML parser
// rejects: `Doc` holds a whole file as a tree that writes back byte for
// byte, for editors; the free functions read and edit single values in text.

/// A whole engine-XML file as an element tree that keeps every byte:
/// whitespace, both closer styles, stray closers, garbage in unused
/// strings. Writing an unedited doc gives the input back, and an edit
/// changes only the bytes of what it edits.
///
/// Elements live in one array linked by index, their raw bytes in one
/// arena and their decoded names and texts in another. Edits append to the
/// arenas and relink, so a removed element's entry and bytes stay until the
/// next parse: bounded by a session's edits, small next to a file of a few
/// hundred KB. Cloning is a whole-document snapshot, which is what undo
/// keeps.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    bytes: Vec<u8>,
    strs: String,
    elements: Vec<Element>,
    /// The file's line break (`\r\n` in every shipped cfg); inserted
    /// elements use it.
    newline: &'static str,
    /// The file closes tags with `</>`; elements the doc writes do too.
    short_close: bool,
    /// Prefixed to every element name, as 2070's serializer writes
    /// (`m_Config`, `m_Transformer`): names read without it, and elements
    /// the doc writes get it. Found from the first element.
    prefix: &'static str,
}

/// An element of a [`Doc`]. Stays valid across edits: a removed element is
/// unlinked, not freed, so undo snapshots and selections keep their ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(u32);

impl Id {
    /// The document itself: the file's top-level elements are its children.
    pub const ROOT: Id = Id(0);

    /// As a number, for a renderer's pick handles.
    pub fn raw(self) -> u32 {
        self.0
    }

    /// Back from [`Id::raw`]; only for numbers a doc's ids gave.
    pub fn from_raw(raw: u32) -> Id {
        Id(raw)
    }
}

const NONE: u32 = u32::MAX;

/// A range of [`Doc::bytes`] or [`Doc::strs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Span {
    at: u32,
    len: u32,
}

impl Span {
    fn new(from: usize, to: usize) -> Span {
        Span {
            at: from as u32,
            len: (to - from) as u32,
        }
    }

    fn range(self) -> std::ops::Range<usize> {
        self.at as usize..(self.at + self.len) as usize
    }
}

// An element writes as `lead open (children) tail close`. The lead is what
// stood between the previous sibling (or the parent's open tag) and this
// one, so moving or removing an element takes its indentation with it.
#[derive(Clone, Copy, Debug)]
struct Element {
    parent: u32,
    first: u32,
    last: u32,
    next: u32,
    lead: Span,
    open: Span,
    /// After the last child, or after the open tag of a leaf: a leaf's
    /// value with the whitespace around it.
    tail: Span,
    /// `</>` or `</Name>`; empty when self-closing or unclosed at the end.
    close: Span,
    /// In `strs`, lossy UTF-8.
    name: Span,
    /// In `strs`: every text run inside, trimmed and entity-decoded, run
    /// after run. A leaf's value; mixed content is garbage in shipped files.
    text: Span,
}

impl Element {
    fn new(parent: u32) -> Element {
        Element {
            parent,
            first: NONE,
            last: NONE,
            next: NONE,
            lead: Span::default(),
            open: Span::default(),
            tail: Span::default(),
            close: Span::default(),
            name: Span::default(),
            text: Span::default(),
        }
    }
}

impl Doc {
    /// Never fails: an unclosed element closes at the end of the input and
    /// a stray closer stays as text around it. The game's reader is as
    /// lenient, and shipped files need it (some tools write `</>` one time
    /// too many).
    pub fn parse(input: &[u8]) -> Doc {
        let newline = match input.iter().position(|&c| c == b'\n') {
            Some(i) if i > 0 && input[i - 1] == b'\r' => "\r\n",
            _ => "\n",
        };
        let mut doc = Doc {
            bytes: input.to_vec(),
            strs: String::new(),
            elements: vec![Element::new(NONE)],
            newline,
            short_close: input.windows(3).any(|w| w == b"</>"),
            prefix: "",
        };
        // Open elements with the text gathered inside each so far.
        let mut stack: Vec<(u32, String)> = vec![(0, String::new())];
        // `run` starts the raw bytes not yet given to an element; `read`
        // the text not yet read, which skips stray closers.
        let (mut run, mut read, mut i) = (0, 0, 0);
        while let Some(lt) = find(input, b'<', i) {
            let Some(gt) = find(input, b'>', lt) else {
                break;
            };
            let tag = &input[lt + 1..gt];
            push_text(
                &mut stack.last_mut().expect("root stays").1,
                &input[read..lt],
            );
            if tag.first() == Some(&b'/') {
                if stack.len() > 1 {
                    let (id, inner) = stack.pop().expect("len checked");
                    let text = doc.push_str(&inner);
                    let e = &mut doc.elements[id as usize];
                    e.tail = Span::new(run, lt);
                    e.close = Span::new(lt, gt + 1);
                    e.text = text;
                    run = gt + 1;
                }
            } else {
                let parent = stack.last().expect("root stays").0;
                let closed = tag.last() == Some(&b'/');
                let name = &tag[..tag.len() - closed as usize];
                let id = doc.elements.len() as u32;
                let mut e = Element::new(parent);
                e.lead = Span::new(run, lt);
                e.open = Span::new(lt, gt + 1);
                e.name = doc.push_str(&String::from_utf8_lossy(name));
                doc.elements.push(e);
                doc.link(id, parent, NONE);
                run = gt + 1;
                if !closed {
                    stack.push((id, String::new()));
                    // Binary, '<' and '>' bytes and all: skip it whole.
                    if let Some(end) = cdata_end(input, gt + 1) {
                        (read, i) = (end, end);
                        continue;
                    }
                }
            }
            read = gt + 1;
            i = gt + 1;
        }
        // The rest belongs to the innermost open element; an unterminated
        // `<` ends its text but stays in its bytes.
        let end = input.len();
        let text_end = find(input, b'<', i).unwrap_or(end);
        push_text(
            &mut stack.last_mut().expect("root stays").1,
            &input[read..text_end],
        );
        let mut tail = Span::new(run, end);
        while let Some((id, inner)) = stack.pop() {
            let text = doc.push_str(&inner);
            let e = &mut doc.elements[id as usize];
            e.tail = tail;
            e.close = Span::new(end, end);
            e.text = text;
            tail = Span::new(end, end);
        }
        let first = doc.elements[0].first;
        if first != NONE && doc.str(doc.elements[first as usize].name).starts_with("m_") {
            doc.prefix = "m_";
        }
        doc
    }

    /// What the file prefixes its element names with: `m_` in 2070's.
    pub fn prefix(&self) -> &'static str {
        self.prefix
    }

    /// The file as it stands, edits included.
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len());
        self.emit(0, &mut out);
        out
    }

    fn emit(&self, id: u32, out: &mut Vec<u8>) {
        let e = &self.elements[id as usize];
        out.extend_from_slice(self.raw(e.lead));
        out.extend_from_slice(self.raw(e.open));
        let mut c = e.first;
        while c != NONE {
            self.emit(c, out);
            c = self.elements[c as usize].next;
        }
        out.extend_from_slice(self.raw(e.tail));
        out.extend_from_slice(self.raw(e.close));
    }

    fn raw(&self, s: Span) -> &[u8] {
        &self.bytes[s.range()]
    }

    fn str(&self, s: Span) -> &str {
        &self.strs[s.range()]
    }

    fn push_str(&mut self, s: &str) -> Span {
        let at = self.strs.len();
        self.strs.push_str(s);
        Span::new(at, self.strs.len())
    }

    fn push_bytes(&mut self, b: &[u8]) -> Span {
        let at = self.bytes.len();
        self.bytes.extend_from_slice(b);
        Span::new(at, self.bytes.len())
    }

    /// The tag name without the file's [`Doc::prefix`].
    pub fn name(&self, id: Id) -> &str {
        let name = self.str(self.elements[id.0 as usize].name);
        name.strip_prefix(self.prefix).unwrap_or(name)
    }

    /// A leaf's value, trimmed and entity-decoded; `""` when empty.
    pub fn text(&self, id: Id) -> &str {
        self.str(self.elements[id.0 as usize].text)
    }

    /// `None` for [`Id::ROOT`] and for removed elements' former parents'
    /// view of them: a removed element keeps its parent link.
    pub fn parent(&self, id: Id) -> Option<Id> {
        let p = self.elements[id.0 as usize].parent;
        (p != NONE).then_some(Id(p))
    }

    pub fn children(&self, id: Id) -> Children<'_> {
        Children {
            doc: self,
            at: self.elements[id.0 as usize].first,
        }
    }

    /// The first child named `name`.
    pub fn child(&self, id: Id, name: &str) -> Option<Id> {
        self.children(id).find(|&c| self.name(c) == name)
    }

    /// Has no child elements, so its text is its value.
    pub fn is_leaf(&self, id: Id) -> bool {
        self.elements[id.0 as usize].first == NONE
    }

    /// Still in the tree: every ancestor up to the root is linked. An id
    /// from a later snapshot than this one (undo went back past the edit
    /// that made it) is not.
    pub fn is_attached(&self, mut id: Id) -> bool {
        if id.0 as usize >= self.elements.len() {
            return false;
        }
        while id != Id::ROOT {
            let Some(p) = self.parent(id) else {
                return false;
            };
            if !self.children(p).any(|c| c == id) {
                return false;
            }
            id = p;
        }
        true
    }

    /// A leaf's binary value, `CDATA[<u32 length><bytes>]` as the engine
    /// serializer writes arrays into XML (rdp particle tracks, `.fc`).
    pub fn cdata(&self, id: Id) -> Option<&[u8]> {
        let e = &self.elements[id.0 as usize];
        if e.first != NONE {
            return None;
        }
        let tail = self.raw(e.tail);
        let at = tail.iter().take_while(|c| c.is_ascii_whitespace()).count();
        let end = cdata_end(tail, at)?;
        Some(&tail[at + 10..end - 1])
    }

    /// Sets a leaf's binary value; see [`Doc::cdata`].
    pub fn set_cdata(&mut self, id: Id, payload: &[u8]) {
        let mut value = b"CDATA[".to_vec();
        value.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        value.extend_from_slice(payload);
        value.push(b']');
        self.set_value(id, &value, "");
    }

    /// Sets a leaf's value and keeps the whitespace around it. A
    /// self-closing `<Tag/>` gains a closer.
    pub fn set_text(&mut self, id: Id, value: &str) {
        self.set_value(id, encode_entities(value).as_bytes(), value.trim());
    }

    fn set_value(&mut self, id: Id, raw: &[u8], value: &str) {
        let e = self.elements[id.0 as usize];
        assert!(
            e.first == NONE,
            "set_text on <{}>, which has children",
            self.name(id)
        );
        let old = self.raw(e.tail);
        let lead = old.iter().take_while(|c| c.is_ascii_whitespace()).count();
        let trail = (old[lead..].iter().rev())
            .take_while(|c| c.is_ascii_whitespace())
            .count();
        let mut new = old[..lead].to_vec();
        new.extend_from_slice(raw);
        new.extend_from_slice(&old[old.len() - trail..]);
        let tail = self.push_bytes(&new);
        let text = self.push_str(value);
        let self_closing = e.close.len == 0 && self.raw(e.open).ends_with(b"/>");
        let (open, close) = if self_closing {
            let name = self.str(e.name).to_owned();
            let open = self.push_bytes(format!("<{name}>").as_bytes());
            (open, self.push_bytes(self.closer(&name).as_bytes()))
        } else {
            (e.open, e.close)
        };
        let e = &mut self.elements[id.0 as usize];
        (e.tail, e.text, e.open, e.close) = (tail, text, open, close);
    }

    /// The child leaf `name` set to `value`; added as the last child when
    /// missing.
    pub fn set_child_text(&mut self, id: Id, name: &str, value: &str) -> Id {
        if let Some(c) = self.child(id, name) {
            self.set_text(c, value);
            return c;
        }
        let xml = format!("<{name}>{}{}", encode_entities(value), self.closer(name));
        self.insert(id, None, &xml)
    }

    fn closer(&self, name: &str) -> String {
        match self.short_close {
            true => "</>".to_owned(),
            false => format!("</{name}>"),
        }
    }

    /// Parses `xml`, one element with its children, and links it under
    /// `parent` before `before`, or last. It takes the indentation of the
    /// sibling it lands beside and the file's line breaks.
    pub fn insert(&mut self, parent: Id, before: Option<Id>, xml: &str) -> Id {
        let xml = xml.replace("\r\n", "\n").replace('\n', self.newline);
        let xml = prefixed(&xml, self.prefix);
        let frag = Doc::parse(xml.as_bytes());
        let top = frag.elements[0].first;
        assert!(top != NONE, "insert of {xml:?}, which holds no element");
        let (byte_shift, str_shift) = (self.bytes.len() as u32, self.strs.len() as u32);
        self.bytes.extend_from_slice(&frag.bytes);
        self.strs.push_str(&frag.strs);
        let id = self.copy(&frag.elements, top, parent.0, byte_shift, str_shift);
        self.elements[id as usize].lead = self.sibling_lead(parent, before);
        self.link(id, parent.0, before.map_or(NONE, |b| b.0));
        Id(id)
    }

    /// A deep copy of `id` right after it, sharing its bytes.
    pub fn duplicate(&mut self, id: Id) -> Id {
        let e = self.elements[id.0 as usize];
        let src = self.elements.clone();
        let copy = self.copy(&src, id.0, e.parent, 0, 0);
        self.link(copy, e.parent, e.next);
        Id(copy)
    }

    /// Unlinks `id` and its subtree; its id stays valid for undo and for
    /// [`Doc::move_to`].
    pub fn remove(&mut self, id: Id) {
        assert!(id != Id::ROOT, "remove of the root");
        let p = self.elements[id.0 as usize].parent;
        let mut prev = NONE;
        let mut c = self.elements[p as usize].first;
        while c != NONE && c != id.0 {
            prev = c;
            c = self.elements[c as usize].next;
        }
        if c == NONE {
            return;
        }
        let next = self.elements[c as usize].next;
        match prev {
            NONE => self.elements[p as usize].first = next,
            prev => self.elements[prev as usize].next = next,
        }
        if self.elements[p as usize].last == id.0 {
            self.elements[p as usize].last = prev;
        }
        self.elements[id.0 as usize].next = NONE;
    }

    /// Moves `id` under `parent`, before `before` or last. The element
    /// keeps its own lead whitespace when it stays among siblings of the
    /// same depth, else takes its new neighbours'.
    pub fn move_to(&mut self, id: Id, parent: Id, before: Option<Id>) {
        let mut up = Some(parent);
        while let Some(p) = up {
            assert!(p != id, "move of an element into itself");
            up = self.parent(p);
        }
        if before == Some(id) {
            return;
        }
        let same_depth = self.depth(parent) + 1 == self.depth(id);
        self.remove(id);
        if !same_depth {
            self.elements[id.0 as usize].lead = self.sibling_lead(parent, before);
        }
        self.link(id.0, parent.0, before.map_or(NONE, |b| b.0));
    }

    fn depth(&self, mut id: Id) -> usize {
        let mut d = 0;
        while let Some(p) = self.parent(id) {
            (d, id) = (d + 1, p);
        }
        d
    }

    // The whitespace a new child of `parent` takes: its future sibling's,
    // else its new previous sibling's, else a line break.
    fn sibling_lead(&mut self, parent: Id, before: Option<Id>) -> Span {
        let from = before
            .or_else(|| self.children(parent).last())
            .map(|s| self.elements[s.0 as usize].lead)
            .filter(|l| self.raw(*l).iter().all(u8::is_ascii_whitespace));
        match from {
            Some(lead) => lead,
            None => self.push_bytes(self.newline.as_bytes()),
        }
    }

    fn copy(&mut self, src: &[Element], id: u32, parent: u32, bytes: u32, strs: u32) -> u32 {
        let shift = |s: Span, by: u32| Span {
            at: s.at + by,
            len: s.len,
        };
        let e = src[id as usize];
        let new = self.elements.len() as u32;
        self.elements.push(Element {
            lead: shift(e.lead, bytes),
            open: shift(e.open, bytes),
            tail: shift(e.tail, bytes),
            close: shift(e.close, bytes),
            name: shift(e.name, strs),
            text: shift(e.text, strs),
            ..Element::new(parent)
        });
        let mut c = e.first;
        while c != NONE {
            let child = self.copy(src, c, new, bytes, strs);
            self.link(child, new, NONE);
            c = src[c as usize].next;
        }
        new
    }

    fn link(&mut self, id: u32, parent: u32, before: u32) {
        self.elements[id as usize].parent = parent;
        if before == NONE {
            match self.elements[parent as usize].last {
                NONE => self.elements[parent as usize].first = id,
                last => self.elements[last as usize].next = id,
            }
            self.elements[parent as usize].last = id;
            self.elements[id as usize].next = NONE;
            return;
        }
        let mut prev = NONE;
        let mut c = self.elements[parent as usize].first;
        while c != NONE && c != before {
            prev = c;
            c = self.elements[c as usize].next;
        }
        assert!(c == before, "insert before an element of another parent");
        self.elements[id as usize].next = before;
        match prev {
            NONE => self.elements[parent as usize].first = id,
            prev => self.elements[prev as usize].next = id,
        }
    }
}

/// The child elements of one element, in file order.
pub struct Children<'a> {
    doc: &'a Doc,
    at: u32,
}

impl Iterator for Children<'_> {
    type Item = Id;

    fn next(&mut self) -> Option<Id> {
        (self.at != NONE).then(|| {
            let id = self.at;
            self.at = self.doc.elements[id as usize].next;
            Id(id)
        })
    }
}

/// `xml` with `prefix` before every element name, open and close: an
/// unprefixed fragment as a 2070 file spells it.
fn prefixed(xml: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return xml.to_owned();
    }
    let mut out = String::with_capacity(xml.len() + xml.len() / 8);
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..=lt]);
        rest = &rest[lt + 1..];
        if let Some(name) = rest.strip_prefix('/') {
            out.push('/');
            rest = name;
        }
        if rest.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            out.push_str(prefix);
        }
    }
    out.push_str(rest);
    out
}

/// Where a `CDATA[<u32 length><bytes>]` starting at `at` ends, past its `]`.
fn cdata_end(input: &[u8], at: usize) -> Option<usize> {
    let body = input.get(at..)?.strip_prefix(b"CDATA[")?;
    let len = u32::from_le_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let close = at + 10 + len;
    (input.get(close) == Some(&b']')).then_some(close + 1)
}

fn find(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    hay[from..]
        .iter()
        .position(|&c| c == needle)
        .map(|k| from + k)
}

fn push_text(acc: &mut String, raw: &[u8]) {
    let s = String::from_utf8_lossy(raw);
    let t = s.trim();
    if !t.is_empty() {
        acc.push_str(&decode_entities(t));
    }
}

/// `&lt;Default Material&gt;` and friends.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn encode_entities(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// First `<Tag>value</>` (Anno's short-closed XML) or `<Tag>value</Tag>`.
pub fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let rest = &xml[xml.find(&open)? + open.len()..];
    rest.find("</").map(|end| rest[..end].trim())
}

/// The text between `<name>` and its own closing `</>`, nested tags and
/// all; ambient presets close every tag with a bare `</>`.
pub fn block<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = xml.find(&open)? + open.len();
    let rest = &xml[start..];
    let mut depth = 1;
    let mut i = 0;
    while let Some(at) = rest[i..].find('<') {
        let at = i + at;
        if rest[at..].starts_with("</") {
            depth -= 1;
            if depth == 0 {
                return Some(&rest[..at]);
            }
        } else {
            depth += 1;
        }
        i = at + 1;
    }
    None
}

/// The whole text of each `<Config>…</Config>` in the first `<name>`
/// list, in order: a cfg's `Decals`, `Particles` or `Lights` entries as
/// the file holds them, for a writer that copies them whole.
pub fn configs<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let Some(list) = block(xml, name) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = list;
    while let Some(start) = rest.find("<Config>") {
        let Some(inner) = block(&rest[start..], "Config") else {
            break;
        };
        // `inner` ends where its closing tag starts; take that tag too.
        let close = start + "<Config>".len() + inner.len();
        let end = rest[close..]
            .find('>')
            .map_or(rest.len(), |gt| close + gt + 1);
        out.push(&rest[start..end]);
        rest = &rest[end..];
    }
    out
}

/// Replaces the content of every leaf `<name>…</name>` (or `…</>`) in
/// `xml`.
pub fn set(xml: &str, name: &str, value: &str) -> String {
    let open = format!("<{name}>");
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let body = &rest[i + open.len()..];
        let Some(j) = body.find("</") else { break };
        out.push_str(&rest[..i + open.len()]);
        out.push_str(value);
        rest = &body[j..];
    }
    out.push_str(rest);
    out
}

/// Sets `<name><x>…</x><y>…</y><z>…</z></name>`, when present.
pub fn set_point(xml: &str, name: &str, p: [f32; 3]) -> String {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let (Some(a), Some(b)) = (xml.find(&open), xml.find(&close)) else {
        return xml.to_owned();
    };
    let mut body = xml[a..b].to_owned();
    for (axis, v) in ["x", "y", "z"].iter().zip(p) {
        body = set(&body, axis, &v.to_string());
    }
    format!("{}{body}{}", &xml[..a], &xml[b..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::{FileId, Install};

    fn round_trip(input: &[u8]) -> Doc {
        let doc = Doc::parse(input);
        assert_eq!(
            String::from_utf8_lossy(&doc.write()),
            String::from_utf8_lossy(input)
        );
        doc
    }

    // Everything a shipped file throws at the reader: both closer styles,
    // a self-closing tag, an entity, a stray closer, a garbage byte, binary
    // holding '<' and '>', and a file that ends mid-element.
    #[test]
    fn a_doc_writes_back_every_byte() {
        let mut bin = b"<Root>\r\n  <T>CDATA[".to_vec();
        bin.extend_from_slice(&4u32.to_le_bytes());
        bin.extend_from_slice(b"<a>\xff");
        bin.extend_from_slice(
            b"]</>\r\n  <N>&lt;Default&gt;</N>\r\n  <E/>\xfe\r\n</>\r\n</>\r\n<Open><x>1</x>tail <",
        );
        let doc = round_trip(&bin);
        let root = doc.child(Id::ROOT, "Root").unwrap();
        let names: Vec<_> = doc.children(root).map(|c| doc.name(c)).collect();
        assert_eq!(names, ["T", "N", "E"]);
        let t = doc.child(root, "T").unwrap();
        assert_eq!(doc.cdata(t), Some(&b"<a>\xff"[..]));
        assert_eq!(doc.text(t), "");
        assert_eq!(doc.text(doc.child(root, "N").unwrap()), "<Default>");
        let open = doc.child(Id::ROOT, "Open").unwrap();
        assert_eq!(doc.text(doc.child(open, "x").unwrap()), "1");
        assert_eq!(doc.text(open), "tail");
        round_trip(b"");
        round_trip(b"no tags at all");
        round_trip(b"</></>");
    }

    #[test]
    fn set_text_changes_only_the_value() {
        let mut doc = Doc::parse(b"<A>\r\n<B> 1 </>\r\n<C/>\r\n</>");
        let a = doc.child(Id::ROOT, "A").unwrap();
        let b = doc.child(a, "B").unwrap();
        doc.set_text(b, "x<y");
        let c = doc.child(a, "C").unwrap();
        doc.set_text(c, "2");
        assert_eq!(doc.text(b), "x<y");
        assert_eq!(doc.write(), b"<A>\r\n<B> x&lt;y </>\r\n<C>2</>\r\n</>");
        let d = doc.set_child_text(a, "D", "3");
        assert_eq!(doc.text(d), "3");
        assert_eq!(
            doc.write(),
            b"<A>\r\n<B> x&lt;y </>\r\n<C>2</>\r\n<D>3</>\r\n</>"
        );
        doc.set_cdata(d, &[1, 2]);
        assert_eq!(doc.cdata(d), Some(&[1u8, 2][..]));
        assert_eq!(Doc::parse(&doc.write()).cdata(d), Some(&[1u8, 2][..]));
    }

    #[test]
    fn structural_edits_keep_the_files_layout() {
        let src = "<Config>\r\n  <List>\r\n    <Config><N>a</N></Config>\r\n    <Config><N>b</N></Config>\r\n  </List>\r\n  <Other>\r\n  </Other>\r\n</Config>";
        let mut doc = Doc::parse(src.as_bytes());
        let top = doc.child(Id::ROOT, "Config").unwrap();
        let list = doc.child(top, "List").unwrap();
        let [a, b] = doc.children(list).collect::<Vec<_>>()[..] else {
            panic!()
        };
        let copy = doc.duplicate(a);
        let n = doc.child(copy, "N").unwrap();
        doc.set_text(n, "c");
        assert_eq!(
            doc.write(),
            src.replace(
                "<N>a</N></Config>",
                "<N>a</N></Config>\r\n    <Config><N>c</N></Config>"
            )
            .as_bytes()
        );
        doc.remove(a);
        assert!(!doc.is_attached(a) && doc.is_attached(b));
        assert!(
            !Doc::parse(src.as_bytes()).is_attached(copy),
            "an id from a later doc"
        );
        doc.move_to(b, list, Some(copy));
        let other = doc.child(top, "Other").unwrap();
        // New elements take the file's line breaks.
        let added = doc.insert(other, None, "<Config>\n<N>d</>\n</>");
        assert_eq!(doc.text(doc.child(added, "N").unwrap()), "d");
        let out = String::from_utf8(doc.write()).unwrap();
        assert_eq!(
            out,
            "<Config>\r\n  <List>\r\n    <Config><N>b</N></Config>\r\n    <Config><N>c</N></Config>\r\n  </List>\r\n  <Other>\r\n<Config>\r\n<N>d</>\r\n</>\r\n  </Other>\r\n</Config>"
        );
        // An element moved to another depth takes its new siblings' lead.
        doc.move_to(b, other, None);
        assert_eq!(doc.parent(b), Some(other));
        assert!(doc.children(other).any(|c| c == b));
    }

    // 2070's files prefix every element: names read without it, and what
    // the doc writes gets it, closers too.
    #[test]
    fn a_prefixed_file_reads_and_writes_in_its_spelling() {
        let src = "<m_Config><m_Transformer><m_Config><m_Position.x>1</m_Position.x></m_Config></m_Transformer></m_Config>";
        let mut doc = Doc::parse(src.as_bytes());
        assert_eq!(doc.prefix(), "m_");
        let top = doc.child(Id::ROOT, "Config").unwrap();
        let t = doc.child(top, "Transformer").unwrap();
        let c = doc.child(t, "Config").unwrap();
        assert_eq!(doc.text(doc.child(c, "Position.x").unwrap()), "1");
        doc.set_child_text(c, "Position.y", "2");
        assert_eq!(
            String::from_utf8(doc.write()).unwrap(),
            src.replace(
                "1</m_Position.x>",
                "1</m_Position.x><m_Position.y>2</m_Position.y>"
            )
        );
        assert_eq!(Doc::parse(b"<Config/>").prefix(), "");
    }

    #[test]
    #[should_panic(expected = "into itself")]
    fn an_element_cannot_move_into_itself() {
        let mut doc = Doc::parse(b"<A><B></B></A>");
        let a = doc.child(Id::ROOT, "A").unwrap();
        let b = doc.child(a, "B").unwrap();
        doc.move_to(a, b, None);
    }

    // Every engine-XML file the model editor opens writes back unchanged.
    #[test]
    fn shipped_files_write_back_unchanged() {
        for var in ["ANNO117_DIR", "ANNO1800_DIR"] {
            let Some(dir) = crate::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let install = Install::open(dir, Install::DATA_FIRST).unwrap();
            let (mut files, mut cdata) = (0, 0);
            for i in 0..install.file_count() {
                let path = install.file_path(FileId(i as u32));
                if !["cfg", "prp", "rdp"].iter().any(|e| path.ends_with(e)) {
                    continue;
                }
                let bytes = install.read(&path).unwrap();
                let doc = Doc::parse(&bytes);
                assert!(doc.write() == bytes, "{var}: {path} changed");
                files += 1;
                cdata += path.ends_with(".rdp") as usize;
            }
            eprintln!("{var}: {files} files, {cdata} rdp");
            assert!(files > 1000, "{var}: only {files} files");
        }
    }

    #[test]
    fn set_replaces_every_tag_body() {
        assert_eq!(
            set("<a>1</a><b>2</b><a>3</a>", "a", "x"),
            "<a>x</a><b>2</b><a>x</a>"
        );
        assert_eq!(set("<a>1</a>", "c", "x"), "<a>1</a>");
        assert_eq!(
            set_point("<P><x>0</x><y>0</y><z>0</z></P>", "P", [1., 2.5, -3.]),
            "<P><x>1</x><y>2.5</y><z>-3</z></P>"
        );
    }

    #[test]
    fn reads_short_closed_xml() {
        let xml = "<Material><ID>7</>\n<FileName>a/b.xml</>\n<Repetition>4.0</Repetition></>";
        assert_eq!(tag(xml, "ID"), Some("7"));
        assert_eq!(tag(xml, "FileName"), Some("a/b.xml"));
        assert_eq!(tag(xml, "Repetition"), Some("4.0"));
        assert_eq!(tag(xml, "Missing"), None);
    }

    #[test]
    fn a_block_holds_its_nested_tags_up_to_its_own_close() {
        let xml = "<Ambient><Atmosphere><Scale>10</><Rayleigh><x>1</></></><After>2</></>";
        assert_eq!(
            block(xml, "Atmosphere"),
            Some("<Scale>10</><Rayleigh><x>1</></>")
        );
        assert_eq!(block(xml, "Missing"), None);
    }

    #[test]
    fn configs_are_each_list_entry_whole() {
        let xml = "<Config><Models><Config><A>1</A></Config></Models>\
<Decals><Config><T><Config><P>1</P></Config></T><Common [X]></Common [X]></Config>\n\
<Config><N>2</></>\n</Decals></Config>";
        assert_eq!(
            configs(xml, "Decals"),
            [
                "<Config><T><Config><P>1</P></Config></T><Common [X]></Common [X]></Config>",
                "<Config><N>2</></>"
            ]
        );
        assert!(configs(xml, "Lights").is_empty());
    }
}
