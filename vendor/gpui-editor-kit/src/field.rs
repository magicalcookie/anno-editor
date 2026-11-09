// In-place text and number editing without a platform text input.
//
// The host keeps one `TextEdit` for the field being edited and forwards its
// root `on_key_down` to it. Only one field edits at a time, which matches how
// inspectors behave and keeps focus handling in one place.
//
// Platform text input (IME composition, OS text services) reaches an edit
// through `InputBuffer`, which a host's `EntityInputHandler` forwards to
// whichever edit is active.
//
// Mouse selection needs no host code: the field widgets hit-test their own
// text and leave the press here (`POINTER`); the edit takes it when it is
// created by that click, or on its next key. Until then the field draws the
// press's selection itself.
use crate::{theme, widgets::button};
use gpui::{prelude::*, *};
use std::{cell::RefCell, ops::Range, rc::Rc, time::Instant};

/// Inputs longer than this are ignored. Names and property values never get
/// close; it stops a stuck key from growing the buffer without bound.
const MAX_LEN: usize = 1024;

/// What `display` puts around a selection and around text an IME is still
/// composing; private-use, so no text has them.
const SELECT_START: char = '\u{e000}';
const SELECT_END: char = '\u{e001}';
const MARK_START: char = '\u{e002}';
const MARK_END: char = '\u{e003}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Text,
    /// Digits, sign, decimal point and exponent.
    Number,
}

/// The field being edited, identified by the host's own key type `F`.
///
/// Hosts may assign `text` directly; the caret then moves to its end.
// TODO(alex): no IME; a platform text input (`EntityInputHandler`) would
// bring it.
#[derive(Clone, Debug)]
pub struct TextEdit<F> {
    pub field: F,
    pub text: String,
    filter: Filter,
    /// Where the selection started, in chars; it runs to the caret. A new
    /// edit selects everything, so the first key replaces it, unless a
    /// click placed the caret.
    anchor: Option<usize>,
    /// Caret position in chars, `0..=text.chars().count()`.
    caret: usize,
    /// Text an IME is composing (preedit), in chars: drawn underlined and
    /// replaced by what it commits.
    marked: Option<Range<usize>>,
    /// `text.len()` after the last edit here. A different length means the
    /// host assigned `text`; one of the same length goes unnoticed.
    known: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditEvent {
    /// The text changed; re-render, and live-filter if this is a search box.
    Changed,
    /// Enter. Take the edit out of the host and apply its text.
    Commit,
    /// Escape. Drop the edit.
    Cancel,
    /// Not an editing key.
    Ignored,
}

/// A press in a field: which field, on what text, and the chars it spans.
struct Pointer {
    id: ElementId,
    text: String,
    anchor: usize,
    head: usize,
    live: bool,
    at: Instant,
}

thread_local! {
    static POINTER: RefCell<Option<Pointer>> = const { RefCell::new(None) };
    /// Where the field being edited was last drawn.
    static ACTIVE: std::cell::Cell<Option<Bounds<Pixels>>> = const { std::cell::Cell::new(None) };
}

/// Whether window point `p` falls in the field being edited, as last drawn.
/// A press there places the caret; a host that ends typing on any press
/// in a pane lets that one be.
pub fn in_active_field(p: Point<Pixels>) -> bool {
    ACTIVE.get().is_some_and(|b| b.contains(&p))
}

/// The pending press on `text`, taken: a few seconds old at most, so a
/// stale one cannot move the caret of an edit started by a key.
fn take_pointer(text: &str) -> Option<(usize, usize)> {
    POINTER.with_borrow_mut(|p| {
        let q = p.take()?;
        (q.text == text && q.at.elapsed().as_secs() < 10).then_some((q.anchor, q.head))
    })
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Chars of the word around char `at`, else the one char there.
fn word_around(text: &str, at: usize) -> Range<usize> {
    let chars: Vec<char> = text.chars().collect();
    let at = at.min(chars.len());
    let mut a = at;
    while a > 0 && word(chars[a - 1]) {
        a -= 1;
    }
    let mut b = at;
    while b < chars.len() && word(chars[b]) {
        b += 1;
    }
    if a == b {
        b = (at + 1).min(chars.len());
    }
    a..b
}

impl<F> TextEdit<F> {
    pub fn new(field: F, text: impl Into<String>, filter: Filter) -> Self {
        let text = text.into();
        let len = text.chars().count();
        let (anchor, caret) = match take_pointer(&text) {
            Some((anchor, head)) => (Some(anchor), head),
            None => (Some(0), len),
        };
        Self {
            field,
            caret,
            known: text.len(),
            text,
            filter,
            anchor,
            marked: None,
        }
    }

    /// The selected chars, if any.
    fn selection(&self) -> Option<Range<usize>> {
        self.anchor
            .filter(|&a| a != self.caret)
            .map(|a| a.min(self.caret)..a.max(self.caret))
    }

    /// The selected text, else all of it: what copy and cut take.
    fn chosen(&self) -> String {
        match self.selection() {
            Some(r) => self.text[self.byte(r.start)..self.byte(r.end)].to_owned(),
            None => self.text.clone(),
        }
    }

    /// Removes the selection; false when there was none.
    fn delete_selection(&mut self) -> bool {
        let range = self.selection();
        self.anchor = None;
        match range {
            Some(r) => {
                self.remove(r);
                true
            }
            None => false,
        }
    }

    /// A press or drag left by the field widget, and text the host set.
    fn sync(&mut self) {
        let len = self.text.chars().count();
        if self.text.len() != self.known {
            self.caret = len;
            self.anchor = None;
        }
        if let Some((anchor, head)) = take_pointer(&self.text) {
            self.anchor = Some(anchor);
            self.caret = head;
        }
    }

    /// Editing keys without the clipboard. Prefer [`TextEdit::key_down_in`]
    /// where an `App` is at hand, so Ctrl C/X/V work too.
    pub fn key_down(&mut self, event: &KeyDownEvent) -> EditEvent {
        self.sync();
        let key = event.keystroke.key.as_str();
        let mods = event.keystroke.modifiers;
        let ctrl = mods.control || mods.platform;
        let len = self.text.chars().count();
        let event = self.edit(key, ctrl, mods.shift, len, event);
        self.known = self.text.len();
        event
    }

    fn edit(
        &mut self,
        key: &str,
        ctrl: bool,
        shift: bool,
        len: usize,
        event: &KeyDownEvent,
    ) -> EditEvent {
        match key {
            "escape" => return EditEvent::Cancel,
            "enter" => return EditEvent::Commit,
            "a" if ctrl => {
                self.anchor = Some(0);
                self.caret = len;
                return EditEvent::Changed;
            }
            "backspace" | "delete" => {
                if self.delete_selection() {
                    return EditEvent::Changed;
                }
                let chars: Vec<char> = self.text.chars().collect();
                if key == "delete" {
                    if self.caret < len {
                        self.remove(self.caret..self.caret + 1);
                    }
                } else if ctrl {
                    // Back over spaces, then over the word before them.
                    let mut from = self.caret;
                    while from > 0 && chars[from - 1].is_whitespace() {
                        from -= 1;
                    }
                    while from > 0 && !chars[from - 1].is_whitespace() {
                        from -= 1;
                    }
                    self.remove(from..self.caret);
                } else if self.caret > 0 {
                    self.remove(self.caret - 1..self.caret);
                }
                return EditEvent::Changed;
            }
            "left" | "right" | "home" | "end" => {
                let chars: Vec<char> = self.text.chars().collect();
                let selection = self.selection();
                let to = match (key, ctrl, selection) {
                    ("home", ..) => 0,
                    ("end", ..) => len,
                    // A selection drops to the side moved to.
                    ("left", false, Some(r)) if !shift => r.start,
                    ("right", false, Some(r)) if !shift => r.end,
                    ("left", true, _) => {
                        let mut i = self.caret;
                        while i > 0 && !word(chars[i - 1]) {
                            i -= 1;
                        }
                        while i > 0 && word(chars[i - 1]) {
                            i -= 1;
                        }
                        i
                    }
                    ("right", true, _) => {
                        let mut i = self.caret;
                        while i < len && !word(chars[i]) {
                            i += 1;
                        }
                        while i < len && word(chars[i]) {
                            i += 1;
                        }
                        i
                    }
                    ("left", ..) => self.caret.saturating_sub(1),
                    _ => (self.caret + 1).min(len),
                };
                if shift {
                    self.anchor.get_or_insert(self.caret);
                } else {
                    self.anchor = None;
                }
                self.caret = to;
                return EditEvent::Changed;
            }
            _ => {}
        }
        if ctrl {
            return EditEvent::Ignored;
        }
        let Some(text) = event
            .keystroke
            .key_char
            .as_deref()
            .or_else(|| (key.chars().count() == 1).then_some(key))
        else {
            return EditEvent::Ignored;
        };
        self.insert(text)
    }

    /// [`TextEdit::key_down`] plus Ctrl C, X and V: copy and cut take the
    /// selection (all of the text when nothing is selected), paste replaces
    /// it.
    pub fn key_down_in(&mut self, event: &KeyDownEvent, cx: &mut App) -> EditEvent {
        let mods = event.keystroke.modifiers;
        if mods.control || mods.platform {
            match event.keystroke.key.as_str() {
                "c" => {
                    self.sync();
                    cx.write_to_clipboard(ClipboardItem::new_string(self.chosen()));
                    return EditEvent::Changed;
                }
                "x" => {
                    self.sync();
                    cx.write_to_clipboard(ClipboardItem::new_string(self.chosen()));
                    if !self.delete_selection() {
                        self.clear();
                    }
                    self.known = self.text.len();
                    return EditEvent::Changed;
                }
                "v" => {
                    let Some(pasted) = cx.read_from_clipboard().and_then(|c| c.text()) else {
                        return EditEvent::Ignored;
                    };
                    self.sync();
                    // One line: a pasted newline would otherwise commit.
                    let line = pasted.lines().next().unwrap_or("").trim_end();
                    let event = self.insert(line);
                    self.known = self.text.len();
                    return event;
                }
                _ => {}
            }
        }
        self.key_down(event)
    }

    /// Types `text` at the caret, replacing the selection.
    fn insert(&mut self, text: &str) -> EditEvent {
        let valid = match self.filter {
            Filter::Text => !text.chars().any(char::is_control),
            Filter::Number => text
                .chars()
                .all(|c| c.is_ascii_digit() || ".-+eE".contains(c)),
        };
        let selected = self
            .selection()
            .map_or(0, |r| self.byte(r.end) - self.byte(r.start));
        if text.is_empty() || !valid || self.text.len() - selected + text.len() > MAX_LEN {
            return EditEvent::Ignored;
        }
        self.delete_selection();
        let at = self.byte(self.caret);
        self.text.insert_str(at, text);
        self.caret += text.chars().count();
        EditEvent::Changed
    }

    fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = None;
    }

    /// Removes the chars in `range` and puts the caret where they were.
    fn remove(&mut self, range: Range<usize>) {
        let (a, b) = (self.byte(range.start), self.byte(range.end));
        self.text.replace_range(a..b, "");
        self.caret = range.start;
    }

    /// Byte offset of char `index`.
    fn byte(&self, index: usize) -> usize {
        self.text
            .char_indices()
            .nth(index)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// The text with a `│` caret, and the selection between two private-use
    /// marks, for the field widgets to draw (they strip the marks).
    pub fn display(&self) -> String {
        let len = self.text.chars().count();
        let caret = self.caret.min(len);
        let selection = self.selection().map(|r| r.start.min(len)..r.end.min(len));
        let marked = self
            .marked
            .as_ref()
            .map(|r| r.start.min(len)..r.end.min(len));
        let mut out = String::with_capacity(self.text.len() + 8);
        for (i, c) in self.text.chars().chain(std::iter::once('\0')).enumerate() {
            if marked.as_ref().is_some_and(|r| r.end == i) {
                out.push(MARK_END);
            }
            if marked.as_ref().is_some_and(|r| r.start == i) {
                out.push(MARK_START);
            }
            if selection.as_ref().is_some_and(|r| r.end == i) {
                out.push(SELECT_END);
            }
            if i == caret {
                out.push('│');
            }
            if selection.as_ref().is_some_and(|r| r.start == i) {
                out.push(SELECT_START);
            }
            if i < len {
                out.push(c);
            }
        }
        out
    }

    /// Parses a committed number, rejecting NaN and infinities.
    pub fn number(&self) -> Option<f32> {
        self.text
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
    }
}

/// `display` text split into plain text, the caret, the selection and the
/// composing text, in chars. Text without marks is all plain.
struct Shown {
    plain: String,
    caret: Option<usize>,
    selection: Option<Range<usize>>,
    marked: Option<Range<usize>>,
}

fn parse_display(content: &str) -> Shown {
    let mut plain = String::with_capacity(content.len());
    let (mut caret, mut n) = (None, 0);
    let (mut start, mut end, mut mark, mut unmark) = (None, None, None, None);
    for c in content.chars() {
        match c {
            '│' => caret = Some(n),
            SELECT_START => start = Some(n),
            SELECT_END => end = Some(n),
            MARK_START => mark = Some(n),
            MARK_END => unmark = Some(n),
            _ => {
                plain.push(c);
                n += 1;
            }
        }
    }
    Shown {
        plain,
        caret,
        selection: start.zip(end).map(|(a, b)| a..b),
        marked: mark.zip(unmark).map(|(a, b)| a..b),
    }
}

thread_local! {
    /// Where the active field drew its caret last frame, in window
    /// coordinates: what an IME places its candidate window by.
    static CARET: std::cell::Cell<Option<Bounds<Pixels>>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    /// The text being composed, in UTF-16 units, as the last `compose` or
    /// commit left it: what a host answers `marked_text_range` (`&self`,
    /// so it cannot look its edit up) with.
    static COMPOSING: RefCell<Option<Range<usize>>> = const { RefCell::new(None) };
}

/// The range an IME is composing in the active edit, if any.
pub fn composing() -> Option<Range<usize>> {
    COMPOSING.with_borrow(Clone::clone)
}

/// Forgets the composing range: the edit it was in has gone.
pub fn end_composing() {
    COMPOSING.set(None);
}

/// The caret of the field being edited, as last drawn.
pub fn caret_bounds() -> Option<Bounds<Pixels>> {
    CARET.get()
}

/// What a platform text input (an IME, OS text services) reads and writes
/// in the edit being typed into. Ranges are in UTF-16 code units, as
/// platforms count them. A host's `EntityInputHandler` forwards to its
/// active edit; committed text then goes through the host's own key path
/// (see [`TextEdit::select_for_replace`]), so every edit reacts to it as
/// to typing.
pub trait InputBuffer {
    fn len16(&self) -> usize;
    fn text_for_range16(&self, range: Range<usize>) -> String;
    fn selection16(&self) -> UTF16Selection;
    fn marked16(&self) -> Option<Range<usize>>;
    fn unmark(&mut self);
    /// Selects `range`, else the composing text, so the text the host
    /// inserts next replaces it.
    fn select_for_replace(&mut self, range: Option<Range<usize>>);
    /// Composing: `text` in place of `range`, the composing text or the
    /// selection, marked, with the caret at `selected`'s end (relative to
    /// `text`) or after it.
    fn compose(&mut self, range: Option<Range<usize>>, text: &str, selected: Option<Range<usize>>);
}

impl<F> TextEdit<F> {
    /// UTF-16 offset of char `chars`.
    fn utf16_of(&self, chars: usize) -> usize {
        self.text.chars().take(chars).map(char::len_utf16).sum()
    }

    /// Char index at UTF-16 offset `units` (rounded down inside a pair).
    fn chars_of(&self, units: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.text.chars().enumerate() {
            if seen + c.len_utf16() > units {
                return i;
            }
            seen += c.len_utf16();
        }
        self.text.chars().count()
    }

    fn chars_range(&self, range: Range<usize>) -> Range<usize> {
        self.chars_of(range.start)..self.chars_of(range.end)
    }
}

impl<F> InputBuffer for TextEdit<F> {
    fn len16(&self) -> usize {
        self.text.encode_utf16().count()
    }

    fn text_for_range16(&self, range: Range<usize>) -> String {
        let r = self.chars_range(range);
        self.text[self.byte(r.start)..self.byte(r.end)].to_owned()
    }

    fn selection16(&self) -> UTF16Selection {
        let (a, c) = (self.anchor.unwrap_or(self.caret), self.caret);
        UTF16Selection {
            range: self.utf16_of(a.min(c))..self.utf16_of(a.max(c)),
            reversed: c < a,
        }
    }

    fn marked16(&self) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|r| self.utf16_of(r.start)..self.utf16_of(r.end))
    }

    fn unmark(&mut self) {
        self.marked = None;
        COMPOSING.set(None);
    }

    fn select_for_replace(&mut self, range: Option<Range<usize>>) {
        let r = range.map(|r| self.chars_range(r)).or(self.marked.take());
        self.marked = None;
        COMPOSING.set(None);
        if let Some(r) = r {
            self.anchor = Some(r.start);
            self.caret = r.end;
        }
        self.known = self.text.len();
    }

    fn compose(&mut self, range: Option<Range<usize>>, text: &str, selected: Option<Range<usize>>) {
        let r = range
            .map(|r| self.chars_range(r))
            .or(self.marked.clone())
            .or(self.selection())
            .unwrap_or(self.caret..self.caret);
        let (a, b) = (self.byte(r.start), self.byte(r.end));
        if self.text.len() - (b - a) + text.len() > MAX_LEN {
            return;
        }
        self.text.replace_range(a..b, text);
        let n = text.chars().count();
        self.marked = (n > 0).then(|| r.start..r.start + n);
        COMPOSING.set(self.marked16());
        // `selected` counts UTF-16 units of `text`.
        let caret_in = selected.map_or(n, |s| {
            let mut seen = 0;
            text.chars()
                .take_while(|c| {
                    seen += c.len_utf16();
                    seen <= s.end
                })
                .count()
        });
        self.caret = r.start + caret_in.min(n);
        self.anchor = None;
        self.known = self.text.len();
    }
}

fn byte_of(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

/// A field's text as drawn: the selection's background, the caret as a
/// 1 px bar, and the presses that select with the mouse. `content` is the
/// edit's `display()` while `active`, else the plain value.
fn edited_text(
    id: ElementId,
    content: String,
    active: bool,
    wrap: bool,
) -> (AnyElement, PointerHooks) {
    let Shown {
        plain,
        mut caret,
        mut selection,
        marked,
    } = if active {
        parse_display(&content)
    } else {
        Shown {
            plain: content,
            caret: None,
            selection: None,
            marked: None,
        }
    };
    // A press not yet taken by an edit shows here: the drag as it goes.
    POINTER.with_borrow(|p| {
        if let Some(p) = p.as_ref().filter(|p| p.id == id && p.text == plain) {
            caret = active.then_some(p.head);
            selection = (p.anchor != p.head).then(|| p.anchor.min(p.head)..p.anchor.max(p.head));
        }
    });
    let highlights = selection
        .map(|r| {
            (
                byte_of(&plain, r.start)..byte_of(&plain, r.end),
                HighlightStyle {
                    background_color: Some(theme::get().selected.into()),
                    ..Default::default()
                },
            )
        })
        // Text still being composed, underlined as IMEs show it.
        .into_iter()
        .chain(marked.map(|r| {
            (
                byte_of(&plain, r.start)..byte_of(&plain, r.end),
                HighlightStyle {
                    underline: Some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(theme::get().text.into()),
                        wavy: false,
                    }),
                    ..Default::default()
                },
            )
        }))
        .collect::<Vec<_>>();
    let caret_byte = caret.map(|c| byte_of(&plain, c));
    let styled = StyledText::new(plain.clone()).with_highlights(highlights);
    let layout = styled.layout().clone();
    let view: Rc<std::cell::Cell<Option<EntityId>>> = Rc::default();
    let paint_layout = layout.clone();
    let paint_view = view.clone();
    let body = div()
        .relative()
        .min_w_0()
        .when(!wrap, |d| d.whitespace_nowrap())
        .child(styled)
        .child(
            canvas(
                move |_, window, _| paint_view.set(Some(window.current_view())),
                move |_, _, window, _| {
                    let Some(at) = caret_byte else { return };
                    let line = paint_layout.line_height();
                    if let Some(p) = paint_layout.position_for_index(at) {
                        let caret =
                            Bounds::new(point(p.x, p.y + line * 0.1), size(px(1.), line * 0.8));
                        CARET.set(Some(caret));
                        window.paint_quad(fill(caret, theme::get().text));
                    }
                },
            )
            .absolute()
            .inset_0(),
        )
        .into_any_element();
    (
        body,
        PointerHooks {
            id,
            layout,
            text: plain,
            view,
            active,
        },
    )
}

/// What a field needs to select with the mouse over its text.
struct PointerHooks {
    id: ElementId,
    layout: TextLayout,
    text: String,
    view: Rc<std::cell::Cell<Option<EntityId>>>,
    /// The field is the one being edited: it records where it is drawn.
    active: bool,
}

impl PointerHooks {
    /// Press, drag and release on `field`: click places the caret, a drag
    /// selects, two clicks a word, three everything.
    fn attach(self, field: Stateful<Div>) -> Stateful<Div> {
        let field = field.when(self.active, |f| {
            f.child(
                canvas(|bounds, _, _| ACTIVE.set(Some(bounds)), |_, _, _, _| {})
                    .absolute()
                    .inset_0(),
            )
        });
        let hooks = Rc::new(self);
        let chars_at = {
            let hooks = hooks.clone();
            move |p: Point<Pixels>| {
                let byte = match hooks.layout.index_for_position(p) {
                    Ok(i) | Err(i) => i.min(hooks.text.len()),
                };
                hooks.text[..byte].chars().count()
            }
        };
        let redraw = {
            let hooks = hooks.clone();
            move |cx: &mut App| {
                if let Some(view) = hooks.view.get() {
                    cx.notify(view);
                }
            }
        };
        let (down, moved) = (chars_at.clone(), chars_at);
        let (h1, h2, h3) = (hooks.clone(), hooks.clone(), hooks);
        let (r1, r2) = (redraw.clone(), redraw);
        field
            .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, _, cx| {
                let at = down(e.position);
                let len = h1.text.chars().count();
                let (anchor, head) = match e.click_count {
                    1 => (at, at),
                    2 => {
                        let w = word_around(&h1.text, at);
                        (w.start, w.end)
                    }
                    _ => (0, len),
                };
                POINTER.set(Some(Pointer {
                    id: h1.id.clone(),
                    text: h1.text.clone(),
                    anchor,
                    head,
                    live: e.click_count == 1,
                    at: Instant::now(),
                }));
                r1(cx);
            })
            .on_mouse_move(move |e: &MouseMoveEvent, _, cx| {
                let changed = POINTER.with_borrow_mut(|p| {
                    let Some(p) = p.as_mut().filter(|p| p.live && p.id == h2.id) else {
                        return false;
                    };
                    if e.pressed_button != Some(MouseButton::Left) {
                        p.live = false;
                        return false;
                    }
                    let at = moved(e.position);
                    let changed = at != p.head;
                    p.head = at;
                    p.at = Instant::now();
                    changed
                });
                if changed {
                    r2(cx);
                }
            })
            .on_mouse_up(MouseButton::Left, move |_, _, _| {
                POINTER.with_borrow_mut(|p| {
                    if let Some(p) = p.as_mut().filter(|p| p.id == h3.id) {
                        p.live = false;
                    }
                })
            })
    }
}

/// An edit's `display()` drawn as a field draws it, for hosts that frame
/// their own field: selection, caret and mouse selection included.
pub fn edit_text(id: impl Into<ElementId>, content: String, active: bool) -> Stateful<Div> {
    let id = id.into();
    let (body, hooks) = edited_text(id.clone(), content, active, false);
    hooks.attach(div().id(id).flex().items_center().min_w_0().child(body))
}

/// What a click on a number field asks the host to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NumberAction {
    /// The − or + buttons request a relative change, including queued clicks.
    Step(f32),
    /// The text was clicked: start a [`TextEdit`] with [`Filter::Number`].
    Edit,
}

/// A labeled number with step buttons. `editing` is the active edit's
/// display text when this field is being edited.
pub fn number_field<V: 'static>(
    label: &'static str,
    value: f32,
    step: f32,
    editing: Option<String>,
    cx: &mut Context<V>,
    on: impl Fn(&mut V, NumberAction, &mut Window, &mut Context<V>) + 'static,
) -> Stateful<Div> {
    let on = Rc::new(on);
    let action = |action: NumberAction| {
        let on = on.clone();
        cx.listener(move |this, _: &ClickEvent, window, cx| on(this, action, window, cx))
    };
    let active = editing.is_some();
    div()
        .id(SharedString::from(format!("number-{label}")))
        .flex()
        .items_center()
        .gap_1()
        .h(px(29.))
        .child(div().w(px(75.)).text_color(theme::get().muted).child(label))
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .bg(theme::get().input)
                .border_1()
                .border_color(if active {
                    theme::get().accent
                } else {
                    theme::get().border
                })
                .rounded(px(2.))
                .child(
                    button("−", false)
                        .w(px(22.))
                        .px_0()
                        .on_click(action(NumberAction::Step(-step))),
                )
                .child({
                    let id = ElementId::Name(format!("field-{label}").into());
                    let (body, hooks) = edited_text(
                        id.clone(),
                        editing.unwrap_or_else(|| format!("{value:.1}")),
                        active,
                        false,
                    );
                    hooks.attach(
                        div()
                            .id(id)
                            .flex_1()
                            .flex()
                            .justify_center()
                            .cursor_text()
                            .child(body)
                            .on_click(action(NumberAction::Edit)),
                    )
                })
                .child(
                    button("+", false)
                        .w(px(22.))
                        .px_0()
                        .on_click(action(NumberAction::Step(step))),
                ),
        )
}

/// The × that clears a filled search field, for a host that wraps the
/// field in a `relative` box: it sits over the field's right end. Attach
/// `.on_click` (stop propagation, so the field under it does not start
/// editing) and give the field `pr(px(22.))` while filled, so text stops
/// short of it.
pub fn clear_search(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .right(px(6.))
        .top(px(4.))
        .bottom(px(4.))
        .flex()
        .items_center()
        .px_1()
        .cursor_pointer()
        .text_color(theme::get().muted)
        .hover(|s| s.text_color(theme::get().text))
        .child("×")
        .tooltip(crate::widgets::Tooltip::text("Clear search", "", ""))
}

/// A one-line text box, such as a search field. Attach `.on_click` to start
/// editing. Shows `placeholder` muted when `text` is empty.
pub fn text_field(
    id: impl Into<ElementId>,
    text: &str,
    editing: Option<String>,
    placeholder: &'static str,
) -> Stateful<Div> {
    let active = editing.is_some();
    // An empty field keeps its placeholder after the caret while editing:
    // the hint is most useful at the moment typing starts.
    if editing.as_deref() == Some("│") {
        return div()
            .id(id)
            .flex()
            .items_center()
            .h(px(26.))
            .px_2()
            .min_w_0()
            .overflow_hidden()
            .bg(theme::get().input)
            .border_1()
            .border_color(theme::get().accent)
            .rounded(px(2.))
            .cursor_text()
            .text_color(theme::get().text)
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(1.))
                    .h(px(15.))
                    .bg(theme::get().text),
            )
            .child(
                div()
                    .min_w_0()
                    .ml_1()
                    .truncate()
                    .text_color(theme::get().disabled)
                    .child(placeholder),
            );
    }
    let (content, muted) = match editing {
        Some(text) => (text, false),
        None if text.is_empty() => (placeholder.to_owned(), true),
        None => (text.to_owned(), false),
    };
    let id = id.into();
    let field = div()
        .id(id.clone())
        .flex()
        .items_center()
        .h(px(26.))
        .px_2()
        .min_w_0()
        .overflow_hidden()
        .bg(theme::get().input)
        .border_1()
        .border_color(if active {
            theme::get().accent
        } else {
            theme::get().border
        })
        .rounded(px(2.))
        .cursor_text()
        // A placeholder must not pass for a value: the disabled colour,
        // upright. Muted italic still read as something typed.
        .text_color(if muted {
            theme::get().disabled
        } else {
            theme::get().text
        });
    // A placeholder is not text to select.
    if muted {
        return field.child(content);
    }
    let (body, hooks) = edited_text(id, content, active, false);
    hooks.attach(field.child(body))
}

/// A [`text_field`] for prose (a description): `rows` lines tall and
/// wrapping, so the whole text reads without scrolling sideways. Enter
/// still commits; the text is one paragraph.
pub fn text_area(
    id: impl Into<ElementId>,
    text: &str,
    editing: Option<String>,
    placeholder: &'static str,
    rows: usize,
) -> Stateful<Div> {
    let active = editing.is_some();
    let (content, muted) = match editing {
        Some(text) => (text, false),
        None if text.is_empty() => (placeholder.to_owned(), true),
        None => (text.to_owned(), false),
    };
    let id = id.into();
    let area = div()
        .id(id.clone())
        .min_h(px(10. + 18. * rows as f32))
        .px_2()
        .py_1()
        .min_w_0()
        .bg(theme::get().input)
        .border_1()
        .border_color(if active {
            theme::get().accent
        } else {
            theme::get().border
        })
        .rounded(px(2.))
        .cursor_text()
        .line_height(px(18.))
        .text_color(if muted {
            theme::get().disabled
        } else {
            theme::get().text
        });
    if muted {
        return area.child(content);
    }
    let (body, hooks) = edited_text(id, content, active, true);
    hooks.attach(area.child(body))
}

/// A compact label and value for floating tool bars. Attach `.on_click`.
pub fn compact_field(
    id: impl Into<ElementId>,
    label: &'static str,
    value: String,
    active: bool,
) -> Stateful<Div> {
    let id = id.into();
    let (body, hooks) = edited_text(id.clone(), value, active, false);
    let field = div()
        .id(id)
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .h(px(25.))
        .rounded(px(4.))
        .bg(theme::get().well)
        .cursor_text()
        .border_1()
        .border_color(if active {
            theme::get().accent
        } else {
            theme::get().border
        })
        .child(
            div()
                .text_size(px(10.))
                // X, Y and Z wear the gizmo's axis colours.
                .text_color(match label {
                    "X" => theme::get().axes[0],
                    "Y" => theme::get().axes[1],
                    "Z" => theme::get().axes[2],
                    _ => theme::get().muted,
                })
                .child(label),
        )
        .child(div().min_w(px(26.)).text_size(px(11.)).child(body));
    hooks.attach(field)
}

/// What a slider drag carries: the id of the slider being dragged, so a
/// host listening to every slider's drag moves can tell them apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SliderDrag(pub SharedString);

// The value moves under the cursor; nothing follows it.
struct NoDragPreview;

impl Render for NoDragPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A compact field with a fill bar: `fraction` (0..1) of the way through its
/// range. Dragging across it sets the value (the host listens with
/// `on_drag_move::<SliderDrag>` and reads [`slider_fraction`]); a click
/// without dragging is the host's to open for typing, as `compact_field`.
pub fn slider_field(
    id: impl Into<SharedString>,
    label: &'static str,
    value: String,
    fraction: f32,
    active: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let fill = fraction.clamp(0., 1.);
    div()
        .id(ElementId::Name(id.clone()))
        .relative()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        // Text sits above the track.
        .pb(px(5.))
        .w(px(150.))
        .h(px(28.))
        .rounded(px(4.))
        .overflow_hidden()
        .bg(theme::get().well)
        .cursor_ew_resize()
        .border_1()
        .border_color(if active {
            theme::get().accent
        } else {
            theme::get().border
        })
        .when(!active, |d| {
            d.hover(|s| s.border_color(theme::get().floating_border))
        })
        // A track along the bottom, accent-filled to the value, with a
        // knob at the end of the fill: reads as a slider, not a divider,
        // and stays clear of the label and value text.
        // Inset 9 px: at 0% and 100% the 8 px knob stays clear of the
        // field's rounded corner, which clipped it at 6.
        .child(
            div()
                .absolute()
                .left(px(9.))
                .right(px(9.))
                .bottom(px(3.))
                .h(px(3.))
                .rounded(px(1.5))
                .bg(theme::get().track)
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(relative(fill))
                        .rounded(px(1.5))
                        .bg(theme::get().accent),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(-2.5))
                        .left(relative(fill))
                        .ml(px(-4.))
                        .size(px(8.))
                        .rounded_full()
                        .border_1()
                        .border_color(theme::get().knob_edge)
                        .bg(if active {
                            theme::get().accent
                        } else {
                            theme::get().knob
                        }),
                ),
        )
        .child(
            div()
                .relative()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .child(label),
        )
        // Drawn as an edit draws, but a drag here moves the value: no
        // mouse selection.
        .child(
            div().relative().text_size(px(11.)).child(
                edited_text(
                    ElementId::Name(format!("{id}-value").into()),
                    value,
                    active,
                    false,
                )
                .0,
            ),
        )
        .on_drag(SliderDrag(id), |_, _, _, cx| cx.new(|_| NoDragPreview))
}

/// How far across the dragged slider the cursor is, 0..1.
pub fn slider_fraction(event: &DragMoveEvent<SliderDrag>) -> f32 {
    let b = event.bounds;
    // Along the track, 9 px in from each edge, so the knob follows the
    // pointer.
    let x = f32::from(event.event.position.x - b.origin.x) - 9.;
    (x / (f32::from(b.size.width) - 18.).max(1.)).clamp(0., 1.)
}

#[cfg(test)]
mod tests {
    use super::{EditEvent, Filter, MAX_LEN, TextEdit};
    use gpui::{KeyDownEvent, Keystroke};

    /// Text a host inserts for a platform commit: no key name, the text as
    /// the key's character.
    fn key_char(edit: &mut TextEdit<()>, text: &str) -> EditEvent {
        edit.key_down(&KeyDownEvent {
            keystroke: Keystroke {
                key: String::new(),
                key_char: Some(text.to_owned()),
                ..Keystroke::parse("a").unwrap()
            },
            is_held: false,
            prefer_character_input: false,
        })
    }

    fn key(edit: &mut TextEdit<()>, keystroke: &'static str) -> EditEvent {
        edit.key_down(&KeyDownEvent {
            keystroke: Keystroke::parse(keystroke).unwrap(),
            is_held: false,
            prefer_character_input: false,
        })
    }

    #[test]
    fn first_key_replaces_then_appends_and_backspace_edits() {
        let mut edit = TextEdit::new((), "12.5", Filter::Number);
        assert_eq!(key(&mut edit, "3"), EditEvent::Changed);
        assert_eq!(key(&mut edit, "-"), EditEvent::Changed);
        assert_eq!(edit.text, "3-");
        assert_eq!(key(&mut edit, "backspace"), EditEvent::Changed);
        assert_eq!(edit.text, "3");
        assert_eq!(key(&mut edit, "x"), EditEvent::Ignored);
        assert_eq!(key(&mut edit, "ctrl-a"), EditEvent::Changed);
        assert_eq!(key(&mut edit, "backspace"), EditEvent::Changed);
        assert_eq!(edit.text, "");
        assert_eq!(key(&mut edit, "enter"), EditEvent::Commit);
        assert_eq!(key(&mut edit, "escape"), EditEvent::Cancel);
    }

    #[test]
    fn numbers_must_be_finite_and_text_is_bounded() {
        let mut edit = TextEdit::new((), "inf", Filter::Text);
        assert_eq!(edit.number(), None);
        edit.text = " 4.25 ".into();
        assert_eq!(edit.number(), Some(4.25));
        edit.text = "a".repeat(MAX_LEN);
        edit.anchor = None;
        assert_eq!(key(&mut edit, "b"), EditEvent::Ignored);
        assert_eq!(edit.text.len(), MAX_LEN);
    }

    #[test]
    fn modifier_and_navigation_keys_do_not_become_text() {
        let mut edit = TextEdit::new((), "Mission title", Filter::Text);
        for name in ["shift", "f1", "tab"] {
            assert_eq!(key(&mut edit, name), EditEvent::Ignored);
            assert_eq!(edit.text, "Mission title");
        }
        for name in ["left", "right", "home", "end"] {
            assert_eq!(key(&mut edit, name), EditEvent::Changed);
            assert_eq!(edit.text, "Mission title");
        }
    }

    #[test]
    fn the_caret_moves_and_edits_in_place() {
        let mut edit = TextEdit::new((), "Isle de", Filter::Text);
        // Left drops the selection to the start; typing then inserts there.
        key(&mut edit, "left");
        key(&mut edit, "n");
        assert_eq!(edit.text, "nIsle de");
        assert_eq!(edit.display(), "n│Isle de");
        key(&mut edit, "end");
        key(&mut edit, "backspace");
        key(&mut edit, "left");
        key(&mut edit, "delete");
        assert_eq!(edit.text, "nIsle ");
        key(&mut edit, "ctrl-backspace");
        assert_eq!(edit.text, "");
        // Multi-byte chars count as one.
        let mut edit = TextEdit::new((), "Ölberg", Filter::Text);
        key(&mut edit, "home");
        key(&mut edit, "delete");
        assert_eq!(edit.text, "lberg");
        // Text assigned by the host clamps the caret.
        edit.text = "ab".into();
        key(&mut edit, "c");
        assert_eq!(edit.text, "abc");
    }

    #[test]
    fn shift_selects_by_char_and_word_and_edits_replace_it() {
        let mut edit = TextEdit::new((), "olive tree_01", Filter::Text);
        key(&mut edit, "home");
        key(&mut edit, "shift-ctrl-right");
        assert_eq!(edit.chosen(), "olive");
        assert_eq!(edit.display(), "\u{e000}olive\u{e001}│ tree_01");
        key(&mut edit, "shift-left");
        assert_eq!(edit.chosen(), "oliv");
        key(&mut edit, "x");
        assert_eq!(edit.text, "xe tree_01");
        // Shift+End from the caret, then delete what it took.
        key(&mut edit, "shift-end");
        assert_eq!(edit.chosen(), "e tree_01");
        key(&mut edit, "backspace");
        assert_eq!(edit.text, "x");
        // Without shift, left drops a selection to its start.
        key(&mut edit, "ctrl-a");
        key(&mut edit, "left");
        assert_eq!(edit.display(), "│x");
    }

    #[test]
    fn a_press_left_by_the_field_places_the_caret_of_the_next_edit() {
        use super::{POINTER, Pointer};
        POINTER.set(Some(Pointer {
            id: "f".into(),
            text: "harbour".into(),
            anchor: 2,
            head: 5,
            live: false,
            at: std::time::Instant::now(),
        }));
        let edit = TextEdit::new((), "harbour", Filter::Text);
        assert_eq!(edit.chosen(), "rbo");
        // Taken once; a press on other text is not.
        assert_eq!(
            TextEdit::new((), "harbour", Filter::Text).chosen(),
            "harbour"
        );
        assert_eq!(super::word_around("map_01 big", 2), 0..6);
    }

    #[test]
    fn display_marks_parse_back_to_text_caret_and_selection() {
        let shown = super::parse_display("a\u{e000}bc\u{e001}│d");
        assert_eq!(
            (shown.plain.as_str(), shown.caret, shown.selection),
            ("abcd", Some(3), Some(1..3))
        );
    }

    #[test]
    fn composing_marks_text_and_a_commit_replaces_it() {
        use super::InputBuffer;
        let mut edit = TextEdit::new((), "ab", Filter::Text);
        edit.anchor = None;
        // An IME composes "にほ" at the end, then "日本" replaces it.
        edit.compose(None, "にほ", None);
        assert_eq!(edit.text, "abにほ");
        assert_eq!(edit.marked16(), Some(2..4));
        assert_eq!(edit.display(), "ab\u{e002}にほ\u{e003}│");
        edit.select_for_replace(None);
        assert_eq!(key_char(&mut edit, "日本"), EditEvent::Changed);
        assert_eq!(edit.text, "ab日本");
        assert_eq!(edit.marked16(), None);
        // UTF-16 ranges count a surrogate pair as two.
        let mut edit = TextEdit::new((), "a😀b", Filter::Text);
        assert_eq!(edit.len16(), 4);
        assert_eq!(edit.text_for_range16(1..3), "😀");
        edit.select_for_replace(Some(3..4));
        assert_eq!(edit.chosen(), "b");
    }

    #[test]
    fn selected_maximum_length_text_can_be_replaced() {
        let mut edit = TextEdit::new((), "x".repeat(MAX_LEN), Filter::Text);
        assert_eq!(key(&mut edit, "a"), EditEvent::Changed);
        assert_eq!(edit.text, "a");
    }
}
