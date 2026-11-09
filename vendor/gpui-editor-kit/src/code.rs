// Owns read-only source views: XML, Lua and JSON tokenizing, and a
// line-numbered, virtualized view that colours the tokens, with mouse
// selection, copy, find and a context menu. For previews; no editing here.
use crate::{
    field::{EditEvent, Filter, TextEdit, text_field},
    theme, widgets,
};
use gpui::{prelude::*, *};
use std::{ops::Range, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Plain,
    Xml,
    Lua,
    Json,
}

impl Language {
    /// By extension; other text that opens with a tag is XML too (game
    /// data often hides XML behind its own extensions).
    pub fn guess(path: &str, text: &str) -> Self {
        match path.rsplit('.').next().unwrap_or_default() {
            "lua" => Self::Lua,
            "json" => Self::Json,
            "xml" | "cfg" | "ifo" | "fc" => Self::Xml,
            _ if text
                .trim_start_matches('\u{feff}')
                .trim_start()
                .starts_with('<') =>
            {
                Self::Xml
            }
            _ => Self::Plain,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// An XML element name.
    Tag,
    Attribute,
    String,
    Number,
    Comment,
    /// A Lua keyword; `nil`, `true` and `false` are numbers' colour.
    Keyword,
    /// A Lua name called: `print(`, `obj:method(`.
    Function,
    /// XML entities, CDATA and the `<?xml ?>` declaration.
    Entity,
    /// A JSON object key.
    Key,
    /// A doc-comment tag (LuaLS `@param`): above comment prose, below code.
    Doc,
    /// XML brackets, `/` and `=`: dimmer than the names they frame.
    Punct,
}

impl Token {
    fn color(self) -> Rgba {
        let s = &theme::get().syntax;
        match self {
            Self::Tag => s.tag,
            Self::Attribute => s.attribute,
            Self::String => s.string,
            Self::Number => s.number,
            Self::Comment => s.comment,
            Self::Keyword => s.keyword,
            Self::Function => s.function,
            Self::Entity => s.entity,
            Self::Key => s.key,
            Self::Doc => s.doc,
            Self::Punct => s.punct,
        }
    }
}

/// A place in the text: a line and a byte offset into it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: u32,
    pub byte: u32,
}

/// A token's byte range in the text; sorted and disjoint.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u32,
    end: u32,
    token: Token,
}

/// Source text ready to view: tabs expanded, line starts and token spans
/// found once (off the UI thread, as the host builds it), so drawing a
/// line only slices.
pub struct Code {
    text: String,
    /// Byte offset of each line's start.
    lines: Vec<u32>,
    spans: Vec<Span>,
    /// The line with the most characters, measured for the view's width.
    longest: usize,
}

impl Code {
    pub fn new(text: &str, language: Language) -> Self {
        let text = expand_tabs(text);
        let mut lines = vec![0u32];
        let (mut longest, mut most, mut chars) = (0, 0, 0);
        for (i, c) in text.char_indices() {
            if c == '\n' {
                if chars > most {
                    (longest, most) = (lines.len() - 1, chars);
                }
                chars = 0;
                lines.push(i as u32 + 1);
            } else {
                chars += 1;
            }
        }
        if chars > most {
            longest = lines.len() - 1;
        }
        let spans = match language {
            Language::Plain => Vec::new(),
            Language::Xml => xml(&text),
            Language::Lua => lua(&text),
            Language::Json => json(&text),
        };
        Self {
            text,
            lines,
            spans,
            longest,
        }
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line(&self, i: usize) -> Range<usize> {
        let start = self.lines[i] as usize;
        let end = self
            .lines
            .get(i + 1)
            .map_or(self.text.len(), |&e| e as usize);
        let s = &self.text[start..end];
        start..start + s.trim_end_matches(['\n', '\r']).len()
    }

    /// Line `i` and its tokens, relative to the line.
    fn styled(&self, i: usize) -> (SharedString, Vec<(Range<usize>, HighlightStyle)>) {
        let line = self.line(i);
        let first = self
            .spans
            .partition_point(|s| (s.end as usize) <= line.start);
        let runs = self.spans[first..]
            .iter()
            .take_while(|s| (s.start as usize) < line.end)
            .map(|s| {
                let a = (s.start as usize).max(line.start) - line.start;
                let b = (s.end as usize).min(line.end) - line.start;
                (
                    a..b,
                    HighlightStyle {
                        color: Some(s.token.color().into()),
                        ..Default::default()
                    },
                )
            })
            .collect();
        (self.text[line].to_owned().into(), runs)
    }

    fn line_text(&self, i: usize) -> &str {
        &self.text[self.line(i)]
    }

    /// The position `col` characters into line `line`, clamped to its end.
    fn at_col(&self, line: usize, col: usize) -> Pos {
        let text = self.line_text(line);
        let byte = text.char_indices().nth(col).map_or(text.len(), |(b, _)| b);
        Pos {
            line: line as u32,
            byte: byte as u32,
        }
    }

    fn col(&self, p: Pos) -> usize {
        self.line_text(p.line as usize)[..p.byte as usize]
            .chars()
            .count()
    }

    fn offset(&self, p: Pos) -> usize {
        self.lines[p.line as usize] as usize + p.byte as usize
    }

    /// The text from `a` to `b`, line breaks included. Tabs come out as the
    /// spaces the view shows.
    pub fn text_between(&self, a: Pos, b: Pos) -> &str {
        let (a, b) = (a.min(b), a.max(b));
        &self.text[self.offset(a)..self.offset(b)]
    }

    pub fn end(&self) -> Pos {
        let last = self.line_count() - 1;
        Pos {
            line: last as u32,
            byte: self.line(last).len() as u32,
        }
    }

    /// The word (letters, digits, `_`) around `p`, else the one character.
    fn word_at(&self, p: Pos) -> (Pos, Pos) {
        let text = self.line_text(p.line as usize);
        let word = |c: char| c.is_alphanumeric() || c == '_';
        let at = p.byte as usize;
        let start = text[..at]
            .char_indices()
            .rev()
            .take_while(|&(_, c)| word(c))
            .last()
            .map_or(at, |(i, _)| i);
        let mut end = text[at..]
            .char_indices()
            .find(|&(_, c)| !word(c))
            .map_or(text.len(), |(i, _)| at + i);
        if start == end {
            end = text[at..].chars().next().map_or(at, |c| at + c.len_utf8());
        }
        let pos = |byte: usize| Pos {
            line: p.line,
            byte: byte as u32,
        };
        (pos(start), pos(end))
    }

    fn pos_of(&self, offset: usize) -> Pos {
        let line = self.lines.partition_point(|&s| s as usize <= offset) - 1;
        Pos {
            line: line as u32,
            byte: (offset - self.lines[line] as usize) as u32,
        }
    }

    /// Where `query` occurs, ignoring ASCII case, at most `MATCHES` times.
    pub fn find_all(&self, query: &str) -> Vec<(Pos, Pos)> {
        if query.is_empty() {
            return Vec::new();
        }
        let (hay, needle) = (self.text.to_ascii_lowercase(), query.to_ascii_lowercase());
        hay.match_indices(&needle)
            .take(MATCHES)
            .map(|(at, m)| (self.pos_of(at), self.pos_of(at + m.len())))
            .collect()
    }

    /// Tokens as ranges, for tests and hosts that colour text themselves.
    pub fn tokens(&self) -> impl Iterator<Item = (&str, Token)> {
        self.spans
            .iter()
            .map(|s| (&self.text[s.start as usize..s.end as usize], s.token))
    }
}

/// Tabs to spaces at 4-column stops; GPUI draws a tab as a narrow box.
fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + text.len() / 8);
    let mut col = 0;
    for c in text.chars() {
        match c {
            '\t' => {
                let n = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\n' => {
                out.push(c);
                col = 0;
            }
            _ => {
                out.push(c);
                col += 1;
            }
        }
    }
    out
}

fn push(spans: &mut Vec<Span>, range: Range<usize>, token: Token) {
    if range.start < range.end {
        spans.push(Span {
            start: range.start as u32,
            end: range.end as u32,
            token,
        });
    }
}

/// Where `needle` next starts at or after `from`, else the text's end.
fn find(text: &str, from: usize, needle: &str) -> Option<usize> {
    text.get(from..)?.find(needle).map(|i| from + i)
}

fn is_name(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b':') || b >= 0x80
}

fn xml(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let n = b.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < n {
        let rest = &text[i..];
        if rest.starts_with("<!--") {
            let end = find(text, i + 4, "-->").map_or(n, |e| e + 3);
            push(&mut spans, i..end, Token::Comment);
            i = end;
        } else if rest.starts_with("<![CDATA[") {
            let end = find(text, i + 9, "]]>").map_or(n, |e| e + 3);
            push(&mut spans, i..end, Token::Entity);
            i = end;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            let end = find(text, i + 2, ">").map_or(n, |e| e + 1);
            push(&mut spans, i..end, Token::Entity);
            i = end;
        } else if b[i] == b'<' {
            let mut j = i + 1;
            if j < n && b[j] == b'/' {
                j += 1;
            }
            push(&mut spans, i..j, Token::Punct);
            let name = j;
            while j < n && is_name(b[j]) {
                j += 1;
            }
            push(&mut spans, name..j, Token::Tag);
            // Attributes up to the closing bracket.
            while j < n && b[j] != b'>' {
                match b[j] {
                    b'"' | b'\'' => {
                        let q = b[j];
                        let end = b[j + 1..]
                            .iter()
                            .position(|&c| c == q)
                            .map_or(n, |e| j + 1 + e + 1);
                        push(&mut spans, j..end, Token::String);
                        j = end;
                    }
                    b'=' | b'/' => {
                        push(&mut spans, j..j + 1, Token::Punct);
                        j += 1;
                    }
                    c if is_name(c) => {
                        let start = j;
                        while j < n && is_name(b[j]) {
                            j += 1;
                        }
                        push(&mut spans, start..j, Token::Attribute);
                    }
                    // A stray `<` means the tag never closed: start over there.
                    b'<' => break,
                    _ => j += 1,
                }
            }
            if j < n && b[j] == b'>' {
                push(&mut spans, j..j + 1, Token::Punct);
                j += 1;
            }
            i = j;
        } else if b[i] == b'&' {
            let end = b[i..n.min(i + 12)]
                .iter()
                .position(|&c| c == b';')
                .map_or(i + 1, |e| i + e + 1);
            push(&mut spans, i..end, Token::Entity);
            i = end;
        } else {
            // Element text up to the next markup. Anno's data is mostly
            // numbers in elements (`<GUID>1500001</GUID>`): those count as
            // numbers, words stay plain.
            let end = b[i..]
                .iter()
                .position(|&c| c == b'<' || c == b'&')
                .map_or(n, |e| i + e);
            let body = &text[i..end];
            let trimmed = body.trim();
            if !trimmed.is_empty() && trimmed.parse::<f64>().is_ok() {
                let lead = body.len() - body.trim_start().len();
                push(
                    &mut spans,
                    i + lead..i + lead + trimmed.len(),
                    Token::Number,
                );
            }
            i = end;
        }
    }
    spans
}

const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "for", "function", "goto", "if", "in", "local",
    "not", "or", "repeat", "return", "then", "until", "while",
];

/// The `]==]` that closes a long bracket opened at `i` (`[==[`), if `i`
/// opens one: the end of the whole bracket.
fn long_bracket(text: &str, i: usize) -> Option<usize> {
    let b = text.as_bytes();
    if b.get(i) != Some(&b'[') {
        return None;
    }
    let level = b[i + 1..].iter().take_while(|&&c| c == b'=').count();
    if b.get(i + 1 + level) != Some(&b'[') {
        return None;
    }
    let close = format!("]{}]", "=".repeat(level));
    Some(find(text, i + level + 2, &close).map_or(text.len(), |e| e + close.len()))
}

fn lua(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let n = b.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if text[i..].starts_with("---@") {
            // A LuaLS annotation: the tag in a muted doc colour; after
            // @param and @field the name too, the type after it as comment.
            // Other tags start with a type (@return, @class), which stays
            // comment grey, so one type has one colour whichever tag it
            // follows. With the word in the name orange, generated stubs
            // outshouted the `function … end` they describe.
            let end = find(text, i, "\n").unwrap_or(n);
            let tag = i
                + 3
                + b[i + 3..end]
                    .iter()
                    .take_while(|c| !c.is_ascii_whitespace())
                    .count();
            push(&mut spans, i..i + 3, Token::Comment);
            push(&mut spans, i + 3..tag, Token::Doc);
            let rest = &text[tag..end];
            let gap = rest.len() - rest.trim_start().len();
            let word = rest
                .trim_start()
                .find(char::is_whitespace)
                .unwrap_or(rest.trim_start().len());
            let named = matches!(&text[i + 3..tag], "@param" | "@field");
            let name_end = if named { tag + gap + word } else { tag };
            push(&mut spans, tag..name_end, Token::Doc);
            push(&mut spans, name_end..end, Token::Comment);
            i = end;
        } else if text[i..].starts_with("--") {
            let end = long_bracket(text, i + 2).unwrap_or_else(|| find(text, i, "\n").unwrap_or(n));
            push(&mut spans, i..end, Token::Comment);
            i = end;
        } else if let Some(end) = long_bracket(text, i) {
            push(&mut spans, i..end, Token::String);
            i = end;
        } else if c == b'"' || c == b'\'' {
            let mut j = i + 1;
            while j < n && b[j] != c && b[j] != b'\n' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(n);
            push(&mut spans, i..end, Token::String);
            i = end;
        } else if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            let mut j = i + 1;
            while j < n {
                let d = b[j];
                // Exponent signs: 1e-5, 0x1p+4.
                let sign =
                    matches!(d, b'+' | b'-') && matches!(b[j - 1], b'e' | b'E' | b'p' | b'P');
                if !(d.is_ascii_alphanumeric() || d == b'.' || sign) {
                    break;
                }
                j += 1;
            }
            push(&mut spans, i..j, Token::Number);
            i = j;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = i + 1;
            while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let word = &text[i..j];
            let next = b[j..].iter().find(|c| !c.is_ascii_whitespace()).copied();
            let token = if matches!(word, "nil" | "true" | "false") {
                Some(Token::Number)
            } else if LUA_KEYWORDS.contains(&word) {
                Some(Token::Keyword)
            } else if matches!(next, Some(b'(' | b'"' | b'{')) {
                Some(Token::Function)
            } else {
                None
            };
            if let Some(token) = token {
                push(&mut spans, i..j, token);
            }
            i = j;
        } else {
            // One character, whole: names and strings above are ASCII-led,
            // so anything else is skipped by its UTF-8 length.
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    spans
}

fn json(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let n = b.len();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'"' {
            let mut j = i + 1;
            while j < n && b[j] != b'"' && b[j] != b'\n' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(n);
            // A key is a string a colon follows.
            let key = b[end..].iter().find(|c| !c.is_ascii_whitespace()) == Some(&b':');
            push(
                &mut spans,
                i..end,
                if key { Token::Key } else { Token::String },
            );
            i = end;
        } else if c == b'-' || c.is_ascii_digit() {
            let mut j = i + 1;
            while j < n
                && (b[j].is_ascii_digit() || matches!(b[j], b'.' | b'e' | b'E' | b'+' | b'-'))
            {
                j += 1;
            }
            push(&mut spans, i..j, Token::Number);
            i = j;
        } else if text[i..].starts_with("//") {
            // Comments are not JSON, but configs written by hand carry them.
            let end = find(text, i, "\n").unwrap_or(n);
            push(&mut spans, i..end, Token::Comment);
            i = end;
        } else if text[i..].starts_with("/*") {
            let end = find(text, i + 2, "*/").map_or(n, |e| e + 2);
            push(&mut spans, i..end, Token::Comment);
            i = end;
        } else if c.is_ascii_alphabetic() {
            let mut j = i + 1;
            while j < n && b[j].is_ascii_alphanumeric() {
                j += 1;
            }
            if matches!(&text[i..j], "true" | "false" | "null") {
                push(&mut spans, i..j, Token::Number);
            }
            i = j;
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    spans
}

/// Matches a find stops counting at: past it the count reads "10,000+".
const MATCHES: usize = 10_000;

/// An open find bar: the query being typed and where it occurs.
struct Find {
    edit: TextEdit<()>,
    matches: Vec<(Pos, Pos)>,
    current: usize,
}

/// The view's selection and menu, kept per view id across frames.
struct Selection {
    /// Which `Code` it belongs to (its address): a new file starts clean.
    code: usize,
    focus: FocusHandle,
    anchor: Pos,
    head: Pos,
    dragging: bool,
    /// Where the context menu opened, and the line under it.
    menu: Option<(Point<Pixels>, usize)>,
    find: Option<Find>,
}

impl Selection {
    fn range(&self) -> Option<(Pos, Pos)> {
        (self.anchor != self.head).then(|| (self.anchor.min(self.head), self.anchor.max(self.head)))
    }
}

/// A read-only view of `code`, filling its parent: line numbers, coloured
/// tokens, long lines scrolled sideways rather than wrapped. Only the lines
/// on screen are laid out, so a 4 MB file scrolls as a short one.
pub fn code_view(
    id: impl Into<SharedString>,
    code: Arc<Code>,
    caption: Option<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> Stateful<Div> {
    const LINE: f32 = 16.;
    const SIZE: f32 = 11.;
    let id: SharedString = id.into();
    let scroll = window
        .use_keyed_state(id.clone(), cx, |_, _| UniformListScrollHandle::new())
        .read(cx)
        .clone();
    let base = scroll.0.borrow().base_handle.clone();
    let key = Arc::as_ptr(&code) as usize;
    let state = window.use_keyed_state(
        SharedString::from(format!("{id}-selection")),
        cx,
        |_, cx| Selection {
            code: key,
            focus: cx.focus_handle(),
            anchor: Pos::default(),
            head: Pos::default(),
            dragging: false,
            menu: None,
            find: None,
        },
    );
    if state.read(cx).code != key {
        state.update(cx, |s, _| {
            s.code = key;
            s.anchor = Pos::default();
            s.head = Pos::default();
            s.menu = None;
            // A find stays open across files, searched again in the new one.
            if let Some(f) = &mut s.find {
                f.matches = code.find_all(&f.edit.text);
                f.current = 0;
            }
        });
    }
    let (selected, menu, focus) = {
        let s = state.read(cx);
        (s.range(), s.menu, s.focus.clone())
    };
    // Every match, for the rows' marks; the current one is the selection.
    let found: Rc<[(Pos, Pos)]> = state
        .read(cx)
        .find
        .as_ref()
        .map_or_else(|| Rc::from([]), |f| f.matches.as_slice().into());
    // One advance for every character: the font is monospace, so columns
    // map to pixels without laying the line out.
    let cw = {
        let ts = window.text_system();
        let id = ts.resolve_font(&font(theme::MONO_FONT));
        ts.advance(id, px(SIZE), 'm')
            .map_or(6.6, |s| f32::from(s.width))
    };
    let digits = code.line_count().to_string().len() as f32;
    let gutter = digits * cw + 16.;
    // Window position to text position, through the list's scroll.
    let hit = {
        let (base, code) = (base.clone(), code.clone());
        move |p: Point<Pixels>| {
            let (b, o) = (base.bounds(), base.offset());
            let y = f32::from(p.y - b.origin.y - o.y);
            let line = ((y / LINE).floor().max(0.) as usize).min(code.line_count() - 1);
            let x = f32::from(p.x - b.origin.x - o.x) - gutter;
            code.at_col(line, (x / cw).round().max(0.) as usize)
        }
    };
    let rows = code.clone();
    let list = uniform_list(id.clone(), code.line_count(), move |range, _, _| {
        range
            .map(|i| {
                let (text, runs) = rows.styled(i);
                // Find matches on this line, as column bands under the text.
                let line = i as u32;
                let first = found.partition_point(|(a, _)| a.line < line);
                let marks: Vec<(f32, f32)> = found[first..]
                    .iter()
                    .take_while(|(a, _)| a.line == line)
                    .map(|&(a, b)| {
                        let c0 = rows.col(a);
                        let c1 = if b.line == line { rows.col(b) } else { c0 + 1 };
                        (gutter + c0 as f32 * cw, (c1 - c0) as f32 * cw)
                    })
                    .collect();
                // The selected part of this line, in columns; past the end
                // of a line it continues from, a sliver for the break.
                let band = selected.and_then(|(a, b)| {
                    let line = i as u32;
                    if line < a.line || line > b.line {
                        return None;
                    }
                    let c0 = if line == a.line { rows.col(a) } else { 0 };
                    let c1 = if line == b.line {
                        rows.col(b)
                    } else {
                        rows.col(Pos {
                            line,
                            byte: rows.line(i).len() as u32,
                        })
                    };
                    let tail = if line < b.line { cw * 0.6 } else { 0. };
                    let w = (c1 - c0) as f32 * cw + tail;
                    (w > 0.).then(|| (gutter + c0 as f32 * cw, w))
                });
                div()
                    .relative()
                    .flex()
                    .h(px(LINE))
                    .items_center()
                    .whitespace_nowrap()
                    .children(marks.into_iter().map(|(left, w)| {
                        div()
                            .absolute()
                            .top(px(1.))
                            .bottom(px(1.))
                            .left(px(left))
                            .w(px(w))
                            .rounded(px(2.))
                            .bg(theme::get().highlight.alpha(0.25))
                    }))
                    .children(band.map(|(left, w)| {
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(left))
                            .w(px(w))
                            .bg(theme::get().selected)
                    }))
                    .child(
                        div()
                            .w(px(gutter))
                            .flex_none()
                            .pr(px(10.))
                            .text_right()
                            .text_color(theme::get().disabled)
                            .child((i + 1).to_string()),
                    )
                    .child(StyledText::new(text).with_highlights(runs))
                    .pr_4()
                    .into_any_element()
            })
            .collect()
    })
    .with_width_from_item(Some(code.longest))
    .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
    .track_scroll(&scroll)
    .absolute()
    .inset_0()
    .cursor(CursorStyle::IBeam)
    .font_family(theme::MONO_FONT)
    .text_size(px(SIZE))
    .text_color(theme::get().text);

    let copy = {
        let (state, code) = (state.clone(), code.clone());
        move |cx: &mut App| {
            if let Some((a, b)) = state.read(cx).range() {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    code.text_between(a, b).to_owned(),
                ));
            }
        }
    };
    let select_all = {
        let (state, code) = (state.clone(), code.clone());
        move |cx: &mut App| {
            state.update(cx, |s, cx| {
                s.anchor = Pos::default();
                s.head = code.end();
                cx.notify();
            })
        }
    };
    // To the next (or previous) match: selected and scrolled to. The first
    // step after typing lands on the first match from the top.
    let step: Rc<dyn Fn(&mut App, bool)> = {
        let (state, scroll) = (state.clone(), scroll.clone());
        Rc::new(move |cx: &mut App, forward: bool| {
            state.update(cx, |s, cx| {
                let selected = s.range();
                let Some(f) = &mut s.find else { return };
                if f.matches.is_empty() {
                    return;
                }
                let n = f.matches.len();
                let on_match = selected == Some(f.matches[f.current]);
                if on_match {
                    f.current = if forward {
                        (f.current + 1) % n
                    } else {
                        (f.current + n - 1) % n
                    };
                }
                let (a, b) = f.matches[f.current];
                (s.anchor, s.head) = (a, b);
                scroll.scroll_to_item(a.line as usize, ScrollStrategy::Center);
                cx.notify();
            })
        })
    };
    let mut view = div()
        .id(ElementId::Name(format!("{id}-view").into()))
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .rounded(px(3.))
        .bg(theme::get().bg)
        .track_focus(&focus)
        .child(list)
        .child(widgets::scrollbar(id.clone(), &base))
        .on_key_down({
            let (copy, select_all, state, step, code) = (
                copy.clone(),
                select_all.clone(),
                state.clone(),
                step.clone(),
                code.clone(),
            );
            move |e: &KeyDownEvent, _, cx| {
                let k = &e.keystroke;
                let command = k.modifiers.control || k.modifiers.platform;
                let finding = state.read(cx).find.is_some();
                match (command, k.key.as_str()) {
                    (true, "c") => copy(cx),
                    (true, "a") => select_all(cx),
                    // Opens on the selected text when it is one line.
                    (true, "f") => state.update(cx, |s, cx| {
                        let seed = s
                            .range()
                            .filter(|(a, b)| a.line == b.line)
                            .map(|(a, b)| code.text_between(a, b).to_owned());
                        let find = s.find.get_or_insert_with(|| Find {
                            edit: TextEdit::new((), "", Filter::Text),
                            matches: Vec::new(),
                            current: 0,
                        });
                        if let Some(seed) = seed {
                            find.edit = TextEdit::new((), seed, Filter::Text);
                            find.matches = code.find_all(&find.edit.text);
                            find.current = 0;
                        }
                        cx.notify();
                    }),
                    (false, "enter") if finding => step(cx, !k.modifiers.shift),
                    (false, "escape") if finding => state.update(cx, |s, cx| {
                        s.find = None;
                        cx.notify();
                    }),
                    _ if finding => {
                        let changed = state.update(cx, |s, cx| {
                            let f = s.find.as_mut().expect("finding");
                            match f.edit.key_down_in(e, cx) {
                                EditEvent::Changed => {
                                    f.matches = code.find_all(&f.edit.text);
                                    f.current = 0;
                                    cx.notify();
                                    true
                                }
                                EditEvent::Ignored => false,
                                _ => {
                                    cx.notify();
                                    false
                                }
                            }
                        });
                        // The first match, as the query grows.
                        if changed {
                            step(cx, true);
                        }
                    }
                    (false, "escape") if state.read(cx).menu.is_some() => {
                        state.update(cx, |s, cx| {
                            s.menu = None;
                            cx.notify();
                        })
                    }
                    // Everything else goes on to the host (list keys).
                    _ => return,
                }
                cx.stop_propagation();
            }
        })
        .on_mouse_down(MouseButton::Left, {
            let (state, hit, code, focus) =
                (state.clone(), hit.clone(), code.clone(), focus.clone());
            move |e: &MouseDownEvent, window, cx| {
                window.focus(&focus, cx);
                let at = hit(e.position);
                state.update(cx, |s, cx| {
                    s.menu = None;
                    match e.click_count {
                        // The word, then the whole line.
                        2 => (s.anchor, s.head) = code.word_at(at),
                        n if n >= 3 => {
                            s.anchor = Pos {
                                line: at.line,
                                byte: 0,
                            };
                            s.head = code.at_col(at.line as usize, usize::MAX);
                        }
                        _ if e.modifiers.shift => s.head = at,
                        _ => (s.anchor, s.head) = (at, at),
                    }
                    s.dragging = e.click_count == 1;
                    cx.notify();
                });
            }
        })
        .on_mouse_move({
            let (state, hit) = (state.clone(), hit.clone());
            move |e: &MouseMoveEvent, _, cx| {
                if !state.read(cx).dragging {
                    return;
                }
                if e.pressed_button != Some(MouseButton::Left) {
                    state.update(cx, |s, _| s.dragging = false);
                    return;
                }
                // TODO(alex): scroll while dragging past the top or bottom.
                let at = hit(e.position);
                state.update(cx, |s, cx| {
                    if s.head != at {
                        s.head = at;
                        cx.notify();
                    }
                });
            }
        })
        .on_mouse_up(MouseButton::Left, {
            let state = state.clone();
            move |_, _, cx| state.update(cx, |s, _| s.dragging = false)
        })
        .on_mouse_down(MouseButton::Right, {
            let (state, hit, focus) = (state.clone(), hit.clone(), focus.clone());
            move |e: &MouseDownEvent, window, cx| {
                window.focus(&focus, cx);
                let line = hit(e.position).line as usize;
                state.update(cx, |s, cx| {
                    s.menu = Some((e.position, line));
                    cx.notify();
                });
            }
        });
    // The find bar, or a button that opens it (Ctrl F alone was a guess),
    // at the end of the caption row: floating over the code, it covered
    // the ends of long lines.
    let bar = match &state.read(cx).find {
        Some(f) => {
            let count = match f.matches.len() {
                0 if f.edit.text.is_empty() => String::new(),
                0 => "No matches".to_owned(),
                n if n >= MATCHES => format!("{} of 10,000+", f.current + 1),
                n => format!("{} of {n}", f.current + 1),
            };
            let button = |id: &'static str,
                          label: &'static str,
                          tip: &'static str,
                          on: Rc<dyn Fn(&mut App)>| {
                div()
                    .id(id)
                    .px_1()
                    .h(px(22.))
                    .flex()
                    .items_center()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_color(theme::get().muted)
                    .hover(|s| s.bg(theme::get().hover).text_color(theme::get().text))
                    .child(label)
                    .tooltip(widgets::Tooltip::text(tip, "", ""))
                    .on_click(move |_, _, cx| on(cx))
            };
            let close = {
                let state = state.clone();
                Rc::new(move |cx: &mut App| {
                    state.update(cx, |s, cx| {
                        s.find = None;
                        cx.notify();
                    })
                })
            };
            let (next, prev) = (step.clone(), step.clone());
            div()
                .id("code-find")
                .flex()
                .items_center()
                .gap_1()
                .text_size(px(11.))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    text_field(
                        "code-find-field",
                        &f.edit.text,
                        Some(f.edit.display()),
                        "Find in file",
                    )
                    .w(px(200.)),
                )
                .child(
                    div()
                        .min_w(px(72.))
                        .text_color(theme::get().muted)
                        .child(count),
                )
                .child(button(
                    "code-find-prev",
                    "↑",
                    "Previous match (Shift Enter)",
                    Rc::new(move |cx| prev(cx, false)),
                ))
                .child(button(
                    "code-find-next",
                    "↓",
                    "Next match (Enter)",
                    Rc::new(move |cx| next(cx, true)),
                ))
                .child(button("code-find-close", "×", "Close (Escape)", close))
                .into_any_element()
        }
        None => {
            let (state, focus) = (state.clone(), focus.clone());
            div()
                .id("code-find-open")
                .px_2()
                .h(px(22.))
                .flex()
                .items_center()
                .gap_2()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(theme::get().muted)
                .bg(theme::get().secondary)
                .border_1()
                .border_color(theme::get().border)
                .text_color(theme::get().text)
                .hover(|s| s.bg(theme::get().hover))
                .child("Find")
                .child(widgets::keycap("Ctrl F"))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    window.focus(&focus, cx);
                    state.update(cx, |s, cx| {
                        s.find = Some(Find {
                            edit: TextEdit::new((), "", Filter::Text),
                            matches: Vec::new(),
                            current: 0,
                        });
                        cx.notify();
                    })
                })
                .into_any_element()
        }
    };
    if let Some((at, line)) = menu {
        let close = {
            let state = state.clone();
            move |cx: &mut App| {
                state.update(cx, |s, cx| {
                    s.menu = None;
                    cx.notify();
                })
            }
        };
        let item = |label: SharedString, shortcut: &'static str, on: Rc<dyn Fn(&mut App)>| {
            let close = close.clone();
            widgets::menu_item(label, shortcut).on_click(move |_, _, cx| {
                on(cx);
                close(cx);
            })
        };
        let copy_line = {
            let code = code.clone();
            move |cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(code.line_text(line).to_owned()));
            }
        };
        let menu = div()
            .id("code-menu")
            .occlude()
            .w(px(220.))
            .bg(theme::get().popup)
            .rounded(px(6.))
            .border_1()
            .border_color(theme::get().floating_border)
            .shadow_lg()
            .p_1()
            .flex()
            .flex_col()
            .on_mouse_down_out({
                let close = close.clone();
                move |_, _, cx| close(cx)
            })
            .child(match selected {
                Some(_) => item("Copy".into(), "Ctrl C", Rc::new(copy)),
                None => widgets::menu_item_off_why("Copy", "Select text first", "Ctrl C"),
            })
            .child(item(
                format!("Copy line {}", line + 1).into(),
                "",
                Rc::new(copy_line),
            ))
            .child(item("Select all".into(), "Ctrl A", Rc::new(select_all)));
        view = view.child(
            deferred(
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu),
            )
            .with_priority(1),
        );
    }
    div()
        .id(ElementId::Name(format!("{id}-code").into()))
        .flex()
        .flex_col()
        .size_full()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .h(px(32.))
                .flex_none()
                // Flush left: the caption lines up with the text above the
                // view (a host's path line); px_1 set it 4 px in.
                .pr_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .children(caption.map(|c| crate::widgets::meta_line(&c))),
                )
                .child(bar),
        )
        .child(view)
}

#[cfg(test)]
mod tests {
    use super::{Code, Language, Token};

    fn tokens(text: &str, language: Language) -> Vec<(String, Token)> {
        Code::new(text, language)
            .tokens()
            .map(|(s, t)| (s.to_owned(), t))
            .collect()
    }

    #[test]
    fn xml_splits_tags_attributes_values_and_numbers() {
        use Token::*;
        let got = tokens(
            r#"<Prop Name="x" a='1'><GUID> 42 </GUID><!-- c --></Prop>"#,
            Language::Xml,
        );
        let want = [
            ("<", Punct),
            ("Prop", Tag),
            ("Name", Attribute),
            ("=", Punct),
            ("\"x\"", String),
            ("a", Attribute),
            ("=", Punct),
            ("'1'", String),
            (">", Punct),
            ("<", Punct),
            ("GUID", Tag),
            (">", Punct),
            ("42", Number),
            ("</", Punct),
            ("GUID", Tag),
            (">", Punct),
            ("<!-- c -->", Comment),
            ("</", Punct),
            ("Prop", Tag),
            (">", Punct),
        ];
        assert_eq!(got, want.map(|(s, t)| (s.to_owned(), t)));
    }

    #[test]
    fn xml_comments_and_cdata_span_lines_and_text_words_stay_plain() {
        let code = Code::new(
            "<a>\n<!-- one\ntwo -->\nword &amp; <![CDATA[x<y]]>\n</a>",
            Language::Xml,
        );
        let got: Vec<_> = code.tokens().map(|(s, t)| (s, t)).collect();
        assert!(got.contains(&("<!-- one\ntwo -->", Token::Comment)));
        assert!(got.contains(&("&amp;", Token::Entity)));
        assert!(got.contains(&("<![CDATA[x<y]]>", Token::Entity)));
        assert!(!got.iter().any(|(s, _)| s.contains("word")));
        // Line 2 starts inside the comment; its run is clipped to the line.
        let (text, runs) = code.styled(2);
        assert_eq!(&*text, "two -->");
        assert_eq!(runs[0].0, 0..7);
    }

    #[test]
    fn lua_marks_keywords_strings_comments_numbers_and_calls() {
        use Token::*;
        let got = tokens(
            "local x = 0x1F + 2e-3 -- note\nif x then print(\"a\\\"b\") end\n--[==[ long\n]==] s = [[raw]] nil",
            Language::Lua,
        );
        let want = [
            ("local", Keyword),
            ("0x1F", Number),
            ("2e-3", Number),
            ("-- note", Comment),
            ("if", Keyword),
            ("then", Keyword),
            ("print", Function),
            ("\"a\\\"b\"", String),
            ("end", Keyword),
            ("--[==[ long\n]==]", Comment),
            ("[[raw]]", String),
            ("nil", Number),
        ];
        assert_eq!(got, want.map(|(s, t)| (s.to_owned(), t)));
    }

    #[test]
    fn lua_annotations_stay_below_the_code() {
        use Token::*;
        let got = tokens("---@param self rdgs.CData\n", Language::Lua);
        let want = [
            ("---", Comment),
            ("@param", Doc),
            (" self", Doc),
            (" rdgs.CData", Comment),
        ];
        assert_eq!(got, want.map(|(s, t)| (s.to_owned(), t)));
        // A type after the tag stays comment, as it does after a name.
        let got = tokens("---@return rdgs.CData\n", Language::Lua);
        let want = [("---", Comment), ("@return", Doc), (" rdgs.CData", Comment)];
        assert_eq!(got, want.map(|(s, t)| (s.to_owned(), t)));
    }

    #[test]
    fn json_tells_keys_from_values() {
        use Token::*;
        let got = tokens(
            "{\"a\": \"b\", \"n\" : -1.5e3, \"ok\": [true, null] // note\n}",
            Language::Json,
        );
        let want = [
            ("\"a\"", Key),
            ("\"b\"", String),
            ("\"n\"", Key),
            ("-1.5e3", Number),
            ("\"ok\"", Key),
            ("true", Number),
            ("null", Number),
            ("// note", Comment),
        ];
        assert_eq!(got, want.map(|(s, t)| (s.to_owned(), t)));
    }

    #[test]
    fn selections_copy_across_lines_and_words_stop_at_punctuation() {
        let code = Code::new("let ab_c = 1\nnext", Language::Plain);
        let (a, b) = code.word_at(code.at_col(0, 5));
        assert_eq!(code.text_between(a, b), "ab_c");
        // Backwards selections copy the same.
        let (from, to) = (code.at_col(0, 9), code.at_col(1, 2));
        assert_eq!(code.text_between(to, from), "= 1\nne");
        assert_eq!(code.at_col(1, 99), code.end());
        assert_eq!(code.col(code.end()), 4);
    }

    #[test]
    fn find_ignores_case_and_reports_line_positions() {
        let code = Code::new("<GUID>1</GUID>\n<guid>2</guid>", Language::Xml);
        let found = code.find_all("guid>");
        assert_eq!(found.len(), 4);
        assert_eq!(code.text_between(found[2].0, found[2].1), "guid>");
        assert_eq!(found[2].0.line, 1);
        assert!(code.find_all("").is_empty());
    }

    #[test]
    fn tabs_expand_to_four_column_stops_and_long_line_is_found() {
        let code = Code::new("a\tb\n\tlonger line\nc", Language::Plain);
        assert_eq!(&*code.styled(0).0, "a   b");
        assert_eq!(&*code.styled(1).0, "    longer line");
        assert_eq!(code.longest, 1);
        assert_eq!(code.line_count(), 3);
    }

    #[test]
    fn language_follows_extension_then_content() {
        assert_eq!(Language::guess("a/b.lua", ""), Language::Lua);
        assert_eq!(Language::guess("a/b.json", "{}"), Language::Json);
        assert_eq!(Language::guess("a/b.cfg", ""), Language::Xml);
        assert_eq!(
            Language::guess("a/b.a7te", "\u{feff}  <Terrain/>"),
            Language::Xml
        );
        assert_eq!(Language::guess("a/b.txt", "plain"), Language::Plain);
    }
}
