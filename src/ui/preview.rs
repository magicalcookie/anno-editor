// Owns preview data: fake values for the template properties of the open
// file and of the container templates it shows, the way game data fills
// them at runtime. Values live only in the editor (never staged or saved);
// the layout evaluates them like values a container passes in, so bindings,
// label texts and clips react. "Fill sample data" picks a plausible value
// per property from its type and name.
use super::{UiEditor, default_text};
use crate::editor::Editor;
use anno_formats::phx::{Asset, DefaultValue, Guid, layout};
use anyhow::Result;
use gpui_editor_kit::{
    field::{EditEvent, Filter, TextEdit, text_field},
    gpui::{prelude::*, *},
    theme,
    widgets::{Tooltip, button},
};

// CustomPropertyType values (`phx::property_type_name`) the preview edits.
const BOOL: u32 = 0x00;
const INT: u32 = 0x01;
const UINT: u32 = 0x02;
const FLOAT: u32 = 0x03;
const STRING: u32 = 0x04;
const VEC2: u32 = 0x05;
const VEC3: u32 = 0x06;
const ENUM: u32 = 0x11;
const SPACING: u32 = 0x16;
const EDITABLE: [u32; 9] = [BOOL, INT, UINT, FLOAT, STRING, VEC2, VEC3, ENUM, SPACING];

pub struct Prop {
    pub uid: u32,
    pub name: String,
    pub kind: u32,
    pub default: DefaultValue,
    /// An enum input's EnumAsset: its values show and type as entry
    /// names where the game ships them (`phx::enum_entry_name`).
    pub enum_asset: Option<Guid>,
}

pub struct File {
    pub guid: Guid,
    /// "this file", or the container widget that expands the template.
    pub owner: String,
    pub props: Vec<Prop>,
}

#[derive(Default)]
pub struct Preview {
    pub files: Vec<File>,
    pub values: layout::Preview,
    /// (file, prop) being typed, or [`FLAG_FILTER`].
    pub(super) edit: Option<TextEdit<(usize, usize)>>,
    /// What the Game data section lists: flags whose path holds it (any
    /// case).
    flag_filter: String,
    /// Which flags change the open screen.
    pub probe: FlagProbe,
}

/// The edit field key of the Game data section's filter.
const FLAG_FILTER: (usize, usize) = (usize::MAX, usize::MAX);

impl Preview {
    /// The value being typed, if any.
    pub(crate) fn typing(&mut self) -> Option<&mut dyn gpui_editor_kit::field::InputBuffer> {
        self.edit
            .as_mut()
            .map(|e| e as &mut dyn gpui_editor_kit::field::InputBuffer)
    }
}

/// The preview inputs of `asset`: its template properties of kinds the
/// preview edits that none of its own bindings write. Those the bindings
/// write are results (`Subtraction`, `MOBO`, `Division`…): a preview value
/// there is overwritten at once.
pub fn file(asset: &Asset, owner: &str) -> Option<File> {
    let guid = asset.guid()?;
    let widgets = asset.widgets();
    let table = asset.bindings(&widgets);
    let written: std::collections::HashSet<usize> = table
        .bindings
        .iter()
        .flat_map(|b| &b.targets)
        .filter_map(|&t| match table.slots.get(t) {
            Some(anno_formats::phx::binding::Slot::Template(i, _)) => Some(*i),
            _ => None,
        })
        .collect();
    let props: Vec<Prop> = asset
        .properties()
        .iter()
        .enumerate()
        .filter(|(i, p)| EDITABLE.contains(&p.kind) && !written.contains(i))
        .map(|(_, p)| Prop {
            uid: p.uid,
            name: asset.property_name(p).to_owned(),
            kind: p.kind,
            default: asset.property_default(p).clone(),
            enum_asset: p.enum_asset.map(|r| r.id),
        })
        .collect();
    (!props.is_empty()).then(|| File {
        guid,
        owner: owner.to_owned(),
        props,
    })
}

fn floats(v: &DefaultValue) -> Vec<f32> {
    match v {
        DefaultValue::F32(b) => vec![f32::from_bits(*b)],
        DefaultValue::Vec(x) => x.iter().map(|b| f32::from_bits(*b)).collect(),
        DefaultValue::S32(i) => vec![*i as f32],
        DefaultValue::U32(u) => vec![*u as f32],
        _ => Vec::new(),
    }
}

/// Text to edit a value as.
fn edit_text(v: &DefaultValue, enum_asset: Option<Guid>) -> String {
    match v {
        DefaultValue::Str(s) => s.clone(),
        DefaultValue::Bool(b) => (*b != 0).to_string(),
        DefaultValue::S32(i) => i.to_string(),
        // An enum entry by name (AlignmentX 1 is Center); the id when the
        // game ships no name for it.
        DefaultValue::U32(u) => enum_asset
            .and_then(|g| anno_formats::phx::enum_entry_name(g, *u))
            .or_else(|| anno_formats::phx::unique_entry_name(*u))
            .unwrap_or_else(|| u.to_string()),
        DefaultValue::F32(_) | DefaultValue::Vec(_) => floats(v)
            .iter()
            .map(|x| x.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        other => default_text(other),
    }
}

/// Parses `text` as a value of property kind `kind` (shape from `default`);
/// an enum's entry also by its name, any case.
fn parse(
    kind: u32,
    default: &DefaultValue,
    text: &str,
    enum_asset: Option<Guid>,
) -> Result<DefaultValue> {
    if kind == ENUM
        && let Some(e) = enum_asset.and_then(anno_formats::phx::enum_info)
    {
        let t = text.trim();
        if let Some(x) = e.entries.iter().find(|x| x.name.eq_ignore_ascii_case(t)) {
            return Ok(DefaultValue::U32(x.id));
        }
        if t.parse::<u32>().is_err() {
            let names: Vec<&str> = e.entries.iter().map(|x| x.name.as_str()).collect();
            anyhow::bail!("{} takes {}", e.name, names.join(", "));
        }
    }
    // What the input holds, for the grammar the inspector and the graph
    // type defaults in (`parse_default`).
    let like = match kind {
        BOOL => DefaultValue::Bool(0),
        INT => DefaultValue::S32(0),
        UINT | ENUM => DefaultValue::U32(0),
        FLOAT => DefaultValue::F32(0),
        STRING => DefaultValue::Str(String::new()),
        _ => {
            let n = floats(default).len().max(match kind {
                VEC2 => 2,
                VEC3 => 3,
                _ => 4,
            });
            DefaultValue::Vec(vec![0; n])
        }
    };
    super::parse_default(&like, text)
}

/// A plausible game value for a property, from its type and name; None
/// keeps the default. Used by Fill sample data and, in the layout, for
/// every property nothing else sets (`layout::Options::sample`).
pub fn sample(p: &Prop) -> Option<DefaultValue> {
    sample_value(&p.name, p.kind, &p.default)
}

/// What game-object icon fields show with sample data: the gold coin.
const SAMPLE_ICON: &str = "data/ui/2kimages/main/icons/icon_resource_money_4.png";

/// What the game's own assets put where layout only has sample data: the
/// minimap ring's entries, from the `MinimapScene` and `GameSpeedFeature`
/// assets in `assets.xml`. Read once per process (the sampler is a plain
/// function the layout calls).
struct SceneContent {
    /// The ring list (`RadialButtonsBase`) item by item, in its groups'
    /// order (the wheel group components on the list: DayAndNight,
    /// GameSpeed, Controls, Lodes, Fertilities): icon path and badge text.
    ring: Vec<(String, String)>,
    /// `RadialMinimapData.MapsCount` / `LodesCount`: where the Lodes and
    /// Fertilities groups begin.
    controls: usize,
    lodes: usize,
    /// Where the fertilities begin in `ring`.
    fertilities: usize,
}

static SCENE_CONTENT: std::sync::OnceLock<Option<SceneContent>> = std::sync::OnceLock::new();

/// Reads the scene content sample data draws from (see [`SceneContent`]);
/// later calls do nothing.
pub fn load_scene_content(install: &anno_formats::vfs::Install) {
    SCENE_CONTENT.get_or_init(|| {
        let xml = install.read(install.game().profile().assets_path).ok()?;
        SceneContent::read(std::str::from_utf8(&xml).ok()?)
    });
}

impl SceneContent {
    fn read(xml: &str) -> Option<SceneContent> {
        // An asset's values run to `</Values>` (`</Asset>` also closes
        // `<Asset>` list items inside, as in MinimapDotSortOrder).
        let asset = |template: &str| {
            let at = xml.find(&format!("<Template>{template}</Template>"))?;
            let rest = &xml[at..];
            Some(&rest[..rest.find("</Values>")?])
        };
        let inner = |chunk: &'_ str, tag: &str| -> Option<String> {
            let open = format!("<{tag}>");
            let at = chunk.find(&open)? + open.len();
            let end = chunk[at..].find(&format!("</{tag}>"))?;
            Some(chunk[at..at + end].to_owned())
        };
        // An Icon (or Fertility, Slot) asset's texture: its own
        // IconFilename, where its Standard block names it.
        let icon = |guid: &str| -> Option<String> {
            let at = xml
                .match_indices(&format!("<GUID>{guid}</GUID>"))
                .map(|(at, _)| at)
                .find(|&at| xml[..at].trim_end().ends_with("<Standard>"))?;
            let rest = &xml[at..];
            let values = &rest[..rest.find("</Values>")?];
            inner(values, "IconFilename").map(|p| p.replace('\\', "/"))
        };
        let minimap = asset("MinimapScene")?;
        let speeds = asset("GameSpeedFeature")?;
        let button = |name: &str| {
            let block = inner(minimap, name)?;
            icon(&inner(&block, "Icon")?)
        };
        let listed = |block: &str, tag: &str| -> Vec<String> {
            block
                .split(&format!("<{tag}>"))
                .skip(1)
                .filter_map(|s| icon(&s[..s.find(&format!("</{tag}>"))?]))
                .collect()
        };
        let none = String::new();
        let mut ring: Vec<(String, String)> = Vec::new();
        ring.extend(button("DayNight").map(|i| (i, none.clone())));
        // The game lays the speeds and controls out clockwise in this order
        // (the 2026-11-04 shots: fastest next to day and night, pause next
        // to the notes button).
        for speed in ["Fastest", "Fast", "Normal", "Paused"] {
            let block = inner(speeds, speed)?;
            ring.push((icon(&inner(&block, "Icon")?)?, none.clone()));
        }
        // CoopPing only shows in a co-op game.
        let controls: Vec<String> = ["NotesTool", "QuickNavigation", "SwitchSession"]
            .iter()
            .filter_map(|b| button(b))
            .collect();
        let lodes = listed(&inner(&inner(minimap, "LodesPerRegion")?, "Roman")?, "Slot");
        let fertilities = listed(&inner(minimap, "FertilityOrder")?, "GUID");
        let (n_controls, n_lodes) = (controls.len(), lodes.len());
        let first_fertility = ring.len() + n_controls + n_lodes;
        ring.extend(controls.into_iter().map(|i| (i, none.clone())));
        // A lode shows how many deposits are left.
        ring.extend(lodes.into_iter().map(|i| (i, "5".to_owned())));
        // An island has a handful: the ring's Fertilities group keeps room
        // for 8 (its MinGroupItemCount).
        ring.extend(fertilities.into_iter().take(8).map(|i| (i, none.clone())));
        Some(SceneContent {
            ring,
            controls: n_controls,
            lodes: n_lodes,
            fertilities: first_fertility,
        })
    }

    /// A sampled value the scene content answers: `item` is the list item
    /// the path is under (see `layout::Parent::item`).
    fn sample(&self, item: Option<(&str, usize)>, name: &str, last: &str) -> Option<DefaultValue> {
        match item {
            // How many items the list stamps (see `layout`'s sample count).
            None if name.ends_with("minimapradialwheel.radialbuttonsbase#len") => {
                Some(DefaultValue::S32(self.ring.len() as i32))
            }
            // Radial menus all name their list `RadialButtonsBase`; the
            // minimap's is under `MinimapRadialWheel`.
            Some((list, n)) if list.ends_with("MinimapRadialWheel.RadialButtonsBase") => {
                let (icon, badge) = self.ring.get(n)?;
                if last == "icon" {
                    Some(DefaultValue::Str(icon.clone()))
                } else if last == "text" && name.ends_with("textdata.text") {
                    Some(DefaultValue::Str(badge.clone()))
                } else if name.ends_with("states.context.index") {
                    // Context 1 picks the sector style without a button
                    // tile: fertilities are information, not buttons (they
                    // float on the band in the 2026-11-04 shots).
                    Some(DefaultValue::S32(i32::from(n >= self.fertilities)))
                } else {
                    None
                }
            }
            None if name.ends_with("radialminimapdata.mapscount") => {
                Some(DefaultValue::S32(self.controls as i32))
            }
            None if name.ends_with("radialminimapdata.lodescount") => {
                Some(DefaultValue::S32(self.lodes as i32))
            }
            _ => None,
        }
    }
}

/// Flags that show a state the game turns on only when something happens:
/// default to off (shipped defaults often turn them on for authoring).
/// Gamepad prompts are off too: the reference is a mouse and keyboard game.
const OFF: [&str; 22] = [
    "lock",
    "gamepad",
    "controller",
    "btnfeedback",
    "disable",
    "hidden",
    "hide",
    "error",
    "warning",
    "empty",
    "new",
    "highlight",
    "select",
    "press",
    "focus",
    "open",
    "expand",
    "hover",
    "debug",
    "test",
    "tutorial",
    "blink",
];
const ON: [&str; 7] = [
    "visible",
    "show",
    "enabled",
    "available",
    "unlocked",
    "valid",
    "has",
];

/// [`sample`] by name, type and default. `kind` u32::MAX is a game-object
/// field the Lua stubs give no type for: guessed from the name.
pub fn sample_value(name: &str, kind: u32, default: &DefaultValue) -> Option<DefaultValue> {
    // Under a list item the layout names the item first:
    // `<list DataSource>#<index>:<path>`.
    let (item, name) = match name.split_once(':') {
        Some((head, rest)) => match head.rsplit_once('#') {
            Some((list, n)) => (n.parse::<usize>().ok().map(|n| (list, n)), rest),
            None => (None, name),
        },
        None => (None, name),
    };
    let mut data = GAME_DATA.lock().unwrap_or_else(|e| e.into_inner());
    let v = match data.overrides.get(name) {
        Some(&on) => Some(DefaultValue::Bool(u8::from(on))),
        None => sample_rules(item, name, kind, default),
    };
    // A flag probe on this thread records what it read and flips its one
    // flag; the editor's own list stays out of it.
    let probed = PROBE.with_borrow_mut(|p| {
        let p = p.as_mut()?;
        Some(match &v {
            Some(DefaultValue::Bool(b)) => {
                p.read.entry(name.to_owned()).or_insert(*b != 0);
                let flip = u8::from(p.flip.as_deref() == Some(name));
                Some(DefaultValue::Bool(b ^ flip))
            }
            other => other.clone(),
        })
    });
    if let Some(v) = probed {
        return v;
    }
    if let Some(DefaultValue::Bool(b)) = v
        && !data.overrides.contains_key(name)
    {
        data.seen.insert(name.to_owned(), b != 0);
    }
    v
}

/// A flag probe running on this thread: the flag it flips, and every flag
/// sampling answered (before the flip).
struct Probe {
    flip: Option<String>,
    read: std::collections::BTreeMap<String, bool>,
}

thread_local! {
    static PROBE: std::cell::RefCell<Option<Probe>> = const { std::cell::RefCell::new(None) };
}

/// What switching one game-data flag does to a screen: the widgets it
/// shows, hides or otherwise changes (outermost ones, by name).
#[derive(Clone, Debug, Default)]
pub struct FlagEffect {
    pub path: String,
    /// What the flag reads now (hand-set or sampled).
    pub value: bool,
    pub shows: Vec<String>,
    pub hides: Vec<String>,
    pub changes: Vec<String>,
    /// Those widgets, for finding them on the canvas (see `NodeKey`).
    pub targets: Vec<NodeKey>,
}

/// A layout node by where it comes from, stable across layouts of one
/// screen: the instance key of the template copy it sits in (0 for the
/// screen's own widgets), its file, its index in that file's widgets, and
/// which repeat of those three it is (list items).
pub type NodeKey = (u64, Option<Guid>, usize, usize);

/// Widgets one effect may point at; a panel's worth is plenty to frame.
const MAX_TARGETS: usize = 64;

impl FlagEffect {
    pub fn any(&self) -> bool {
        !(self.shows.is_empty() && self.hides.is_empty() && self.changes.is_empty())
    }
}

/// A drawn node as a probe compares it: shown (through its ancestors), and
/// what shows of it, in coarse units so float noise is no change.
#[derive(PartialEq)]
struct Look {
    shown: bool,
    what: Option<[i32; 9]>,
    text: Option<String>,
}

/// Threads a flag probe runs on: each holds one layout tree at a time.
const PROBE_THREADS: usize = 4;

/// A sampler name without the list item it is asked for under
/// (`<list>#<n>:<path>` → `<path>`), as flags are switched.
fn flag_name(name: &str) -> &str {
    match name.split_once(':') {
        Some((head, rest)) if head.contains('#') => rest,
        _ => name,
    }
}

/// The flags a layout of `asset` reads, and what flipping each one does.
/// Lays the screen out once, recording which template instances read each
/// flag; then, per flag, rebuilds only those instances from that layout
/// (and what their change reaches) and compares. Hidden readers count: a
/// template can show its own container, or hand a size to a parent. Run
/// it off the UI thread; `stop` ends it early (a newer probe replaced it).
pub fn probe_flags(
    install: &anno_formats::vfs::Install,
    asset: &Asset,
    preview: &layout::Preview,
    shown: bool,
    stop: &std::sync::atomic::AtomicBool,
) -> Option<Vec<FlagEffect>> {
    probe(install, asset, preview, shown, stop, true)
}

fn probe(
    install: &anno_formats::vfs::Install,
    asset: &Asset,
    preview: &layout::Preview,
    shown: bool,
    stop: &std::sync::atomic::AtomicBool,
    incremental: bool,
) -> Option<Vec<FlagEffect>> {
    use rayon::prelude::*;
    use std::collections::HashMap;
    type Assets = HashMap<Guid, Option<std::sync::Arc<Asset>>>;
    // One layout with `flip` switched. `stale` instances read what changed
    // since the cache's last layout of this root; without them (and with a
    // generation of its own) the build starts over.
    let run = |flip: Option<&str>,
               stale: Vec<u64>,
               generation: u64,
               cache: &mut layout::Cache,
               assets: &mut Assets| {
        PROBE.set(Some(Probe {
            flip: flip.map(str::to_owned),
            read: Default::default(),
        }));
        let opts = layout::Options {
            bindings: true,
            preview: preview.clone(),
            shown,
            sample: Some(sample_value as layout::Sampler),
            sample_generation: generation,
            stale,
            record_reads: flip.is_none(),
            ..Default::default()
        };
        let tree = layout::Tree::build_cached(
            asset,
            &mut |g| {
                assets
                    .entry(g)
                    .or_insert_with(|| {
                        super::scene::read_asset(install, g).map(std::sync::Arc::new)
                    })
                    .clone()
            },
            // Widths only need to agree between the runs compared.
            &mut |_, text| Some(text.chars().count() as f32 * 9.),
            &opts,
            cache,
        );
        let read = PROBE.take().map(|p| p.read).unwrap_or_default();
        (looks(&tree, asset), read, tree)
    };
    let mut assets = Assets::new();
    let mut first = layout::Cache::default();
    let (base, read, tree) = run(None, Vec::new(), 1, &mut first, &mut assets);
    // Which instances read each flag.
    let keys = tree.instance_keys();
    let mut readers: HashMap<&str, Vec<u64>> = HashMap::new();
    let reads_by = tree.reads_by.take();
    for (name, ks) in &reads_by {
        readers.entry(flag_name(name)).or_default().extend(ks);
    }
    // Template inputs sampled by name (`-IsFocused`) are read by every
    // instance of a template that has one; their samples are kept with the
    // template, which a flipped run forgets.
    let mut by_template: HashMap<&str, (Vec<u64>, Vec<Guid>)> = HashMap::new();
    for (node, &f) in tree.source.iter().map(|(f, _)| f).enumerate() {
        let Some(t) = f.checked_sub(1).map(|f| &tree.templates[f]) else {
            continue;
        };
        let Some(key) = tree.instance[node].and_then(|c| keys.get(&c)) else {
            continue;
        };
        for p in t.properties() {
            let name = t.property_name(p);
            if let Some((flag, _)) = read.get_key_value(name) {
                // Duplicates go when the plan sorts each flag's lists.
                let e = by_template.entry(flag.as_str()).or_default();
                e.0.push(*key);
                e.1.extend(t.guid());
            }
        }
    }
    let plan: Vec<(String, bool, Vec<u64>, Vec<Guid>)> = read
        .iter()
        .map(|(path, &value)| {
            let path = path.clone();
            let mut ks: Vec<u64> = readers.get(path.as_str()).cloned().unwrap_or_default();
            let (more, mut templates) = by_template.get(path.as_str()).cloned().unwrap_or_default();
            ks.extend(more);
            ks.sort_unstable();
            ks.dedup();
            templates.sort_unstable();
            templates.dedup();
            (path, value, ks, templates)
        })
        .collect();
    drop(tree);
    let mut out: Vec<FlagEffect> = Vec::with_capacity(plan.len());
    // A few threads share the flags, each starting from its own copy of
    // the baseline; each run holds a whole layout tree, so threads are what
    // memory grows with, and the probe must leave the editor its cores.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(PROBE_THREADS)
        .build()
        .ok()?;
    let chunk = plan.len().div_ceil(PROBE_THREADS).max(1);
    let found: Vec<Option<Vec<FlagEffect>>> = pool.install(|| {
        plan.par_chunks(chunk)
            .map(|flags| {
                let mut assets = assets.clone();
                let mut cache = layout::Cache::default();
                let mut done = Vec::with_capacity(flags.len());
                if incremental {
                    let (.., tree) = run(None, Vec::new(), 1, &mut cache, &mut assets);
                    cache.keep(tree);
                }
                // What the last run flipped: rebuilt again, back as it was.
                let mut last: (Vec<u64>, Vec<Guid>) = Default::default();
                for (k, (path, value, ks, templates)) in flags.iter().enumerate() {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        return None;
                    }
                    let (flipped, tree) = if incremental {
                        for g in last.1.iter().chain(templates) {
                            cache.forget_template(*g);
                        }
                        let stale: Vec<u64> = last.0.iter().chain(ks).copied().collect();
                        let (looks, _, tree) = run(Some(path), stale, 1, &mut cache, &mut assets);
                        last = (ks.clone(), templates.clone());
                        (looks, tree)
                    } else {
                        let (looks, _, tree) = run(
                            Some(path),
                            Vec::new(),
                            k as u64 + 2,
                            &mut cache,
                            &mut assets,
                        );
                        (looks, tree)
                    };
                    if incremental {
                        cache.keep(tree);
                    } else {
                        drop(tree);
                        cache.forget(asset.guid());
                    }
                    let mut e = FlagEffect {
                        path: path.clone(),
                        value: *value,
                        ..Default::default()
                    };
                    diff(&base, &flipped, &mut e);
                    done.push(e);
                }
                Some(done)
            })
            .collect()
    });
    for f in found {
        out.extend(f?);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Some(out)
}

type Looks = std::collections::HashMap<NodeKey, (Look, String, Option<NodeKey>)>;

/// Every node of `tree` keyed by where it comes from (instance, file,
/// widget, copy), with how it looks, its name and its parent's key.
fn looks(tree: &layout::Tree, asset: &Asset) -> Looks {
    let keys = tree.instance_keys();
    let file = |f: usize| match f {
        0 => asset.guid(),
        f => tree.templates[f - 1].guid(),
    };
    let lists: Vec<std::sync::Arc<[anno_formats::phx::Widget]>> = std::iter::once(asset.widgets())
        .chain(tree.templates.iter().map(|t| t.widgets()))
        .collect();
    let mut copies: std::collections::HashMap<(u64, usize, usize), usize> = Default::default();
    let mut key_of = Vec::with_capacity(tree.nodes.len());
    let mut shown = super::Bits::new(tree.nodes.len());
    let mut alpha = vec![1f32; tree.nodes.len()];
    let mut out = Looks::default();
    for (i, n) in tree.nodes.iter().enumerate() {
        let (f, k) = tree.source[i];
        let inst = tree.instance[i]
            .and_then(|c| keys.get(&c))
            .copied()
            .unwrap_or(0);
        let c = copies.entry((inst, f, k)).or_default();
        let key = (inst, file(f), k, *c);
        *c += 1;
        key_of.push(key);
        let parent = tree.parent[i];
        let g = &n.geometry;
        alpha[i] = g.alpha * parent.map_or(1., |p| alpha[p]);
        let on = g.visible && alpha[i] > 0.01 && parent.is_none_or(|p| shown.get(p));
        shown.set(i, on);
        let q = |v: f32| (v * 2.).round() as i32;
        let size = n.bound.size.unwrap_or_default();
        let look = Look {
            shown: on,
            what: on.then(|| {
                [
                    q(g.position[0]),
                    q(g.position[1]),
                    q(g.scale[0] * 50.),
                    q(g.scale[1] * 50.),
                    q(g.rotation),
                    q(alpha[i] * 50.),
                    q(size[0]),
                    q(size[1]),
                    g.tint
                        .map_or(-1, |t| i32::from_le_bytes([t[0], t[1], t[2], 0])),
                ]
            }),
            text: on
                .then(|| n.text.clone().or_else(|| n.loc.clone()))
                .flatten(),
        };
        let name = lists[f]
            .get(k)
            .and_then(|w| w.name.clone())
            .unwrap_or_else(|| "widget".into());
        out.insert(key, (look, name, parent.map(|p| key_of[p])));
    }
    out
}

/// What differs between two probes' looks, outermost changes only (a panel
/// that appears names the panel, not its forty children).
fn diff(base: &Looks, flipped: &Looks, e: &mut FlagEffect) {
    let changed = |k: &NodeKey| match (base.get(k), flipped.get(k)) {
        (Some(a), Some(b)) => a.0 != b.0 && (a.0.shown || b.0.shown),
        (Some(a), None) => a.0.shown,
        (None, Some(b)) => b.0.shown,
        (None, None) => false,
    };
    let mut seen = std::collections::BTreeSet::new();
    for (k, (look, name, parent)) in base
        .iter()
        .map(|(k, v)| (k, (Some(&v.0), &v.1, v.2)))
        .chain(
            flipped
                .iter()
                .filter(|(k, _)| !base.contains_key(k))
                .map(|(k, v)| (k, (None, &v.1, v.2))),
        )
    {
        if !changed(k) || parent.as_ref().is_some_and(changed) {
            continue;
        }
        let now = flipped.get(k).map(|v| &v.0);
        let list = match (look.is_some_and(|l| l.shown), now.is_some_and(|l| l.shown)) {
            (false, true) => &mut e.shows,
            (true, false) => &mut e.hides,
            _ => &mut e.changes,
        };
        if seen.insert((name.clone(), list.len())) && !list.contains(name) {
            list.push(name.clone());
        }
        if e.targets.len() < MAX_TARGETS {
            e.targets.push(*k);
        }
    }
    for l in [&mut e.shows, &mut e.hides, &mut e.changes] {
        l.sort();
    }
    e.targets.sort_unstable();
}

impl UiEditor {
    /// Outlines and frames the widgets a Game data flag changes, matching
    /// the probe's keys to the canvas nodes, without selecting them: the
    /// inspector stays on Game data for the next flag. A second click on
    /// the same effect selects the first of them. Says what it did.
    pub(super) fn show_effect(&mut self, e: &FlagEffect) -> String {
        let root = self.layout_root().guid();
        let mut copies: std::collections::HashMap<(u64, Option<Guid>, usize), usize> =
            Default::default();
        let mut hits = Vec::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let (inst, file) = n.owners.first().map_or((0, root), |&(g, k)| (k, Some(g)));
            let c = copies.entry((inst, file, n.widget)).or_default();
            if e.targets.binary_search(&(inst, file, n.widget, *c)).is_ok() {
                hits.push(i);
            }
            *c += 1;
        }
        // Hidden ones still have a box: where they would show.
        let Some(&first) = hits
            .iter()
            .find(|&&i| self.nodes[i].visible)
            .or(hits.first())
        else {
            return "Not on the canvas: what it changes is outside this view".into();
        };
        let r = hits
            .iter()
            .map(|&i| self.extent(i))
            .filter(|r| r[2] > 0. && r[3] > 0.)
            .reduce(|a, b| {
                let (x0, y0) = (a[0].min(b[0]), a[1].min(b[1]));
                let (x1, y1) = (
                    (a[0] + a[2]).max(b[0] + b[2]),
                    (a[1] + a[3]).max(b[1] + b[3]),
                );
                [x0, y0, x1 - x0, y1 - y0]
            });
        let again = self.canvas.marked == hits;
        if again {
            self.canvas.marked.clear();
            self.select_widget(first);
            return format!("Selected {}", self.nodes[first].name);
        }
        if let Some(r) = r {
            self.frame_rect(r);
        }
        self.canvas.marked = hits;
        let name = &self.nodes[first].name;
        match self.canvas.marked.len() {
            1 => format!("Outlined {name}; click again to select it"),
            n => format!("Outlined the {n} widgets it changes; click again to select {name}"),
        }
    }
}

/// Flags of game data set by hand in the Game data section, by path as the
/// sampler sees it, and every flag sampling was asked for with what it
/// gave, for the section to list. `generation` moves on every change, so
/// layout evaluates anew (see `layout::Options::sample_generation`).
#[derive(Default)]
struct GameData {
    overrides: std::collections::BTreeMap<String, bool>,
    seen: std::collections::BTreeMap<String, bool>,
    generation: u64,
}

static GAME_DATA: std::sync::Mutex<GameData> = std::sync::Mutex::new(GameData {
    overrides: std::collections::BTreeMap::new(),
    seen: std::collections::BTreeMap::new(),
    generation: 0,
});

/// Which version of the hand-set game data layout samples from.
pub fn game_data_generation() -> u64 {
    GAME_DATA
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .generation
}

/// Sets flag `path` (None: back to what sample data says).
pub fn set_game_flag(path: &str, on: Option<bool>) {
    let mut data = GAME_DATA.lock().unwrap_or_else(|e| e.into_inner());
    match on {
        Some(on) => data.overrides.insert(path.to_owned(), on),
        None => data.overrides.remove(path),
    };
    data.generation += 1;
}

/// Held by tests that flip the game's real flags: the flags are one table
/// for the process, and two such tests at once see each other's flips.
#[cfg(test)]
pub(crate) static FLAG_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn reset_game_flags() {
    let mut data = GAME_DATA.lock().unwrap_or_else(|e| e.into_inner());
    data.overrides.clear();
    data.generation += 1;
}

/// The flags sampling was asked for: path, value shown, and whether it is
/// set by hand.
pub(super) fn game_flags() -> Vec<(String, bool, bool)> {
    let data = GAME_DATA.lock().unwrap_or_else(|e| e.into_inner());
    data.seen
        .keys()
        .chain(data.overrides.keys())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .map(|k| match data.overrides.get(k) {
            Some(&on) => (k.clone(), on, true),
            None => (k.clone(), data.seen[k], false),
        })
        .collect()
}

/// [`sample_value`]'s rules, by type and name.
fn sample_rules(
    item: Option<(&str, usize)>,
    name: &str,
    kind: u32,
    default: &DefaultValue,
) -> Option<DefaultValue> {
    let n = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| n.contains(w));
    // Game-object fields come as a path (`IconTextData.TextData.Text`):
    // prefixes and exact names are about the field, words anywhere.
    let last = n.rsplit('.').next().unwrap_or(&n);
    if let Some(Some(content)) = SCENE_CONTENT.get()
        && let Some(v) = content.sample(item, &n, last)
    {
        return Some(v);
    }
    let starts = |words: &[&str]| words.iter().any(|w| last.starts_with(w));
    // A text template's layout object (`-TextLayout.Overflow_X`,
    // `-Layout.TextPlane`) holds enums and sizes, not text: sampled as
    // "19,743" it broke every label's sizing that reads it.
    if kind == u32::MAX
        && starts(&[
            "sizing",
            "overflow",
            "alignment",
            "textplane",
            "letter_spacing",
        ])
    {
        return None;
    }
    // A field of no known type is guessed from its own name: words in the
    // path before it name objects (`GameContext` holds "text", which made
    // the minimap's SceneData object a "19,743").
    let in_last = |words: &[&str]| words.iter().any(|w| last.contains(w));
    let kind = match kind {
        u32::MAX if starts(&["is", "has", "can", "should", "show", "use", "enable"]) => BOOL,
        // Composite game objects (`ObjectiveIconTitle`: an icon and a
        // text) hold fields, not a value: their name's "icon" or "title"
        // would sample a string where the template reads fields.
        u32::MAX if ["icontitle", "icontext"].iter().any(|w| last.ends_with(w)) => {
            return None;
        }
        // Objects by their name: data, context, scenes and lists hold
        // fields.
        u32::MAX
            if ["data", "context", "base", "list", "object", "scene"]
                .iter()
                .any(|w| last.ends_with(w)) =>
        {
            return None;
        }
        u32::MAX if in_last(&["text", "name", "title", "label", "desc", "tooltip"]) => STRING,
        u32::MAX if in_last(&["progress", "percent", "ratio"]) => FLOAT,
        u32::MAX if in_last(&["amount", "count", "number", "value"]) => INT,
        k => k,
    };
    match kind {
        // How many entries a list shows: as many as sample lists stamp.
        INT | UINT if last.ends_with("itemcount") => Some(DefaultValue::S32(
            anno_formats::phx::layout::SAMPLE_LIST_ITEMS as i32,
        )),
        // A count of entries (`MapsCount`, `LodesCount`: where the minimap
        // ring's next group begins) is a few; a figure reads as 12.
        INT | UINT if last.ends_with("count") && floats(default).iter().all(|v| *v == 0.) => {
            Some(DefaultValue::S32(2))
        }
        INT | UINT
            if has(&["amount", "count", "number", "value"])
                && floats(default).iter().all(|v| *v == 0.) =>
        {
            Some(DefaultValue::S32(12))
        }
        BOOL if has(&["unlocked"]) => Some(DefaultValue::Bool(1)),
        // Most entries of a row are not its last (dividers show between).
        BOOL if last.starts_with("islast") || last.starts_with("isfirst") => {
            Some(DefaultValue::Bool(0))
        }
        // A panel the game shows from its own state (the minimap's province
        // card, popups, the quest tracker's per-type variants) is
        // conditional: hidden in an idle game.
        // An active quest always shows its objectives (not the community
        // challenge's, `QuestObjectivesTLCData`, nor a choice quest's).
        BOOL if *default == DefaultValue::None
            && (last.contains("visible") || last.contains("shown"))
            && has(&["objective"])
            && !has(&["tlc", "choice"]) =>
        {
            Some(DefaultValue::Bool(1))
        }
        // A HUD's buttons are there: their data's IsVisible is on (the
        // construction menu's round access buttons, its tabs). Debug
        // layouts and gamepad prompts stay off.
        BOOL if *default == DefaultValue::None
            && last == "isvisible"
            && has(&["btn", "button"])
            && !has(&["debug", "gamepad"]) =>
        {
            Some(DefaultValue::Bool(1))
        }
        BOOL if *default == DefaultValue::None
            && (last.contains("visible") || last.contains("shown")) =>
        {
            Some(DefaultValue::Bool(0))
        }
        BOOL if has(&OFF) => Some(DefaultValue::Bool(0)),
        BOOL if has(&ON) => Some(DefaultValue::Bool(1)),
        // A progress or fill the game sets: partly done reads better than
        // the empty or full authoring default.
        FLOAT
            if has(&["progress", "percent", "ratio", "fill"])
                && floats(default).first().is_none_or(|v| *v == 0.) =>
        {
            Some(DefaultValue::F32(0.65f32.to_bits()))
        }
        // A game-object icon field (`Data.Icon`) names an image by path:
        // show a resource icon so icon slots are not empty.
        STRING if *default == DefaultValue::None && last.contains("icon") => {
            Some(DefaultValue::Str(SAMPLE_ICON.into()))
        }
        // Image, sound and id strings name assets; text would break them.
        STRING
            if [
                "image", "icon", "texture", "sound", "path", "guid", "id", "key",
            ]
            .iter()
            .any(|w| last.contains(w)) =>
        {
            None
        }
        STRING if matches!(default, DefaultValue::Str(t) if !t.is_empty()) => None,
        STRING => Some(DefaultValue::Str(
            // The field name first (`Header`, `Island_Name`), then where it
            // sits (`IconTextData.TextData.Text` is a HUD number).
            if last.contains("island") || has(&["island_name", "islandname"]) {
                "Illiadunum"
            } else if has(&["desc", "objective", "body", "info", "tooltip"]) {
                "Use a street to connect the woodcutter to your trading post."
            } else if has(&["title", "header", "headline", "phase", "quest"]) {
                "Tutorial: Your Villa"
            } else if last.contains("name") {
                "Livia Aurelia"
            } else if has(&["time", "date"]) {
                "12:30"
            } else if has(&["lode", "badge"]) || (has(&["btnbasedata"]) && last == "text") {
                // Button and badge text is a small count in the HUD (the
                // minimap's lodes read 10 and 6, a construction button 30);
                // a figure would blow the round lode badges up to 68 px.
                "10"
            } else if has(&[
                "text",
                "icontext",
                "amount",
                "value",
                "count",
                "number",
                "contribution",
            ]) {
                // Most bare `Text` fields in the HUD are figures.
                "19,743"
            } else {
                "Sample text"
            }
            .to_owned(),
        )),
        _ => None,
    }
}

impl UiEditor {
    pub fn fill_preview(&mut self) -> usize {
        let mut n = 0;
        for f in &self.preview.files {
            let map = self.preview.values.entry(f.guid).or_default();
            for p in &f.props {
                if let Some(v) = sample(p) {
                    map.insert(p.uid, v);
                    n += 1;
                }
            }
        }
        self.relayout();
        n
    }

    pub fn reset_preview(&mut self) {
        self.preview.values.clear();
        self.relayout();
    }

    fn set_preview(&mut self, file: usize, prop: usize, v: Option<DefaultValue>) {
        let Some(f) = self.preview.files.get(file) else {
            return;
        };
        let uid = f.props[prop].uid;
        let map = self.preview.values.entry(f.guid).or_default();
        match v {
            Some(v) => {
                map.insert(uid, v);
            }
            None => {
                map.remove(&uid);
            }
        }
        self.relayout();
    }

    /// Applies the preview value being typed, as Enter does, when it says
    /// something other than what shows: a click into another field or pane
    /// commits and only Escape drops it (round 5: a value typed, then left
    /// for the next field, was silently thrown away). The flag filter just
    /// stops taking keys.
    pub(super) fn commit_preview(&mut self) -> anyhow::Result<()> {
        let Some(edit) = self.preview.edit.take() else {
            return Ok(());
        };
        if edit.field == FLAG_FILTER {
            return Ok(());
        }
        let (fi, pi) = edit.field;
        let Some(p) = self.preview.files.get(fi).and_then(|f| f.props.get(pi)) else {
            return Ok(());
        };
        let (kind, default, enum_asset) = (p.kind, p.default.clone(), p.enum_asset);
        let keys = self.preview_instances(fi);
        let Some(shown) = self
            .preview_shown(fi, pi, &keys)
            .map(|(v, _)| edit_text(&v, enum_asset))
        else {
            return Ok(());
        };
        if edit.text == shown {
            return Ok(());
        }
        match parse(kind, &default, &edit.text, enum_asset) {
            Ok(v) => {
                self.set_preview(fi, pi, Some(v));
                Ok(())
            }
            // Refused, it stays as typed.
            Err(e) => {
                self.preview.edit = Some(edit);
                Err(e)
            }
        }
    }

    /// The value preview input `pi` of file `fi` has on the screen, and
    /// where it comes from: set by hand, else what the layout evaluated
    /// for the open file (sample data, when that is on), else its stored
    /// default. The row shows it and a toggle flips it.
    pub(super) fn preview_shown(
        &self,
        fi: usize,
        pi: usize,
        keys: &[u64],
    ) -> Option<(DefaultValue, Shown)> {
        let f = self.preview.files.get(fi)?;
        let p = f.props.get(pi)?;
        if let Some(v) = self.preview.values.get(&f.guid).and_then(|m| m.get(&p.uid)) {
            return Some((v.clone(), Shown::Set));
        }
        // What the instances took, as the graph's Inputs node shows it.
        let root = self.layout_root();
        let mut took = keys.iter().filter_map(|&k| {
            super::scene::file_eval(&self.images, root, k)?
                .value(p.uid)
                .cloned()
        });
        let Some(first) = took.next() else {
            return Some((p.default.clone(), Shown::Default));
        };
        if took.any(|v| v != first) {
            return Some((p.default.clone(), Shown::Varies(keys.len())));
        }
        // The open screen's root takes sample data; an instance takes what
        // its container gives it (round 7: in place, Preview read 120 for
        // a SizeInX the canvas drew at 200).
        let from = if first == p.default {
            Shown::Default
        } else if Some(f.guid) == self.asset.guid() && self.scope.is_none() {
            Shown::Sampled
        } else {
            Shown::Given
        };
        Some((first, from))
    }

    /// Whether preview field `field` is the one being typed (a click on it
    /// then only places the caret).
    pub(super) fn preview_typing(&self, field: (usize, usize)) -> bool {
        self.preview.edit.as_ref().is_some_and(|e| e.field == field)
    }

    /// The layout instances whose inputs file group `fi` shows: the open
    /// file's root, or the instance edited in place; every copy of another
    /// template. Found by guid: a screen without inputs of its own has no
    /// "this file" group, and group 0 is a template (round 7).
    pub(super) fn preview_instances(&self, fi: usize) -> Vec<u64> {
        let Some(f) = self.preview.files.get(fi) else {
            return Vec::new();
        };
        if Some(f.guid) == self.asset.guid() {
            return vec![self.scope.map_or(0, |s| s.1)];
        }
        self.images
            .instances_of(self.layout_root().guid(), f.guid)
            .unwrap_or_default()
    }

    /// Typing into a preview value; false when none is being edited.
    pub fn preview_key(&mut self, event: &KeyDownEvent) -> bool {
        let Some(edit) = &mut self.preview.edit else {
            return false;
        };
        // Tab finishes a value as Enter does (round 14: it did nothing).
        if event.keystroke.key == "tab" && edit.field != FLAG_FILTER {
            if let Err(e) = self.commit_preview() {
                self.message = Some((format!("Not applied: {e:#}"), true));
            }
            return true;
        }
        let event = edit.key_down(event);
        // The flag filter narrows the list as it is typed.
        if edit.field == FLAG_FILTER {
            self.preview.flag_filter = edit.text.clone();
            if matches!(event, EditEvent::Commit | EditEvent::Cancel) {
                self.preview.edit = None;
            }
            return true;
        }
        match event {
            // Refused, the field keeps the text to fix.
            EditEvent::Commit => {
                if let Err(e) = self.commit_preview() {
                    self.message = Some((format!("Not applied: {e:#}"), true));
                }
            }
            EditEvent::Cancel => self.preview.edit = None,
            EditEvent::Changed | EditEvent::Ignored => {}
        }
        true
    }
}

/// Where a preview value shown comes from (see `UiEditor::preview_shown`):
/// set here, sample data at the screen's root, given by the container
/// that shows the template, or the stored default. `Varies(n)`: the n
/// copies of a template take different values, and the default shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shown {
    Set,
    Sampled,
    Given,
    Varies(usize),
    Default,
}

/// The flag probe of the open screen: what it was run for, the run in
/// progress, and the last result.
#[derive(Default)]
pub struct FlagProbe {
    key: Option<u64>,
    /// A state not probed yet and since when it holds: the probe waits for
    /// edits (a drag of nudges, flags switched in a row) to pause.
    waiting: Option<(u64, std::time::Instant)>,
    stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub effects: Option<std::sync::Arc<Vec<FlagEffect>>>,
    pub running: bool,
    /// The rows that changed the screen when the first flag was set by
    /// hand, grouped as listed. While any is set the list keeps them in
    /// place: a re-probe against the new state would otherwise drop the
    /// row the modder needs next (the flag's pair) mid-task.
    held: Option<Vec<FlagEffect>>,
}

impl FlagProbe {
    /// Holds the listed rows before the first flag is set by hand.
    fn hold(&mut self) {
        if self.held.is_none()
            && let Some(effects) = &self.effects
        {
            self.held = Some(
                grouped(effects.iter().filter(|e| e.any()).collect())
                    .into_iter()
                    .cloned()
                    .collect(),
            );
        }
    }

    /// Lets the list re-sort once no flag it shows is set by hand.
    fn release_if_unset(&mut self) {
        let shown = |p: &str| {
            let has = |l: &[FlagEffect]| l.iter().any(|e| e.path == p);
            self.held.as_deref().is_some_and(has) || self.effects.as_deref().is_some_and(|e| has(e))
        };
        if !game_flags().iter().any(|f| f.2 && shown(&f.0)) {
            self.held = None;
        }
    }
}

/// Flags in list order: by owner, then name.
fn grouped(mut list: Vec<&FlagEffect>) -> Vec<&FlagEffect> {
    list.sort_by(|a, b| flag_parts(&a.path).cmp(&flag_parts(&b.path)));
    list
}

/// The rows listed as changing the screen, each with what it does now
/// (`None`: nothing here any more). Held rows keep their place; rows that
/// started changing the screen since go after them.
fn live_rows<'a>(
    effects: &'a [FlagEffect],
    held: Option<&'a [FlagEffect]>,
) -> Vec<(&'a FlagEffect, Option<&'a FlagEffect>)> {
    let now = |p: &str| effects.iter().find(|e| e.path == p && e.any());
    let Some(held) = held else {
        return grouped(effects.iter().filter(|e| e.any()).collect())
            .into_iter()
            .map(|e| (e, Some(e)))
            .collect();
    };
    let mut rows: Vec<_> = held.iter().map(|e| (e, now(&e.path))).collect();
    let new = effects
        .iter()
        .filter(|e| e.any() && !held.iter().any(|h| h.path == e.path))
        .collect();
    rows.extend(grouped(new).into_iter().map(|e| (e, Some(e))));
    rows
}

/// "shows A, B · hides C": what switching a flag does, three names a kind.
fn effect_line(e: &FlagEffect) -> String {
    let names = |verb: &str, l: &[String]| -> Option<String> {
        // Unnamed widgets say nothing; their named neighbours do.
        let named: Vec<&str> = l
            .iter()
            .map(String::as_str)
            .filter(|n| *n != "widget")
            .collect();
        let shown = named.len().min(3);
        match named.len() {
            0 if l.is_empty() => None,
            0 => Some(format!(
                "{verb} {}",
                crate::project::plural(l.len(), "widget", "widgets")
            )),
            n if n > shown => Some(format!(
                "{verb} {} +{}",
                named[..shown].join(", "),
                n - shown
            )),
            _ => Some(format!("{verb} {}", named.join(", "))),
        }
    };
    [
        names("shows", &e.shows),
        names("hides", &e.hides),
        names("changes", &e.changes),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}

/// The last two steps of a data path: the object and its field.
/// A flag path as the Game data list groups it: who owns the flag, and the
/// flag under that owner. Game data (`GameContext.Menu.Button.IsNew`) is
/// owned by its object; a template input the screen leaves unbound
/// (`Scene.Holder.Btn.-Data.IsNew`, `-States.IsFocused` on the screen's own
/// template) by the instance that reads it, `-` marking the input.
fn flag_parts(path: &str) -> (&str, &str) {
    let input = if path.starts_with('-') {
        Some(0)
    } else {
        path.find(".-").map(|i| i + 1)
    };
    if let Some(i) = input {
        let field = &path[i + 1..];
        // `Data` is every template's main input; other inputs keep their name.
        let field = field.strip_prefix("Data.").unwrap_or(field);
        return (path[..i].trim_end_matches('.'), field);
    }
    let path = path.strip_prefix("GameContext.").unwrap_or(path);
    match path.rsplit_once('.') {
        Some((owner, field)) => (owner, field),
        None => ("", path),
    }
}

/// Each owner's shortest trailing run of path segments that no other owner
/// in `owners` ends with, joined with ` › `: `Pins_LeftV03 › … ›
/// ConstrMenuPinSlotContainer` needs its holder, a lone button does not.
fn owner_labels(owners: &[&str]) -> Vec<String> {
    let segs: Vec<Vec<&str>> = owners
        .iter()
        .map(|o| o.split('.').filter(|s| !s.is_empty()).collect())
        .collect();
    segs.iter()
        .map(|mine| {
            if mine.is_empty() {
                return "This screen".to_owned();
            }
            let n = (1..=mine.len())
                .find(|&n| {
                    let tail = &mine[mine.len() - n..];
                    segs.iter()
                        .filter(|o| *o != mine)
                        .all(|o| !o.ends_with(tail))
                })
                .unwrap_or(mine.len());
            let tail = &mine[mine.len() - n..];
            // Two segments read better than an ellipsis and one name.
            match n {
                1 => tail[0].to_owned(),
                _ if n < mine.len() => format!("… › {}", tail.join(" › ")),
                _ => tail.join(" › "),
            }
        })
        .collect()
}

impl Editor {
    /// Starts finding which game-data flags change the open screen, when
    /// the screen, its preview values or the hand-set flags changed since
    /// the last run. The run lays the screen out once per flag on a
    /// background thread; a newer run stops an older one.
    pub(super) fn ensure_flag_probe(&mut self, cx: &mut Context<Self>) {
        use std::hash::{BuildHasher, Hash, Hasher};
        let Some(ui) = &mut self.ui else {
            return;
        };
        if !ui.sample || ui.nodes.is_empty() {
            return;
        }
        let mut h = foldhash::fast::FixedState::with_seed(7).build_hasher();
        ui.layout_root().bytes().hash(&mut h);
        ui.asset.bytes().hash(&mut h);
        format!("{:?}", ui.preview.values).hash(&mut h);
        ui.shown.hash(&mut h);
        game_data_generation().hash(&mut h);
        let key = h.finish();
        let probe = &mut ui.preview.probe;
        if probe.key == Some(key) {
            probe.waiting = None;
            return;
        }
        // The first probe of a screen runs at once; later ones after a
        // pause in editing.
        const QUIET: std::time::Duration = std::time::Duration::from_millis(1200);
        if probe.effects.is_some() {
            match probe.waiting {
                Some((k, since)) if k == key && since.elapsed() >= QUIET => {}
                Some((k, _)) if k == key => return,
                _ => {
                    probe.waiting = Some((key, std::time::Instant::now()));
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(QUIET).await;
                        this.update(cx, |_, cx| cx.notify()).ok();
                    })
                    .detach();
                    return;
                }
            }
        }
        probe.waiting = None;
        if let Some(stop) = probe.stop.take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        probe.key = Some(key);
        probe.stop = Some(stop.clone());
        probe.running = true;
        let (install, asset) = (ui.install.clone(), ui.layout_root().clone());
        let (values, shown) = (ui.preview.values.clone(), ui.shown);
        cx.spawn(async move |this, cx| {
            let effects = cx
                .background_executor()
                .spawn(async move {
                    load_scene_content(&install);
                    probe_flags(&install, &asset, &values, shown, &stop)
                })
                .await;
            this.update(cx, |this, cx| {
                if let Some(ui) = &mut this.ui
                    && ui.preview.probe.key == Some(key)
                    && let Some(effects) = effects
                {
                    let probe = &mut ui.preview.probe;
                    probe.effects = Some(std::sync::Arc::new(effects));
                    probe.running = false;
                    probe.stop = None;
                    // Its layout trees are gone: their memory goes back.
                    let _ = super::release_heap(true);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The inspector's Game data section: the yes/no game data this screen
    /// reads that changes what it shows (the quest tracker's
    /// `ObjectivesListData.IsVisible`, the minimap's `MinimapContent`), each
    /// with what switching it does. Flags that change nothing here fold
    /// away: most of what a screen reads sits in hidden template parts or
    /// feeds events the editor does not run.
    pub(super) fn game_data_section(
        &self,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let ui = self.ui.as_ref()?;
        if !ui.sample {
            return None;
        }
        let probe = &ui.preview.probe;
        let by_hand: std::collections::BTreeMap<String, bool> = game_flags()
            .into_iter()
            .filter(|f| f.2)
            .map(|f| (f.0, f.1))
            .collect();
        let effects = probe.effects.clone().unwrap_or_default();
        let live = live_rows(&effects, probe.held.as_deref());
        let heading_text = match (&probe.effects, probe.running) {
            (None, _) => "Game data · checking…".to_owned(),
            (Some(_), running) => format!(
                "Game data · {} that change this screen{}",
                live.len(),
                if running { " · updating…" } else { "" }
            ),
        };
        // Open by default with nothing selected; one folded line under a
        // selected widget's Data, where its own fields come first.
        let (id, open) = if selected {
            ("ui-game-data-selected", false)
        } else {
            ("ui-game-data", true)
        };
        let (heading, open) =
            gpui_editor_kit::widgets::collapsible(id, heading_text, open, window, cx);
        if !open {
            return Some(div().child(heading));
        }
        let mut d = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading)
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("What the game decides at run time. Switch one to see that state of the screen. Not saved."),
            );
        let needle = ui.preview.flag_filter.to_lowercase();
        let matches = |p: &str| needle.is_empty() || p.to_lowercase().contains(&needle);
        if effects.len() > 8 || !needle.is_empty() {
            let filter = ui.preview.flag_filter.clone();
            let editing = ui
                .preview
                .edit
                .as_ref()
                .filter(|e| e.field == FLAG_FILTER)
                .map(|e| e.display());
            d = d.child(
                text_field("ui-game-data-filter", &filter, editing, "Filter flags").on_click(
                    cx.listener(move |this, _, window, cx| {
                        if this
                            .ui
                            .as_ref()
                            .is_some_and(|ui| ui.preview_typing(FLAG_FILTER))
                        {
                            return;
                        }
                        this.focus.focus(window, cx);
                        this.keys_to(super::Keys::Canvas);
                        if let Some(ui) = &mut this.ui {
                            ui.preview.edit =
                                Some(TextEdit::new(FLAG_FILTER, filter.clone(), Filter::Text));
                        }
                        cx.notify();
                    }),
                ),
            );
        }
        let row = |id: String, e: &FlagEffect, detail: Option<String>, cx: &mut Context<Self>| {
            // A hand-set value shows at once; the probe catches up later.
            let set = by_hand.get(&e.path).copied();
            let (path, on) = (e.path.clone(), set.unwrap_or(e.value));
            let effect = e.clone();
            let field = flag_parts(&path).1.to_owned();
            let set = set.is_some();
            let toggle = path.clone();
            let mut line = div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(if set {
                            theme::get().accent
                        } else {
                            theme::get().text
                        })
                        .child(field),
                )
                .child(
                    button(if on { "on" } else { "off" }, on).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.keys_to(super::Keys::Canvas);
                            if let Some(ui) = &mut this.ui {
                                ui.preview.probe.hold();
                            }
                            set_game_flag(&toggle, Some(!on));
                            if let Some(ui) = &mut this.ui {
                                ui.relayout();
                                ui.relayout_screens();
                            }
                            cx.notify();
                        },
                    )),
                );
            if set {
                let reset = path.clone();
                line = line.child(
                    button("×", false)
                        .tooltip(Tooltip::text("Back to sample data", "", ""))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.keys_to(super::Keys::Canvas);
                            set_game_flag(&reset, None);
                            if let Some(ui) = &mut this.ui {
                                ui.preview.probe.release_if_unset();
                                ui.relayout();
                                ui.relayout_screens();
                            }
                            cx.notify();
                        })),
                );
            }
            div()
                .id(SharedString::from(id))
                .flex()
                .flex_col()
                .py(px(2.))
                .pl(px(8.))
                .tooltip(Tooltip::text(path, "", ""))
                .child(line)
                .when_some(detail, |d, t| {
                    // A click finds what it changes on the canvas; a held
                    // row that changes nothing now has nothing to find.
                    let live = effect.any();
                    d.child(
                        div()
                            .id(SharedString::from(format!("{}-effect", effect.path)))
                            .text_size(px(10.))
                            .text_color(theme::get().muted)
                            .when(live, |d| {
                                d.cursor_pointer()
                                    .hover(|s| s.text_color(theme::get().text).underline())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.keys_to(super::Keys::Canvas);
                                        if let Some(ui) = &mut this.ui {
                                            let m = ui.show_effect(&effect);
                                            ui.message = Some((m, false));
                                        }
                                        cx.notify();
                                    }))
                            })
                            .child(format!("Switching it {t}")),
                    )
                })
        };
        if probe.effects.is_none() {
            d = d.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("Finding the flags that change this screen…"),
            );
        } else if live.is_empty() {
            d = d.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::get().muted)
                    .child("No game data flag changes what this screen shows."),
            );
        }
        // Rows sit under the object or template instance that owns them,
        // so `IsLocked` of three pin slots reads as three owners, not one
        // name three times.
        let owners: Vec<&str> = {
            let mut o: Vec<&str> = effects.iter().map(|e| flag_parts(&e.path).0).collect();
            o.sort_unstable();
            o.dedup();
            o
        };
        let labels = owner_labels(&owners);
        let header = |list: &str, owner: &str| {
            let i = owners.binary_search(&owner).unwrap_or(0);
            div()
                .id(SharedString::from(format!("ui-flag-{list}-owner-{i}")))
                .pt_1()
                .truncate()
                .text_size(px(10.))
                .text_color(theme::get().muted)
                .tooltip(Tooltip::text(owner.to_owned(), "", ""))
                .child(labels.get(i).cloned().unwrap_or_default())
        };
        // How many are set by hand, with Reset all beside it, on one row
        // that is always there: round 10 put the "list holds still" note
        // above the rows when the first flag was set (every row dropped
        // under the pointer) and Reset all sixty rows below it.
        let elsewhere = by_hand
            .keys()
            .filter(|p| !effects.iter().any(|e| &e.path == *p))
            .count();
        let said = match (by_hand.len(), elsewhere) {
            (0, _) => "None set by hand".to_owned(),
            (n, 0) => format!("{n} set by hand; the list holds still"),
            (n, e) => format!("{n} set by hand, {e} of them for other screens"),
        };
        d = d.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .h(px(26.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .child(said),
                )
                .when(!by_hand.is_empty(), |row| {
                    row.child(
                        button("Reset all", false).on_click(cx.listener(|this, _, _, cx| {
                            this.keys_to(super::Keys::Canvas);
                            reset_game_flags();
                            if let Some(ui) = &mut this.ui {
                                ui.preview.probe.release_if_unset();
                                ui.relayout();
                                ui.relayout_screens();
                            }
                            cx.notify();
                        })),
                    )
                }),
        );
        let mut last = None;
        for (i, (e, now)) in live.iter().filter(|r| matches(&r.0.path)).enumerate() {
            let owner = flag_parts(&e.path).0;
            if last != Some(owner) {
                d = d.child(header("live", owner));
                last = Some(owner);
            }
            // A held row that stopped mattering keeps its place, says so,
            // and keeps the flag's current value.
            let (shown, detail) = match now {
                Some(n) => (std::borrow::Cow::Borrowed(*n), effect_line(n)),
                None => (
                    std::borrow::Cow::Owned(FlagEffect {
                        path: e.path.clone(),
                        value: effects
                            .iter()
                            .find(|x| x.path == e.path)
                            .map_or(e.value, |x| x.value),
                        ..Default::default()
                    }),
                    "changes nothing here now".to_owned(),
                ),
            };
            d = d.child(row(format!("ui-flag-{i}"), &shown, Some(detail), cx));
        }
        let idle: Vec<&FlagEffect> = grouped(
            effects
                .iter()
                .filter(|e| {
                    !e.any() && matches(&e.path) && !live.iter().any(|r| r.0.path == e.path)
                })
                .collect(),
        );
        if !idle.is_empty() {
            let (heading, open) = gpui_editor_kit::widgets::collapsible(
                "ui-game-data-idle",
                format!("No visible effect here · {}", idle.len()),
                false,
                window,
                cx,
            );
            d = d.child(heading);
            if open {
                d = d.child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme::get().muted)
                        .child("Read by this screen, but switching them changes nothing it draws: they sit in hidden parts, feed events the editor does not run, or another flag decides first."),
                );
                let mut last = None;
                for (i, e) in idle.iter().enumerate() {
                    let owner = flag_parts(&e.path).0;
                    if last != Some(owner) {
                        d = d.child(header("idle", owner));
                        last = Some(owner);
                    }
                    d = d.child(row(format!("ui-flag-idle-{i}"), e, None, cx));
                }
            }
        }
        Some(d)
    }

    /// The inspector's Preview data section.
    pub(super) fn preview_section(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let ui = self.ui.as_ref()?;
        let pv = &ui.preview;
        if pv.files.is_empty() {
            return None;
        }
        let set = pv.values.values().map(|m| m.len()).sum::<usize>();
        // Folded until asked for: a screen's generated properties run to
        // hundreds and are not what the inspector is for.
        let (heading, open) = gpui_editor_kit::widgets::collapsible(
            "ui-preview-data",
            format!(
                "Preview data · {} · {set} set",
                crate::project::plural(
                    pv.files.iter().map(|f| f.props.len()).sum::<usize>(),
                    "input",
                    "inputs"
                )
            ),
            false,
            window,
            cx,
        );
        if !open {
            return Some(div().child(heading));
        }
        let mut d = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(heading)
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::get().muted)
                    .child("Values the screen's templates take in, set by hand. Values the templates work out themselves are left out. Not saved."),
            )
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        button("Fill sample data", false)
                            .tooltip(Tooltip::text(
                                "Fill sample data",
                                "",
                                "Plausible values by type and name: texts, counts, sizes, true for Is/Show/Visible flags",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.keys_to(super::Keys::Canvas);
                                if let Some(ui) = &mut this.ui {
                                    let n = ui.fill_preview();
                                    this.message(format!("Preview: {n} values filled"), false);
                                }
                                cx.notify();
                            })),
                    )
                    .child(button("Reset", false).on_click(cx.listener(|this, _, _, cx| {
                        this.keys_to(super::Keys::Canvas);
                        if let Some(ui) = &mut this.ui {
                            ui.reset_preview();
                        }
                        cx.notify();
                    }))),
            );
        for (fi, f) in pv.files.iter().enumerate() {
            let map = pv.values.get(&f.guid);
            let set = map.map_or(0, |m| {
                f.props.iter().filter(|p| m.contains_key(&p.uid)).count()
            });
            // One fold per file: the open file's inputs show, the
            // templates' wait until asked for.
            let (heading, open) = gpui_editor_kit::widgets::collapsible(
                SharedString::from(format!("ui-preview-file-{}", f.guid)),
                match set {
                    0 => format!(
                        "{} · {}",
                        f.owner,
                        crate::project::plural(f.props.len(), "input", "inputs")
                    ),
                    n => format!(
                        "{} · {} · {n} set",
                        f.owner,
                        crate::project::plural(f.props.len(), "input", "inputs")
                    ),
                },
                Some(f.guid) == ui.asset.guid() || set > 0,
                window,
                cx,
            );
            d = d.child(heading);
            if !open {
                continue;
            }
            let keys = ui.preview_instances(fi);
            for (pi, p) in f.props.iter().enumerate() {
                let current = map.and_then(|m| m.get(&p.uid));
                let (shown, from) = ui
                    .preview_shown(fi, pi, &keys)
                    .unwrap_or((p.default.clone(), Shown::Default));
                let value = &shown;
                let field: AnyElement = if p.kind == BOOL {
                    let on = matches!(value, DefaultValue::Bool(b) if *b != 0);
                    button(if on { "true" } else { "false" }, on)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.keys_to(super::Keys::Canvas);
                            if let Some(ui) = &mut this.ui {
                                ui.set_preview(fi, pi, Some(DefaultValue::Bool(u8::from(!on))));
                            }
                            cx.notify();
                        }))
                        .into_any_element()
                } else {
                    let text = edit_text(value, p.enum_asset);
                    let editing = pv
                        .edit
                        .as_ref()
                        .filter(|e| e.field == (fi, pi))
                        .map(|e| e.display());
                    text_field(
                        SharedString::from(format!("preview-{fi}-{pi}")),
                        &text,
                        editing,
                        "",
                    )
                    .flex_1()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this
                            .ui
                            .as_ref()
                            .is_some_and(|ui| ui.preview_typing((fi, pi)))
                        {
                            return;
                        }
                        this.focus.focus(window, cx);
                        this.keys_to(super::Keys::Canvas);
                        if let Some(ui) = &mut this.ui {
                            ui.preview.edit =
                                Some(TextEdit::new((fi, pi), text.clone(), Filter::Text));
                        }
                        cx.notify();
                    }))
                    .into_any_element()
                };
                // Its own id: a button's id is its label, and rows of
                // equal values ("false", "×") took each other's clicks.
                let mut row = div()
                    .id(SharedString::from(format!(
                        "preview-row-{}-{}",
                        f.guid, p.uid
                    )))
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .w(px(130.))
                            .flex_none()
                            .overflow_hidden()
                            .text_size(px(11.))
                            .text_color(if current.is_some() {
                                theme::get().text
                            } else {
                                theme::get().muted
                            })
                            .child(p.name.clone()),
                    )
                    .child(field)
                    // Where an unset value comes from: what sample data or
                    // the container gives it, not the stored default (round
                    // 6 showed "false" over a background drawn as on).
                    .when_some(
                        match from {
                            Shown::Sampled => Some("sample".to_owned()),
                            Shown::Given => Some("from parent".to_owned()),
                            Shown::Varies(n) => Some(format!("varies · {n} copies")),
                            Shown::Set | Shown::Default => None,
                        },
                        |r, tag| {
                            r.child(
                                div()
                                    .flex_none()
                                    .text_size(px(10.))
                                    .text_color(theme::get().muted)
                                    .child(tag),
                            )
                        },
                    );
                if current.is_some() {
                    row = row.child(button("×", false).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.keys_to(super::Keys::Canvas);
                            if let Some(ui) = &mut this.ui {
                                ui.set_preview(fi, pi, None);
                            }
                            cx.notify();
                        },
                    )));
                }
                d = d.child(row);
            }
        }
        Some(d)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BOOL, FLOAT, INT, Prop, SAMPLE_ICON, STRING, VEC2, edit_text, parse, sample, sample_value,
    };
    use crate::ui::UiEditor;
    use crate::ui::test_support::{BUTTON, ICON_ROW, install, key, open};
    use anno_formats::phx::DefaultValue;
    use gpui_editor_kit::field::{Filter, TextEdit};

    fn prop(name: &str, kind: u32, default: DefaultValue) -> Prop {
        Prop {
            uid: 1,
            name: name.into(),
            kind,
            default,
            enum_asset: None,
        }
    }

    /// The Preview pane's input `name` as (file, property), in the first
    /// file that has one.
    fn preview_input(ui: &UiEditor, name: &str) -> (usize, usize) {
        ui.preview
            .files
            .iter()
            .enumerate()
            .find_map(|(fi, f)| Some((fi, f.props.iter().position(|p| p.name == name)?)))
            .unwrap_or_else(|| panic!("no Preview input {name}"))
    }

    #[test]
    fn flag_rows_name_their_owner_without_repeats() {
        use super::{flag_parts, owner_labels};
        let paths = [
            "-States.IsFocused",
            "Scene.Container.Main.AccessBtns_Left.2nd_Btn.-Data.IsNew",
            "Scene.Container.Main.Pins_LeftV03.HUDPinsArray.Slot.-Data.SlotStates.IsLocked",
            "Scene.Container.Main.Pins_RightV03.HUDPinsArray.Slot.-Data.SlotStates.IsLocked",
            "GameContext.Menu.AccessButtons.BuildingsButton.BtnInteraction.IsLocked",
            "GameContext.Menu.AccessButtons.ShipsButton.BtnInteraction.IsLocked",
            "GameContext.Menu.AccessButtons.ShipsButton.IsNew",
        ];
        let parts: Vec<(&str, &str)> = paths.iter().map(|p| flag_parts(p)).collect();
        assert_eq!(parts[0], ("", "States.IsFocused"));
        assert_eq!(parts[1].1, "IsNew");
        assert_eq!(parts[2].1, "SlotStates.IsLocked");
        assert_eq!(
            parts[4],
            (
                "Menu.AccessButtons.BuildingsButton.BtnInteraction",
                "IsLocked"
            )
        );
        let mut owners: Vec<&str> = parts.iter().map(|p| p.0).collect();
        owners.dedup();
        let labels = owner_labels(&owners);
        assert_eq!(
            labels,
            [
                "This screen",
                "2nd_Btn",
                "… › Pins_LeftV03 › HUDPinsArray › Slot",
                "… › Pins_RightV03 › HUDPinsArray › Slot",
                "… › BuildingsButton › BtnInteraction",
                "… › ShipsButton › BtnInteraction",
                "ShipsButton",
            ]
        );
        // Owner and flag together name each path once.
        let mut rows: Vec<(String, &str)> = parts
            .iter()
            .map(|(o, f)| {
                (
                    labels[owners.iter().position(|x| x == o).unwrap()].clone(),
                    *f,
                )
            })
            .collect();
        rows.sort();
        rows.dedup();
        assert_eq!(rows.len(), paths.len());
    }

    #[test]
    fn rows_hold_still_while_flags_are_set_by_hand() {
        use super::{FlagEffect, FlagProbe, live_rows, set_game_flag};
        let flag = |path: &str, shows: &[&str]| FlagEffect {
            path: path.into(),
            shows: shows.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let paths = |rows: &[(&FlagEffect, Option<&FlagEffect>)]| -> Vec<String> {
            rows.iter().map(|r| r.0.path.clone()).collect()
        };
        // Flags are process-wide: this test owns the HoldTest paths only.
        let before = vec![
            flag("GameContext.HoldTest.HasJumpTo", &["JumpTo"]),
            flag("GameContext.HoldTest.JumpTo.IsEnabled", &["JumpGlow"]),
            flag("GameContext.HoldTest.IsNew", &[]),
        ];
        let mut probe = FlagProbe {
            effects: Some(std::sync::Arc::new(before.clone())),
            ..Default::default()
        };
        assert_eq!(
            paths(&live_rows(&before, None)),
            ["HoldTest.HasJumpTo", "HoldTest.JumpTo.IsEnabled"].map(|p| format!("GameContext.{p}"))
        );
        // Switching HasJumpTo off: the re-probe says IsEnabled changes
        // nothing now and IsNew started to. The list keeps IsEnabled where
        // it was, says it changes nothing, and adds IsNew after.
        probe.hold();
        set_game_flag("GameContext.HoldTest.HasJumpTo", Some(false));
        let after = vec![
            flag("GameContext.HoldTest.HasJumpTo", &["JumpTo"]),
            flag("GameContext.HoldTest.JumpTo.IsEnabled", &[]),
            flag("GameContext.HoldTest.IsNew", &["NewBadge"]),
        ];
        probe.effects = Some(std::sync::Arc::new(after.clone()));
        let rows = live_rows(&after, probe.held.as_deref());
        assert_eq!(
            paths(&rows),
            [
                "HoldTest.HasJumpTo",
                "HoldTest.JumpTo.IsEnabled",
                "HoldTest.IsNew"
            ]
            .map(|p| format!("GameContext.{p}"))
        );
        assert!(
            rows[1].1.is_none(),
            "a held row says it changes nothing now"
        );
        assert_eq!(
            rows[2].1.map(|e| e.shows.clone()),
            Some(vec!["NewBadge".to_owned()])
        );
        // A second switch keeps the first hold.
        probe.hold();
        assert_eq!(probe.held.as_ref().map(Vec::len), Some(2));
        // Unsetting the last flag lets the list sort itself again.
        set_game_flag("GameContext.HoldTest.HasJumpTo", None);
        probe.release_if_unset();
        assert!(probe.held.is_none());
        assert_eq!(
            paths(&live_rows(&after, probe.held.as_deref())),
            ["HoldTest.HasJumpTo", "HoldTest.IsNew"].map(|p| format!("GameContext.{p}"))
        );
    }

    #[test]
    fn game_flags_set_by_hand_win_and_are_listed() {
        use super::{game_data_generation, game_flags, set_game_flag};
        // Sampling lists the flag with what sample data says.
        let path = "GameFlagTest.Panel.IsPanelOpen";
        let sampled = sample_value(path, BOOL, &DefaultValue::None);
        assert!(matches!(sampled, Some(DefaultValue::Bool(_))));
        assert!(game_flags().iter().any(|f| f.0 == path && !f.2));
        // Set by hand: answered first, marked, and layout sees a new
        // generation.
        let before = game_data_generation();
        set_game_flag(path, Some(true));
        assert!(game_data_generation() > before);
        assert_eq!(
            sample_value(path, BOOL, &DefaultValue::None),
            Some(DefaultValue::Bool(1))
        );
        assert!(game_flags().iter().any(|f| f.0 == path && f.1 && f.2));
        set_game_flag(path, None);
        assert_eq!(sample_value(path, BOOL, &DefaultValue::None), sampled);
    }

    #[test]
    fn samples_follow_type_and_name() {
        let text = |s: &str| Some(DefaultValue::Str(s.into()));
        let empty = DefaultValue::Str(String::new());
        assert_eq!(
            sample(&prop("QuestTitle", STRING, empty)),
            text("Tutorial: Your Villa")
        );
        // Game-object fields come as a path; the field and its context
        // pick the sample.
        let field = |path: &str, kind| sample_value(path, kind, &DefaultValue::None);
        assert_eq!(field("Data.IconText.TextData.Text", STRING), text("19,743"));
        assert_eq!(field("root.-TextLayout.Overflow_X", u32::MAX), None);
        assert_eq!(
            field("Data.BtnBaseData.Data.TextData.Text", STRING),
            text("10")
        );
        assert_eq!(field("HUD_Island.Island_Name", STRING), text("Illiadunum"));
        assert_eq!(field("Data.Icon", STRING), text(SAMPLE_ICON));
        assert_eq!(
            field("Content.IsVisible", BOOL),
            Some(DefaultValue::Bool(0))
        );
        assert_eq!(
            sample(&prop("IsScrollVisible", 0, DefaultValue::Bool(0))),
            Some(DefaultValue::Bool(1))
        );
        // State flags the game raises only on events start off.
        assert_eq!(
            sample(&prop("-IsLocked", 0, DefaultValue::Bool(1))),
            Some(DefaultValue::Bool(0))
        );
        // Authored numbers stay; an empty progress gets a partial fill.
        let width = prop("ContentWidth", FLOAT, DefaultValue::F32(420f32.to_bits()));
        assert_eq!(sample(&width), None);
        let progress = prop("Progress", FLOAT, DefaultValue::F32(0));
        assert_eq!(
            sample(&progress),
            Some(DefaultValue::F32(0.65f32.to_bits()))
        );
    }

    // Needs the game: a preview value for a String template property
    // reaches the label it is bound to, and resetting brings the stored
    // text back.
    #[test]
    fn preview_texts_reach_bound_labels() {
        let Some(mut ui) = open("21dcf70a-6a03-4a75-aa15-2c705d23dde5") else {
            return;
        };
        let shows = |ui: &UiEditor, t: &str| {
            ui.nodes
                .iter()
                .filter(|n| n.text.as_deref() == Some(t))
                .count()
        };
        assert!(shows(&ui, "100") > 0, "the stored value text");
        let (fi, pi) = preview_input(&ui, "-ValueLabel");
        ui.set_preview(fi, pi, Some(DefaultValue::Str("4,321".into())));
        assert!(shows(&ui, "4,321") > 0, "the preview text");
        ui.reset_preview();
        assert_eq!(shows(&ui, "4,321"), 0);
    }

    // Needs the game: the camera stays where the user left it. A preview
    // value that grows the icon (Icon_Size) leaves a fitted view alone; a
    // framed view survives the pane resizing; a page that changes size (a
    // root Size edit) keeps the view. Home fits again.
    #[test]
    fn edits_and_pane_resizes_never_move_the_camera() {
        use gpui_editor_kit::gpui::{Bounds, point, px, size};
        let Some(mut ui) = open(ICON_ROW) else { return };
        let pane = size(px(800.), px(600.));
        ui.canvas_bounds = Bounds::new(point(px(0.), px(0.)), pane);
        ui.canvas.view.fit(pane);
        let camera = |ui: &UiEditor| (ui.canvas.view.zoom, ui.canvas.view.pan);
        let fitted = camera(&ui);
        let (fi, pi) = preview_input(&ui, "-Icon_Size");
        let p = &ui.preview.files[fi].props[pi];
        let v = parse(p.kind, &p.default, "48", None).unwrap();
        ui.set_preview(fi, pi, Some(v));
        assert!(!ui.canvas.view.fit(pane));
        assert_eq!(camera(&ui), fitted, "a preview edit");
        // Home: the whole page, which is what a page size change refitted.
        ui.canvas.view.region = None;
        ui.canvas.view.refit();
        assert!(ui.canvas.view.fit(pane));
        let home = camera(&ui);
        let [w, h] = ui.canvas.view.size;
        ui.canvas.view.resize([w / 2., h]);
        assert!(!ui.canvas.view.fit(pane));
        assert_eq!(camera(&ui), home, "a page size change");
        ui.frame_rect(ui.nodes[1].rect);
        let framed = camera(&ui);
        assert!(!ui.canvas.view.fit(size(px(900.), px(500.))));
        assert_eq!(camera(&ui), framed, "a pane resize after framing");
        ui.canvas.view.refit();
        assert!(ui.canvas.view.fit(pane), "Home fits the page as it is now");
    }

    // Needs the game: a preview value typed and then left for another
    // field or pane is applied, as Enter applies it (round 5 dropped it
    // silently); one left unchanged sets nothing; one that does not parse
    // is refused with a message.
    #[test]
    fn a_preview_value_left_for_another_field_applies() {
        let Some(mut ui) = open(ICON_ROW) else { return };
        let (fi, pi) = preview_input(&ui, "-Icon_Size");
        let set = |ui: &UiEditor| ui.preview.values.values().map(|m| m.len()).sum::<usize>();
        let typed = |ui: &mut UiEditor, text: &str| {
            ui.preview.edit = Some(TextEdit::new((fi, pi), text, Filter::Text));
            ui.take_keys(crate::ui::Keys::Canvas);
            assert!(ui.preview.edit.is_none());
        };
        let shown = edit_text(&ui.preview.files[fi].props[pi].default, None);
        typed(&mut ui, &shown);
        assert_eq!(set(&ui), 0, "unchanged sets nothing");
        typed(&mut ui, "40");
        assert_eq!(set(&ui), 1);
        let p = &ui.preview.files[fi].props[pi];
        let v = ui.preview.values[&ui.preview.files[fi].guid][&p.uid].clone();
        assert_eq!(edit_text(&v, None), "40");
        typed(&mut ui, "not a number");
        assert_eq!(set(&ui), 1);
        assert!(ui.message.as_ref().is_some_and(|m| m.1), "refused, said");
    }

    // Needs the game: Preview data shows the value the screen uses. The
    // icon row's -Bg_IsVisible is stored false and sampled true: it shows
    // true, from sample data (round 6 showed false over a drawn
    // background); set by hand it shows what was set.
    #[test]
    fn preview_data_shows_what_the_screen_uses() {
        use super::Shown;
        let Some(mut ui) = open(ICON_ROW) else { return };
        let (fi, pi) = preview_input(&ui, "-Bg_IsVisible");
        assert_eq!(fi, 0, "the screen's own input");
        assert_eq!(ui.preview.files[0].props[pi].default, DefaultValue::Bool(0));
        let keys = ui.preview_instances(0);
        assert_eq!(keys, [0], "the screen's root");
        let (v, from) = ui.preview_shown(0, pi, &keys).unwrap();
        assert_eq!((v, from), (DefaultValue::Bool(1), Shown::Sampled));
        ui.set_preview(0, pi, Some(DefaultValue::Bool(0)));
        let (v, from) = ui.preview_shown(0, pi, &keys).unwrap();
        assert_eq!((v, from), (DefaultValue::Bool(0), Shown::Set));
    }

    // Needs the game: in place, Preview data shows what the instance being
    // edited takes from its container, and a reset goes back to it (round
    // 7: QT_Objective_IconText's -SizeInX read 120, the stored default,
    // while the canvas and graph used 200).
    #[test]
    fn preview_in_place_shows_what_the_instance_takes() {
        use super::Shown;
        let Some(mut ui) = open(BUTTON) else { return };
        // The coin's Icon sits in the objective's icon text template.
        let candidates: Vec<_> = ui
            .nodes
            .iter()
            .filter(|n| &*n.name == "Icon")
            .filter_map(|n| n.owners.first().copied())
            .collect();
        let mut found = None;
        for (g, key) in candidates {
            ui.enter_template(g, key).unwrap();
            let at = ui.preview.files.iter().enumerate().find_map(|(fi, f)| {
                let pi = f.props.iter().position(|p| p.name == "-SizeInX")?;
                (Some(f.guid) == ui.asset.guid()).then_some((fi, pi))
            });
            if let Some((fi, pi)) = at {
                found = Some((key, fi, pi));
                break;
            }
            ui.leave_template();
        }
        let (key, fi, pi) = found.expect("QT_Objective_IconText in place");
        let keys = ui.preview_instances(fi);
        assert_eq!(keys, [key], "the instance edited");
        let default = ui.preview.files[fi].props[pi].default.clone();
        let (took, from) = ui.preview_shown(fi, pi, &keys).unwrap();
        assert_eq!(from, Shown::Given);
        assert_ne!(took, default, "not the stored default");
        ui.set_preview(fi, pi, Some(DefaultValue::F32(333f32.to_bits())));
        let (v, from) = ui.preview_shown(fi, pi, &keys).unwrap();
        assert_eq!((v, from), (DefaultValue::F32(333f32.to_bits()), Shown::Set));
        ui.set_preview(fi, pi, None);
        assert_eq!(
            ui.preview_shown(fi, pi, &keys).unwrap(),
            (took, Shown::Given)
        );
    }

    // Needs the game: while a Preview value, the Game data filter or a
    // Layout number is typed, the editor's shortcuts still reach the editor
    // and the field keeps its text; other keys stay with the field.
    #[test]
    fn typing_fields_let_the_editors_shortcuts_through() {
        let Some(mut ui) = open(ICON_ROW) else { return };
        for field in [(0, 0), super::FLAG_FILTER] {
            ui.preview.edit = Some(TextEdit::new(field, "Is", Filter::Text));
            for k in ["ctrl-z", "ctrl-y", "ctrl-s", "ctrl-1", "ctrl-3"] {
                assert!(!ui.key_down(&key(k)), "{k} is the editor's");
            }
            assert!(ui.key_down(&key("a")), "typing stays with the field");
            assert!(ui.preview.edit.is_some(), "still typing");
            ui.preview.edit = None;
        }
        let node = (1..ui.nodes.len()).find(|&n| !ui.nodes[n].foreign).unwrap();
        ui.select_widget(node);
        ui.start_layout_edit(crate::ui::placement::Field::Position(0));
        assert!(!ui.key_down(&key("ctrl-s")));
        assert!(ui.layout_edit.is_some(), "still typing");
        // Undo drops every half-typed field, Preview and binder included
        // (round 11 kept a Preview value open across an undo).
        ui.preview.edit = Some(TextEdit::new((0, 0), "Is", Filter::Text));
        ui.open_binder();
        ui.binder.as_mut().unwrap().edit = Some(TextEdit::new(0, "", Filter::Text));
        ui.undo(false);
        assert!(ui.typing().is_none(), "no field typing after an undo");
    }

    // Needs the game: an enum input shows and takes its entries' names,
    // any case, and a name it does not have is refused with the ones it
    // has (LayoutAlignment, whose ids are crc-sized).
    #[test]
    fn enum_inputs_show_and_take_entry_names() {
        let Some(install) = install() else { return };
        anno_formats::phx::register_sources(&install);
        let layout = anno_formats::phx::enum_named("LayoutAlignment");
        let begin = DefaultValue::U32(1403909053);
        let enum_value = |text| parse(super::ENUM, &begin, text, layout);
        assert_eq!(edit_text(&begin, layout), "Begin");
        assert_eq!(enum_value("middle").unwrap(), DefaultValue::U32(1503000153));
        assert_eq!(
            enum_value("1378142755").unwrap(),
            DefaultValue::U32(1378142755)
        );
        let said = enum_value("sideways").unwrap_err().to_string();
        assert_eq!(said, "LayoutAlignment takes Inherited, Begin, Middle, End");
        // A small id needs its enum: AlignmentX's 1 is Center.
        let x = anno_formats::phx::enum_named("AlignmentX");
        assert_eq!(edit_text(&DefaultValue::U32(1), x), "Center");
        assert_eq!(edit_text(&DefaultValue::U32(1), None), "1");
    }

    #[test]
    fn edits_parse_to_the_property_type() {
        let d = DefaultValue::Vec(vec![0, 0]);
        assert_eq!(
            parse(VEC2, &d, "3, 4", None).unwrap(),
            DefaultValue::Vec(vec![3f32.to_bits(), 4f32.to_bits()])
        );
        assert!(parse(VEC2, &d, "3", None).is_err());
        assert_eq!(
            parse(FLOAT, &DefaultValue::F32(0), "1.5", None).unwrap(),
            DefaultValue::F32(1.5f32.to_bits())
        );
        // As the graph shows a vector, and in the inspector's words; never
        // a number the layout cannot place (round 14: "inf" blanked the
        // canvas).
        assert_eq!(
            parse(VEC2, &d, "[3, 4]", None).unwrap(),
            parse(VEC2, &d, "3 4", None).unwrap()
        );
        let said = |kind, text| parse(kind, &d, text, None).unwrap_err().to_string();
        assert_eq!(said(FLOAT, "abc"), "Enter a number");
        assert_eq!(said(FLOAT, "inf"), "Enter a finite number");
        assert_eq!(said(VEC2, "3"), "Enter 2 numbers");
        assert!(said(VEC2, "3, inf").contains("not a number"));
        assert_eq!(said(INT, "1.5"), "Enter a whole number");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::{probe, probe_flags};
    use crate::ui::test_support::{install, open, path};

    // Needs the game: rebuilding only the instances that read a flag finds
    // the same effects as laying the whole screen out again per flag. Also
    // a timing probe: PROBE_SCREENS picks the screens.
    #[test]
    fn incremental_probe_matches_full_layouts() {
        let Some(install) = install() else { return };
        super::load_scene_content(&install);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let names = std::env::var("PROBE_SCREENS")
            .unwrap_or_else(|_| "questtrackerscene,minimapscene".into());
        for name in names.split(',') {
            let asset =
                anno_formats::phx::Asset::parse(install.read(&path(name)).unwrap()).unwrap();
            let effects = |incremental| {
                let t = std::time::Instant::now();
                let e = probe(
                    &install,
                    &asset,
                    &Default::default(),
                    true,
                    &stop,
                    incremental,
                );
                let e = e.unwrap();
                let live = e.iter().filter(|e| e.any()).count();
                eprintln!(
                    "PROBE {name} incremental {incremental}: {} flags read, {live} change it, {:?}",
                    e.len(),
                    t.elapsed()
                );
                // Unnamed widgets say nothing in the list, and sample list
                // items are told apart by order, which a partial rebuild
                // may change.
                let named = |l: Vec<String>| -> Vec<String> {
                    l.into_iter().filter(|n| n != "widget").collect()
                };
                e.into_iter()
                    .map(|e| {
                        (
                            e.path,
                            e.value,
                            named(e.shows),
                            named(e.hides),
                            named(e.changes),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let (fast, full) = (effects(true), effects(false));
            let differ: Vec<_> = fast.iter().zip(&full).filter(|(a, b)| a != b).collect();
            assert_eq!(fast.len(), full.len());
            assert!(differ.is_empty(), "{name}: {differ:#?}");
        }
    }

    // Needs the game: a click on what a flag does selects a widget it
    // names, on a screen whose flags reach into templates.
    #[test]
    fn clicking_an_effect_selects_what_it_changes() {
        let Some(mut ui) = open("questtrackerscene") else {
            return;
        };
        let stop = std::sync::atomic::AtomicBool::new(false);
        let root = ui.layout_root();
        let effects = probe_flags(&ui.install, root, &ui.preview.values, ui.shown, &stop).unwrap();
        let live: Vec<_> = effects.iter().filter(|e| e.any()).collect();
        assert!(!live.is_empty());
        for e in live {
            ui.canvas.selected = None;
            ui.canvas.marked.clear();
            // First click: outlined, nothing selected (the inspector stays
            // on Game data); a second click on it selects.
            ui.show_effect(e);
            assert!(ui.canvas.selected.is_none());
            assert!(!ui.canvas.marked.is_empty(), "{} outlines nothing", e.path);
            ui.show_effect(e);
            let n = ui
                .canvas
                .selected
                .unwrap_or_else(|| panic!("{} selects nothing", e.path));
            let names: Vec<&String> = e.shows.iter().chain(&e.hides).chain(&e.changes).collect();
            let name = ui.nodes[n].name.to_string();
            assert!(
                names.iter().any(|x| **x == name || *x == "widget"),
                "{}: selected {name}, effect names {names:?}",
                e.path
            );
        }
    }
}
