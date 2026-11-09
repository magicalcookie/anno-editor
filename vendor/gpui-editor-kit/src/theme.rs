// Owns the editor palettes and which one is showing. Every color the chrome
// draws comes from `get()`; switching with `set` repaints every window.
//
// The active palette is a process-wide atomic, not a GPUI global: widget
// builders (`widgets::label`, `menu_row`) take no context, and threading `cx`
// through hundreds of them would buy nothing while one app owns one look.

use gpui::{App, Rgba, WindowAppearance};
use std::sync::atomic::{AtomicU8, Ordering};

/// One complete palette. Fields are colors ready for `.bg()`, `.text_color()`
/// and `paint::*`; fade one with `.alpha(a)` instead of packing hex.
pub struct Theme {
    pub bg: Rgba,
    pub panel: Rgba,
    /// Panel headers and other raised strips.
    pub header: Rgba,
    pub menu_bar: Rgba,
    /// Text fields and boxes: a step off the cards they sit on.
    pub input: Rgba,
    pub border: Rgba,
    pub hover: Rgba,
    pub text: Rgba,
    /// Secondary text; must read at 11 px on `panel`.
    pub muted: Rgba,
    /// Text of a control that cannot be used right now.
    pub disabled: Rgba,
    pub section: Rgba,
    pub accent: Rgba,
    pub selected: Rgba,
    pub error: Rgba,
    pub viewport: Rgba,
    /// Action buttons (`widgets::action`): the one main action of a view, the
    /// rest, and destructive ones.
    pub primary: Rgba,
    pub primary_border: Rgba,
    pub secondary: Rgba,
    pub danger_border: Rgba,
    /// Status colors for badges.
    pub ok: Rgba,
    /// Floating viewport tool surfaces sit over 3D content, so near-opaque.
    pub floating: Rgba,
    pub floating_border: Rgba,
    /// Menus and dropdown lists: a shade off the panes, so a popup does not
    /// melt into the panel it opens over.
    pub popup: Rgba,
    pub tooltip: Rgba,
    /// X, Y and Z in gizmos and axis labels. They draw on the 3D view and on
    /// panels alike, so each theme picks ones that read on both.
    pub axes: [Rgba; 3],
    pub highlight: Rgba,
    /// Done, in place: a check that a step is complete.
    pub success: Rgba,

    /// Text and ticks on `primary`, `accent` and `danger` fills.
    pub on_accent: Rgba,
    pub primary_hover: Rgba,
    /// An armed destructive button ("click again to delete").
    pub danger: Rgba,
    pub danger_hover: Rgba,
    /// Hover on things that sit bare on a panel (tool cells): a tint, not a
    /// fill, so it works on any panel shade.
    pub wash: Rgba,
    /// Every other row of a banded list.
    pub band: Rgba,
    /// Drop shadows under sticky headers.
    pub shadow: Rgba,
    /// Pills and toggles over a canvas or viewport.
    pub overlay: Rgba,
    /// The faint edge that keeps an overlay visible on a matching backdrop.
    pub overlay_edge: Rgba,
    /// Edges of unchecked boxes: lighter than `border`, or they vanish.
    pub control_edge: Rgba,
    /// Cards and artboards: a raised surface on a pane.
    pub card: Rgba,
    /// Sunken fields (sliders, search), a step below `input`.
    pub well: Rgba,
    /// Slider tracks and their knobs.
    pub track: Rgba,
    pub knob: Rgba,
    pub knob_edge: Rgba,
    /// Behind modals.
    pub scrim: Rgba,
    pub scrollbar: Rgba,
    pub scrollbar_hover: Rgba,
    /// Behind thumbnails: a mid grey, so dark foliage and pale rocks both
    /// read. Content, not chrome, so both themes share it.
    pub picture_bg: Rgba,
    /// The screen a UI design is laid out on: game art is made for a dark
    /// backdrop, so both themes share it.
    pub artboard: Rgba,
    /// The screen-space handle at a gizmo's pivot.
    pub gizmo_center: Rgba,
    /// Dark casing under every gizmo stroke, so axes read on straw and soil.
    pub gizmo_underlay: Rgba,
    /// A hovered handle: near white, not the amber highlight, which read as
    /// part of the selection outline.
    pub gizmo_hover: Rgba,
    pub syntax: Syntax,
}

/// Code view token colors: distinct hues at even lightness on the panel.
pub struct Syntax {
    pub tag: Rgba,
    pub attribute: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    /// Readable as prose: generated-file notes and field docs live in
    /// comments, and at the punctuation's grey they read as disabled.
    pub comment: Rgba,
    pub keyword: Rgba,
    pub function: Rgba,
    pub entity: Rgba,
    /// The tags' green: names read alike across XML and JSON, and in the
    /// strings' blue a key and its value merged.
    pub key: Rgba,
    /// A doc-comment tag (LuaLS `@param`).
    pub doc: Rgba,
    pub punct: Rgba,
}

/// Which palette is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    /// The palette that matches the desktop's light or dark setting.
    pub fn of_window(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Light | WindowAppearance::VibrantLight => Self::Light,
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Dark,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Appearance::Dark as u8);

/// The palette showing now.
pub fn get() -> &'static Theme {
    match appearance() {
        Appearance::Dark => &DARK,
        Appearance::Light => &LIGHT,
    }
}

pub fn appearance() -> Appearance {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Appearance::Light,
        _ => Appearance::Dark,
    }
}

/// Switch palettes and repaint every window. Views that cache colors outside
/// render (baked thumbnails) do not follow; nothing does that today.
pub fn set(appearance: Appearance, cx: &mut App) {
    if CURRENT.swap(appearance as u8, Ordering::Relaxed) != appearance as u8 {
        cx.refresh_windows();
    }
}

/// Only for tests and startup, before any window draws.
pub fn set_without_refresh(appearance: Appearance) {
    CURRENT.store(appearance as u8, Ordering::Relaxed);
}

const fn hex(c: u32) -> Rgba {
    hexa(c << 8 | 0xff)
}

/// 0xRRGGBBAA.
const fn hexa(c: u32) -> Rgba {
    Rgba {
        r: (c >> 24 & 0xff) as f32 / 255.,
        g: (c >> 16 & 0xff) as f32 / 255.,
        b: (c >> 8 & 0xff) as f32 / 255.,
        a: (c & 0xff) as f32 / 255.,
    }
}

/// The JC3 world editor palette the editor started with.
pub static DARK: Theme = Theme {
    bg: hex(0x1c2026),
    panel: hex(0x242930),
    header: hex(0x34363a),
    menu_bar: hex(0x222427),
    input: hex(0x232a33),
    border: hex(0x373e48),
    hover: hex(0x41454d),
    text: hex(0xdce2e9),
    muted: hex(0xa4aebc),
    disabled: hex(0x7f8996),
    section: hex(0xc8cbd1),
    accent: hex(0x8cbdf2),
    selected: hex(0x304d6c),
    error: hex(0xe59891),
    viewport: hex(0x102b3b),
    primary: hex(0x2f6198),
    primary_border: hex(0x4f86c2),
    secondary: hex(0x2e343d),
    danger_border: hex(0x7d4343),
    ok: hex(0x8fd19e),
    floating: hexa(0x242b34f5),
    floating_border: hex(0x49525f),
    popup: hex(0x2b3139),
    tooltip: hex(0x191f27),
    axes: [hex(0xf07373), hex(0x7cdb8a), hex(0x79acff)],
    highlight: hex(0xffd479),
    success: hex(0x7cdb8a),
    on_accent: hex(0xffffff),
    primary_hover: hex(0x3a72ae),
    danger: hex(0x5a2c2e),
    danger_hover: hex(0x4a2f31),
    wash: hexa(0xffffff0d),
    band: hexa(0xffffff05),
    shadow: hexa(0x00000066),
    overlay: hexa(0x12161ce0),
    overlay_edge: hexa(0xffffff1f),
    control_edge: hex(0x5a6370),
    card: hex(0x2a3038),
    well: hex(0x171c23),
    track: hex(0x2e3742),
    knob: hex(0xc9d4e0),
    knob_edge: hex(0x171c23),
    scrim: hexa(0x000000c8),
    scrollbar: hexa(0x8b96a580),
    scrollbar_hover: hexa(0xb4bfccc0),
    picture_bg: hex(0x48505b),
    artboard: hex(0x2b2f36),
    gizmo_center: hex(0xe4eaf2),
    gizmo_underlay: hexa(0x0a0a0a99),
    gizmo_hover: hex(0xfff6d6),
    // GitHub's dark palette.
    syntax: Syntax {
        tag: hex(0x7ee787),
        attribute: hex(0x79c0ff),
        string: hex(0xa5d6ff),
        number: hex(0x79c0ff),
        comment: hex(0xa4afbd),
        keyword: hex(0xff7b72),
        function: hex(0xd2a8ff),
        entity: hex(0xffa657),
        key: hex(0x7ee787),
        doc: hex(0xc4a27a),
        punct: hex(0x8b949e),
    },
};

/// Light chrome around the same dark 3D view: text and accents are darker
/// than their dark-theme twins so they hold 4.5:1 on `panel`.
pub static LIGHT: Theme = Theme {
    bg: hex(0xe9ecf0),
    panel: hex(0xf6f7f9),
    header: hex(0xe1e5ea),
    menu_bar: hex(0xe4e7eb),
    input: hex(0xffffff),
    border: hex(0xc9cfd7),
    hover: hex(0xdde3ea),
    text: hex(0x1d2228),
    muted: hex(0x56606d),
    disabled: hex(0x939ba6),
    section: hex(0x2c323a),
    accent: hex(0x1f66b8),
    selected: hex(0xcadff6),
    error: hex(0xb3362b),
    viewport: hex(0x102b3b),
    primary: hex(0x2563a8),
    primary_border: hex(0x1d5291),
    secondary: hex(0xe4e8ed),
    danger_border: hex(0xd49a9a),
    ok: hex(0x2c8546),
    floating: hexa(0xf6f7f9f2),
    floating_border: hex(0xbfc6cf),
    popup: hex(0xffffff),
    tooltip: hex(0xffffff),
    axes: [hex(0xd9434b), hex(0x3a9a4a), hex(0x3a78d8)],
    highlight: hex(0xa66a00),
    success: hex(0x2c8546),
    on_accent: hex(0xffffff),
    primary_hover: hex(0x2f70b8),
    danger: hex(0xb3362b),
    danger_hover: hex(0xf6e0de),
    wash: hexa(0x0000000a),
    band: hexa(0x00000006),
    shadow: hexa(0x00000022),
    overlay: hexa(0xf6f7f9e6),
    overlay_edge: hexa(0x00000024),
    control_edge: hex(0x9aa3ae),
    card: hex(0xffffff),
    well: hex(0xedf0f3),
    track: hex(0xd3d9e0),
    knob: hex(0xffffff),
    knob_edge: hex(0x9aa3ae),
    scrim: hexa(0x1d222880),
    scrollbar: hexa(0x56606d60),
    scrollbar_hover: hexa(0x56606da0),
    picture_bg: hex(0x48505b),
    artboard: hex(0x2b2f36),
    gizmo_center: hex(0xe4eaf2),
    gizmo_underlay: hexa(0x0a0a0a99),
    gizmo_hover: hex(0xfff6d6),
    // GitHub's light palette.
    syntax: Syntax {
        tag: hex(0x116329),
        attribute: hex(0x0550ae),
        string: hex(0x0a3069),
        number: hex(0x0550ae),
        comment: hex(0x5b636d),
        keyword: hex(0xcf222e),
        function: hex(0x8250df),
        entity: hex(0x953800),
        key: hex(0x116329),
        doc: hex(0x7d5a2a),
        punct: hex(0x6e7781),
    },
};

/// Code and hex text. A family by name: GPUI does not resolve the generic
/// "monospace", which fell back to the proportional UI font.
pub const MONO_FONT: &str = "Noto Sans Mono";
