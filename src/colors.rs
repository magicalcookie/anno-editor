// The editor's own colors on top of the kit's `theme`. `get()` follows
// `theme::appearance()`, so one switch flips both. The consts at the bottom
// sit on game content (backdrops, marks in the 3D view and on the UI
// canvas): that content looks the same in every theme, so they do too.
use anno_formats::game::Game;
use gpui_editor_kit::gpui::{App, Rgba, Window};
use gpui_editor_kit::{platform, theme, theme::Appearance};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

pub struct Colors {
    pub launcher: Launcher,
    pub graph: Graph,
    pub layout: Layout,
    /// Timeline and canvas while a clip records.
    pub recording: Rgba,
    pub recording_text: Rgba,
    pub recording_dot: Rgba,
    /// A model ported from the other game, as a badge fill.
    pub ported: Rgba,
}

/// The game picker: a darker, bluer room than the editor, around the key art.
pub struct Launcher {
    pub page: Rgba,
    pub title: Rgba,
    /// The "Recent projects" style headings.
    pub heading: Rgba,
    pub card: Rgba,
    pub card_hover: Rgba,
    pub card_edge: Rgba,
    /// Behind the key art until it loads.
    pub art: Rgba,
    pub rule: Rgba,
    /// The game folder button and recent project rows.
    pub field: Rgba,
    pub field_hover: Rgba,
    pub body: Rgba,
    pub dim: Rgba,
    pub faint: Rgba,
    pub found: Rgba,
    pub missing: Rgba,
    pub error: Rgba,
    /// Each game's card accent: Roman gold, industrial steel.
    pub anno117: Rgba,
    pub anno1800: Rgba,
}

impl Launcher {
    pub fn accent(&self, game: Game) -> Rgba {
        match game {
            Game::Anno117 => self.anno117,
            Game::Anno1800 => self.anno1800,
        }
    }
}

/// The binding graph pane.
pub struct Graph {
    pub canvas: Rgba,
    pub node: Rgba,
    /// Event wires and pins: something that happens, not a value.
    pub event: Rgba,
    /// A value wire that carried something in the last layout, one that did
    /// not, and either while another node is hovered.
    pub wire: Rgba,
    pub wire_idle: Rgba,
    pub wire_faded: Rgba,
    /// A wire being dragged, apart from the ones that exist.
    pub wire_new: Rgba,
    pub drop_ok: Rgba,
    pub drop_no: Rgba,
    /// "and through it …" in the unbind menu: what else a removal reaches.
    pub reach: Rgba,
    /// Pin values and round value pins.
    pub value: Rgba,
    pub sub: Rgba,
    /// Why a logic node did not run.
    pub why: Rgba,
    /// Node header and border by kind: where values come from, what they
    /// set, and the logic. `widget` is focused head, head, border.
    pub widget: [Rgba; 3],
    pub inputs: [Rgba; 2],
    pub data: [Rgba; 2],
    pub clip: [Rgba; 2],
    pub logic: [Rgba; 2],
    pub logic_idle: [Rgba; 2],
}

/// UI layout handles, on the canvas and in the inspector that sets them.
pub struct Layout {
    pub anchor: Rgba,
    pub pivot: Rgba,
    /// Bounds text: the widget's rectangle in numbers.
    pub bound: Rgba,
    pub mask: Rgba,
    /// The anchor-to-pivot link and the timeline playhead.
    pub link: Rgba,
}

/// What View › Theme is set to; `System` follows the desktop's setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Choice {
    #[default]
    Dark,
    Light,
    System,
}

static CHOICE: AtomicU8 = AtomicU8::new(Choice::Dark as u8);

pub fn choice() -> Choice {
    match CHOICE.load(Ordering::Relaxed) {
        1 => Choice::Light,
        2 => Choice::System,
        _ => Choice::Dark,
    }
}

/// Show `choice` now and on the next start. A failed save keeps the switch
/// for this run; the theme is not worth an error dialog.
pub fn choose(choice: Choice, window: &Window, cx: &mut App) {
    CHOICE.store(choice as u8, Ordering::Relaxed);
    let dir = platform::config_dir("anno-editor");
    let saved = std::fs::create_dir_all(&dir).and_then(|()| {
        std::fs::write(
            dir.join("theme.json"),
            serde_json::to_vec(&choice).unwrap_or_default(),
        )
    });
    if let Err(e) = saved {
        eprintln!(
            "theme: cannot save {}: {e}",
            dir.join("theme.json").display()
        );
    }
    follow(window, cx);
}

/// Startup: the saved choice, before the first frame draws.
pub fn restore(window: &Window, cx: &mut App) {
    let saved = std::fs::read(platform::config_dir("anno-editor").join("theme.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Choice>(&bytes).ok())
        .unwrap_or_default();
    CHOICE.store(saved as u8, Ordering::Relaxed);
    follow(window, cx);
}

/// Apply the choice; called again when the desktop flips light and dark.
pub fn follow(window: &Window, cx: &mut App) {
    let appearance = match choice() {
        Choice::Dark => Appearance::Dark,
        Choice::Light => Appearance::Light,
        Choice::System => Appearance::of_window(window.appearance()),
    };
    theme::set(appearance, cx);
}

pub fn get() -> &'static Colors {
    match theme::appearance() {
        Appearance::Dark => &DARK,
        Appearance::Light => &LIGHT,
    }
}

const fn hex(c: u32) -> Rgba {
    Rgba {
        r: (c >> 16 & 0xff) as f32 / 255.,
        g: (c >> 8 & 0xff) as f32 / 255.,
        b: (c & 0xff) as f32 / 255.,
        a: 1.,
    }
}

static DARK: Colors = Colors {
    launcher: Launcher {
        page: hex(0x10171d),
        title: hex(0xf0ece3),
        heading: hex(0xc4ac82),
        card: hex(0x192127),
        card_hover: hex(0x202b32),
        card_edge: hex(0x354047),
        art: hex(0x202c32),
        rule: hex(0x2a353d),
        field: hex(0x1e282e),
        field_hover: hex(0x26333a),
        body: hex(0xe4e7e8),
        dim: hex(0x9eacb6),
        faint: hex(0x82929e),
        found: hex(0xa7c8b0),
        missing: hex(0xb8a798),
        error: hex(0xf0b0a0),
        anno117: hex(0xe0c49c),
        anno1800: hex(0xadc9d1),
    },
    graph: Graph {
        canvas: hex(0x16191e),
        node: hex(0x1d2128),
        event: hex(0xd99a4e),
        wire: hex(0x7fa8d9),
        wire_idle: hex(0x56606e),
        wire_faded: hex(0x2a3038),
        wire_new: hex(0xffffff),
        drop_ok: hex(0x8fd6a5),
        drop_no: hex(0xe8806e),
        reach: hex(0xffc857),
        value: hex(0x9fb3c8),
        sub: hex(0xaab4c0),
        why: hex(0xc9a27a),
        widget: [hex(0x2b4a6e), hex(0x24364d), hex(0x34485f)],
        inputs: [hex(0x3d2f52), hex(0x4c3d63)],
        data: [hex(0x274536), hex(0x335744)],
        clip: [hex(0x4d3a22), hex(0x63492b)],
        logic: [hex(0x34383f), hex(0x4a4f58)],
        logic_idle: [hex(0x3a322b), hex(0x5a4a3a)],
    },
    layout: Layout {
        anchor: hex(0xf2a03c),
        pivot: hex(0x5ec8ff),
        bound: hex(0xe8b34a),
        mask: hex(0xd16ad8),
        link: hex(0xf6e27a),
    },
    recording: hex(0x7d2b2b),
    recording_text: hex(0xffe0e0),
    recording_dot: hex(0xff6b6b),
    ported: hex(0xc5a3f0),
};

static LIGHT: Colors = Colors {
    launcher: Launcher {
        page: hex(0xeceff1),
        title: hex(0x1c2226),
        heading: hex(0x86662c),
        card: hex(0xffffff),
        card_hover: hex(0xf5f7f8),
        card_edge: hex(0xcdd4d9),
        art: hex(0xd3dbe0),
        rule: hex(0xe0e5e9),
        field: hex(0xf2f4f6),
        field_hover: hex(0xe5eaee),
        body: hex(0x1f272c),
        dim: hex(0x52606a),
        faint: hex(0x67747f),
        found: hex(0x377349),
        missing: hex(0x85603c),
        error: hex(0xb03c2c),
        anno117: hex(0x94702f),
        anno1800: hex(0x3b6a7b),
    },
    graph: Graph {
        canvas: hex(0xeef1f4),
        node: hex(0xffffff),
        event: hex(0xbd6714),
        wire: hex(0x3a78c8),
        wire_idle: hex(0xa3acb7),
        wire_faded: hex(0xdce1e7),
        wire_new: hex(0x1d2228),
        drop_ok: hex(0x2c8546),
        drop_no: hex(0xb3362b),
        reach: hex(0xa66a00),
        value: hex(0x3e5874),
        sub: hex(0x5a6470),
        why: hex(0x87582a),
        widget: [hex(0xc9dcf3), hex(0xdde8f4), hex(0xa8bfd9)],
        inputs: [hex(0xe7ddf3), hex(0xb8a5d2)],
        data: [hex(0xd8ecdf), hex(0x98c3a6)],
        clip: [hex(0xf2e3cb), hex(0xd0ad79)],
        logic: [hex(0xe5e8ec), hex(0xb4bbc4)],
        logic_idle: [hex(0xf0e6db), hex(0xc8b29b)],
    },
    // Deeper than the dark theme's: the inspector text must read on white,
    // and they still read over game art.
    layout: Layout {
        anchor: hex(0xc9711a),
        pivot: hex(0x1c8fcc),
        bound: hex(0x9a6a0a),
        mask: hex(0xa83cae),
        link: hex(0xb08a00),
    },
    recording: hex(0xf6d6d6),
    recording_text: hex(0x7d1f1f),
    recording_dot: hex(0xcc2f2f),
    ported: hex(0x7c52c2),
};

/// 0xRRGGBB to 0xRRGGBBAA, for `gpu::Line` and `rgba`.
pub const fn with_alpha(rgb: u32, a: u8) -> u32 {
    rgb << 8 | a as u32
}

// Backdrops behind pictures and the game's UI, as 0xRRGGBB.
pub const BACKDROP_DARK: u32 = 0x181c22;
pub const BACKDROP_LIGHT: u32 = 0xd8dbe0;
/// Checkerboard backdrop: base, then every other 12 px cell.
pub const CHECKER: [u32; 2] = [0x8a8f96, 0xb4b8be];
/// Under a full-size texture preview.
pub const IMAGE_BG: u32 = 0x101214;
/// The sea blue island outlines are drawn for: on grey, grey rocky coasts
/// all but vanished.
pub const SEA: u32 = 0x1f4a6b;
/// Around the game's screen on the UI canvas.
pub const UI_CANVAS: u32 = 0x16191e;

// Marks on the UI canvas, over the game's own widgets.
pub const CANVAS_HOVER: u32 = 0x5ec8ff;
/// Widgets a binding or search points at.
pub const CANVAS_MARK: u32 = 0xffc857;
/// Snap guides while dragging.
pub const CANVAS_GUIDE: u32 = 0xff4fd8;
/// Frames of every widget, faint.
pub const CANVAS_FRAME: u32 = 0x8cbdf230;
/// A selection that belongs to another file (a nested template).
pub const CANVAS_FOREIGN: u32 = 0x9aa6b8;
/// Outlines of hidden widgets.
pub const CANVAS_HIDDEN: u32 = 0x7a8aa3a0;

// Marks in the island's 3D view, over terrain and props.
/// The picked handle, path or box.
pub const PICK: u32 = 0xffd479;
/// Handles not picked, the ground grid, hover outlines.
pub const HANDLE: u32 = 0xffffff;
/// Dark casing under strokes, so they read on pale grass.
pub const CASING: u32 = 0x0a0a0a;
/// Selected props: outline, brackets and their label.
pub const SELECTION: u32 = 0xffa733;
pub const SELECTION_LABEL: u32 = 0xffb04d;
pub const RIVER: u32 = 0x3fa9ff;
pub const RIVER_LIT: u32 = 0x9fdcff;
pub const COAST: u32 = 0x4fd1ff;
/// Square and round area markers (blockers, sound emitters).
pub const AREA_SQUARE: u32 = 0xff5a5a;
pub const AREA_ROUND: u32 = 0xc08cff;
pub const PROP_MARKER: u32 = 0xd9c7a0;
pub const OBJECT_MARKER: u32 = 0x7fc4ff;
/// Behind labels in the 3D view: dark in every theme, since their text is
/// one of the marks above.
pub const LABEL_BG: u32 = 0x10141add;

// Data layers tinted into the island's terrain, as 0xRRGGBB. The height
// ramp stays with `island::height_color`, the function it shapes.
pub const LAYER_ACTIVE_MAP: u32 = 0xffd84d;
pub const LAYER_WATER: u32 = 0x3fae5a;
pub const LAYER_RIVER: u32 = 0x1fb8ff;
pub const LAYER_FORD: u32 = 0xff9a2e;
pub const LAYER_TREES: u32 = 0x1d7f34;
pub const LAYER_HERDS: u32 = 0xc66bff;
pub const LAYER_MARSH: u32 = 0x5c7a2e;
/// Environment types without a debug color in assets.xml still need to show.
pub const LAYER_UNCOLORED: u32 = 0xff00ff;
/// Area ids: construction green, base amber, anything else magenta.
pub const AREA_CONSTRUCTION: u32 = 0x4cd964;
pub const AREA_BASE: u32 = 0xffb340;
pub const AREA_OTHER: u32 = 0xff4dd2;
/// The buildable overlay: harbour pads, the edge of buildable land, and
/// the hatching on land that is not.
pub const BUILD_HARBOUR: u32 = 0x35d0c0;
pub const BUILD_EDGE: u32 = 0xffe14d;
pub const BUILD_BLOCKED: u32 = 0xe04a3a;
/// Water deepens toward this with depth so coastlines read from above.
pub const WATER_TINT: u32 = 0x1d5f8a;
