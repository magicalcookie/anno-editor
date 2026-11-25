// Asset database: GUID -> name, template, graphics cfgs and icon, from the
// game's `assets.xml` (plus English display names from `texts_english.xml`).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::vfs::Install;

// Inheritance chains are a few levels deep; this only stops cycles.
const MAX_BASE_DEPTH: usize = 16;

/// One top-level `<Asset>`. Fields an asset leaves out are filled from its
/// `BaseAssetGUID` chain.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Asset {
    pub guid: u32,
    /// Internal `Standard/Name`, e.g. `RO| resident tier01 worker m`.
    pub name: String,
    pub template: String,
    pub base: Option<u32>,
    /// `Object/Variations/Item/Filename`, indexed by a placed object's
    /// `Variation`. Empty for assets without a model.
    pub cfgs: Vec<String>,
    pub icon: Option<String>,
    /// English display name (`Text/OasisId` resolved through the texts file).
    pub text: Option<String>,
    /// `Fertility/NeedsSlot`: the slot kind (`Mountain`, `River`, `Marsh`) a
    /// deposit needs on the island before its mine can be built.
    pub needs_slot: Option<String>,
    /// `FertilitySet/Fertilities` or `FertilityPool/FertilityList`: the
    /// fertility (or, in sets, pool) GUIDs listed, in order.
    pub fertilities: Vec<u32>,
    pub slot_type: Option<String>,
    /// `Slot/WorkArea`: the work area a slot building brings, which takes
    /// over the slot's own work area of the same `SlotType`.
    work_area: Option<u32>,
    /// `WorkAreaPath/AreaPathConfig`: the model the engine lays along a
    /// work area's path (1800's mine rails and river channels). Empty
    /// elements (clay and cement mines) read as none.
    area_path: Option<String>,
    pub replacements: Vec<u32>,
    pub allowed_regions: Option<String>,
    associated_regions: Option<String>,
    pub map: Option<MapSettings>,
    pub island: Option<IslandSettings>,
    /// `Mesh/OnlyVisibleInEditor`, from the asset, its bases, else its
    /// template's default (`templates.xml`): the game draws the model only
    /// in its own editor. Build blockers, herds, flocks, sound spots and
    /// markers set it. `None` where nothing says.
    pub editor_only: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapSettings {
    pub path: String,
    pub region: String,
    pub size: String,
    pub island_size: String,
    pub map_type: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct IslandSettings {
    pub path: String,
    region: String,
    pub kind: String,
}

/// Where one game keeps its assets and texts, and the element paths it
/// spells each `Field` by; `Module::database` hands it to `Assets::load`.
/// Plain data: 117 and 1800 differ here only in paths and names.
#[derive(Debug)]
pub struct Schema {
    /// The assets file, a virtual path.
    pub assets_path: &'static str,
    /// Further assets files read after it, where they exist (2070's
    /// add-on keeps its own). A GUID defined twice is the later file's.
    pub more_assets: &'static [&'static str],
    /// The texts display names come from, English.
    pub texts_path: &'static str,
    /// The template defaults read beside the assets (`apply_templates`).
    pub templates_path: Option<&'static str>,
    /// The element that keys a `<Text>` in a texts file: 117's `LineId`,
    /// 1800's `GUID`. A texts file of `id=text` lines (2070's) has none.
    pub text_key: &'static str,
    /// Every `texts_<language>.xml` the game ships, by language.
    pub languages: &'static [(&'static str, &'static str)],
    /// Leaf paths below a top-level `<Asset>` and the field each fills. A
    /// path the table doesn't name is skipped.
    pub fields: &'static [(&'static str, Field)],
    /// `Slot/SlotType` of a work area that names none: its template's
    /// default in `templates.xml`.
    pub default_work_area_slot: &'static str,
    /// Templates of single assets whose every leaf `Assets::values` keeps,
    /// for readers beyond the field table (117's UI sample data). Matched
    /// on the asset's own `<Template>`, not an inherited one.
    pub kept: &'static [&'static str],
}

/// What a leaf of an asset fills in `Asset`. List fields push one entry
/// per leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Template,
    Base,
    Guid,
    Name,
    Icon,
    Cfg,
    EditorOnly,
    /// A display name's id in the texts file.
    TextId,
    /// The GUID, which also keys the display name in the texts file
    /// (2070's `guids.txt`).
    GuidText,
    /// A display name written inline.
    Text,
    NeedsSlot,
    SlotType,
    WorkArea,
    AreaPath,
    AssociatedRegions,
    Replacement,
    AllowedRegion,
    Fertility,
    MapPath,
    MapRegion,
    MapSize,
    MapIslandSize,
    MapType,
    IslandPath,
    IslandRegion,
    IslandKind,
}

impl Schema {
    /// The texts file of `language` (`english`, `german`, ...).
    pub fn texts(&self, language: &str) -> Option<&'static str> {
        (self.languages.iter())
            .find(|(l, _)| *l == language)
            .map(|&(_, path)| path)
    }

    fn field(&self, path: &str) -> Option<Field> {
        (self.fields.iter())
            .find(|(p, _)| *p == path)
            .map(|&(_, f)| f)
    }

    fn reads_text_ids(&self) -> bool {
        (self.fields.iter()).any(|&(_, f)| matches!(f, Field::TextId | Field::GuidText))
    }
}

/// GUID-indexed view of `assets.xml`.
#[derive(Debug, Default)]
pub struct Assets {
    assets: Vec<Asset>,
    by_guid: HashMap<u32, usize>,
    // Lowercased "name\ntext\ntemplate" per asset for `search`.
    haystack: Vec<String>,
    // Slot buildings (with a `Slot/WorkArea`) by their `Slot/SlotType`.
    slot_buildings: HashMap<String, Vec<usize>>,
    // `Schema::default_work_area_slot`.
    default_work_area_slot: &'static str,
    // Every leaf of the `Schema::kept` assets, by GUID.
    values: HashMap<u32, Vec<(String, String)>>,
}

impl Assets {
    /// Slot replacements whose building can be used in this game region.
    pub fn slot_replacements(&self, guid: u32, region: &str) -> Vec<&Asset> {
        self.get(guid)
            .into_iter()
            .flat_map(|a| &a.replacements)
            .filter_map(|g| self.get(*g))
            .filter(|a| {
                a.associated_regions
                    .as_deref()
                    .is_none_or(|r| r.is_empty() || r.split(';').any(|r| r == region))
            })
            .collect()
    }

    /// Reads the VFS winners of `schema`'s assets, English texts and
    /// template files.
    pub fn load(install: &Install, schema: &Schema) -> Result<Assets> {
        let xml = install.read(schema.assets_path)?;
        let more: Vec<Vec<u8>> = (schema.more_assets.iter())
            .filter_map(|p| install.read(p).ok())
            .collect();
        // 1800 writes its names inline: its texts would be read for nothing.
        let texts = (schema.reads_text_ids())
            .then(|| install.read(schema.texts_path).ok())
            .flatten();
        let files: Vec<&[u8]> = std::iter::once(&xml[..])
            .chain(more.iter().map(Vec::as_slice))
            .collect();
        let mut assets = Assets::parse_files(&files, texts.as_deref(), schema)?;
        if let Some(path) = schema.templates_path
            && let Ok(templates) = install.read(path)
        {
            assets
                .apply_templates(&templates)
                .context("templates xml")?;
        }
        Ok(assets)
    }

    /// Fills what assets and their bases leave unsaid from their template's
    /// defaults in `templates.xml` bytes: today `Mesh/OnlyVisibleInEditor`,
    /// which AudioSpots, PositionMarker, River and a dozen other editor
    /// templates set.
    fn apply_templates(&mut self, xml: &[u8]) -> Result<()> {
        let mut editor_only: HashMap<String, bool> = HashMap::new();
        // (depth of the open <Template>, byte length of its path) and its name.
        let mut current: Option<(usize, usize)> = None;
        let mut name = String::new();
        walk_xml(xml, |ev| {
            match ev {
                Xml::Open { path, depth } if path.ends_with("/Template") && current.is_none() => {
                    current = Some((depth, path.len()));
                    name.clear();
                }
                Xml::Leaf { path, text } => {
                    let Some((_, plen)) = current else {
                        return Ok(());
                    };
                    match path.get(plen + 1..).unwrap_or("") {
                        "Name" => name = text.to_owned(),
                        "Properties/Mesh/OnlyVisibleInEditor" => {
                            editor_only.insert(name.clone(), text == "1");
                        }
                        _ => {}
                    }
                }
                Xml::Close { depth } if current.is_some_and(|(d, _)| d == depth) => {
                    current = None;
                }
                _ => {}
            }
            Ok(())
        })?;
        for a in &mut self.assets {
            if a.editor_only.is_none() {
                a.editor_only = editor_only.get(&a.template).copied();
            }
        }
        Ok(())
    }

    /// Parses `assets.xml` bytes as `schema` spells them; `texts` is a
    /// `texts_<language>.xml`.
    pub fn parse(xml: &[u8], texts: Option<&[u8]>, schema: &Schema) -> Result<Assets> {
        Self::parse_files(&[xml], texts, schema)
    }

    /// `parse` over several assets files, in order: a GUID defined in two
    /// is the later file's, as `get` finds it.
    fn parse_files(files: &[&[u8]], texts: Option<&[u8]>, schema: &Schema) -> Result<Assets> {
        use rayon::prelude::*;
        let mut parsed = Parsed::default();
        for (k, xml) in files.iter().enumerate() {
            let part = parse_assets(xml, schema).with_context(|| format!("assets xml {k}"))?;
            parsed.append(part);
        }
        let Parsed {
            mut assets,
            oasis,
            kept,
        } = parsed;
        if let Some(texts) = texts {
            let wanted: HashSet<i64> = oasis.iter().map(|&(_, id)| id).collect();
            let lines = parse_texts(texts, schema.text_key, &wanted).context("texts xml")?;
            for (i, id) in oasis {
                assets[i].text = lines.get(&id).cloned();
            }
        }
        let by_guid: HashMap<u32, usize> = assets
            .iter()
            .enumerate()
            .map(|(i, a)| (a.guid, i))
            .collect();
        inherit(&mut assets, &by_guid);
        let haystack = assets
            .par_iter()
            .map(|a| {
                format!(
                    "{}\n{}\n{}",
                    a.name,
                    a.text.as_deref().unwrap_or(""),
                    a.template
                )
                .to_lowercase()
            })
            .collect();
        let mut slot_buildings: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, a) in assets.iter().enumerate() {
            if let (Some(_), Some(kind)) = (a.work_area, &a.slot_type) {
                slot_buildings.entry(kind.clone()).or_default().push(i);
            }
        }
        let values = kept.into_iter().map(|(i, v)| (assets[i].guid, v)).collect();
        Ok(Assets {
            assets,
            by_guid,
            haystack,
            slot_buildings,
            default_work_area_slot: schema.default_work_area_slot,
            values,
        })
    }

    /// Buildings that can stand on `slot`, GUIDs ascending: those whose
    /// `Slot/SlotType` is the slot's own or a replacement's (Random mine
    /// slots become Iron, Coal...). `resource`, a replacement a test map
    /// picks, narrows them to its type. Prefers what `AssociatedRegions`
    /// puts in `region`, else takes every one: those tags are not where a
    /// building stands. Every replacement of 1800's "random slot mining"
    /// and the Gold Mine itself say `Moderate`, yet New World islands carry
    /// that slot and mine gold on it.
    pub fn slot_buildings(&self, slot: u32, region: &str, resource: Option<u32>) -> Vec<&Asset> {
        let in_region = |a: &Asset| {
            a.associated_regions
                .as_deref()
                .is_none_or(|r| r.is_empty() || r.split(';').any(|r| r == region))
        };
        let pick = |strict: bool| -> Vec<&Asset> {
            let types: Vec<&str> = match resource.and_then(|r| self.get(r)) {
                Some(r) => r.slot_type.as_deref().into_iter().collect(),
                None => {
                    let s = self.get(slot);
                    let replacements = (s.into_iter())
                        .flat_map(|s| &s.replacements)
                        .filter_map(|g| self.get(*g))
                        .filter(|r| !strict || in_region(r));
                    (s.and_then(|s| s.slot_type.as_deref()).into_iter())
                        .chain(replacements.filter_map(|r| r.slot_type.as_deref()))
                        .collect()
                }
            };
            let mut out: Vec<&Asset> = (types.iter())
                .flat_map(|t| self.slot_buildings.get(*t).into_iter().flatten())
                .map(|&i| &self.assets[i])
                .filter(|b| !strict || in_region(b))
                .collect();
            out.sort_by_key(|b| b.guid);
            out.dedup_by_key(|b| b.guid);
            out
        };
        let strict = pick(true);
        if strict.is_empty() {
            pick(false)
        } else {
            strict
        }
    }

    /// The model `building` lays along the path of `work_area` (a slot's
    /// work area, e.g. "slot mining workarea mountain") once it stands on
    /// the slot: its `Slot/WorkArea` takes over the slot work area of the
    /// same `SlotType` (mountain or cave), and that work area's
    /// `AreaPathConfig` is the model. None for the other work area, and for
    /// buildings that lay none (clay and cement mines).
    pub fn building_work_area_path(&self, building: u32, work_area: u32) -> Option<&str> {
        let kind = |a: &Asset| {
            a.slot_type
                .clone()
                .unwrap_or_else(|| self.default_work_area_slot.into())
        };
        let w = self.get(self.get(building)?.work_area?)?;
        (kind(w) == kind(self.get(work_area)?))
            .then_some(w.area_path.as_deref())
            .flatten()
    }

    /// What `building_work_area_path` gives on the whole, for a slot whose
    /// building is not known: of the buildings that fit (`slot_buildings`)
    /// and take this work area over, the config most of them lay, then the
    /// lowest building GUID's (cave: coal's rail against cement's none;
    /// river: three channels).
    pub fn work_area_path(
        &self,
        slot: u32,
        work_area: u32,
        region: &str,
        resource: Option<u32>,
    ) -> Option<&str> {
        let kind = |a: &Asset| {
            a.slot_type
                .clone()
                .unwrap_or_else(|| self.default_work_area_slot.into())
        };
        let wanted = kind(self.get(work_area)?);
        let configs: Vec<(u32, Option<&str>)> =
            (self.slot_buildings(slot, region, resource).into_iter())
                .filter_map(|b| {
                    let w = self.get(b.work_area?)?;
                    (kind(w) == wanted).then_some((b.guid, w.area_path.as_deref()))
                })
                .collect();
        let count = |c: Option<&str>| configs.iter().filter(|(_, d)| *d == c).count();
        (configs.iter())
            .max_by(|a, b| count(a.1).cmp(&count(b.1)).then(b.0.cmp(&a.0)))
            .and_then(|&(_, c)| c)
    }

    /// Every leaf of a `Schema::kept` asset in file order: its path below
    /// the asset (`Values/MinimapScene/...`) and text. Empty for others.
    pub fn values(&self, guid: u32) -> &[(String, String)] {
        self.values.get(&guid).map_or(&[], Vec::as_slice)
    }

    pub fn get(&self, guid: u32) -> Option<&Asset> {
        self.by_guid.get(&guid).map(|&i| &self.assets[i])
    }

    pub fn len(&self) -> usize {
        self.assets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    /// Assets in file order.
    pub fn iter(&self) -> impl Iterator<Item = &Asset> {
        self.assets.iter()
    }

    /// Case-insensitive substring match on name, display text and template;
    /// an all-digit query also matches the GUID exactly (listed first).
    pub fn search(&self, query: &str, limit: usize) -> Vec<&Asset> {
        let q = query.trim().to_lowercase();
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let exact = q.parse::<u32>().ok().and_then(|g| self.get(g));
        let mut out: Vec<&Asset> = exact.into_iter().collect();
        for (a, h) in self.assets.iter().zip(&self.haystack) {
            if out.len() >= limit {
                break;
            }
            if h.contains(&q) && exact.is_none_or(|e| e.guid != a.guid) {
                out.push(a);
            }
        }
        out
    }
}

// Fills template, cfgs, icon and text from the base chain where the asset
// leaves them out. Name and GUID are always the asset's own.
fn inherit(assets: &mut [Asset], by_guid: &HashMap<u32, usize>) {
    for i in 0..assets.len() {
        let mut base = assets[i].base;
        let mut depth = 0;
        while let Some(b) = base
            && depth < MAX_BASE_DEPTH
        {
            let Some(&j) = by_guid.get(&b) else { break };
            let (src, dst) = if j < i {
                let (l, r) = assets.split_at_mut(i);
                (&l[j], &mut r[0])
            } else if j > i {
                let (l, r) = assets.split_at_mut(j);
                (&r[0], &mut l[i])
            } else {
                break;
            };
            if dst.template.is_empty() {
                dst.template.clone_from(&src.template);
            }
            if dst.cfgs.is_empty() {
                dst.cfgs.clone_from(&src.cfgs);
            }
            if dst.icon.is_none() {
                dst.icon.clone_from(&src.icon);
            }
            if dst.text.is_none() {
                dst.text.clone_from(&src.text);
            }
            if dst.slot_type.is_none() {
                dst.slot_type.clone_from(&src.slot_type);
            }
            if dst.work_area.is_none() {
                dst.work_area = src.work_area;
            }
            if dst.area_path.is_none() {
                dst.area_path.clone_from(&src.area_path);
            }
            if dst.replacements.is_empty() {
                dst.replacements.clone_from(&src.replacements);
            }
            if dst.allowed_regions.is_none() {
                dst.allowed_regions.clone_from(&src.allowed_regions);
            }
            if dst.associated_regions.is_none() {
                dst.associated_regions.clone_from(&src.associated_regions);
            }
            if dst.editor_only.is_none() {
                dst.editor_only = src.editor_only;
            }
            base = src.base;
            depth += 1;
        }
    }
}

enum Xml<'a> {
    Open { path: &'a str, depth: usize },
    Leaf { path: &'a str, text: &'a str },
    Close { depth: usize },
}

// Walks an XML document, reporting element opens/closes with their depth and
// every leaf's text with its `/`-joined element path from the root.
fn walk_xml(xml: &[u8], visit: impl FnMut(Xml<'_>) -> Result<()>) -> Result<()> {
    walk(xml, false, visit)
}

// `walk_xml`, or with `part` set over a slice cut from a document before
// an element: the walk ends where that element does, ahead of the end
// tags of what encloses it, which quick-xml would reject.
fn walk(xml: &[u8], part: bool, mut visit: impl FnMut(Xml<'_>) -> Result<()>) -> Result<()> {
    let mut path = String::new();
    let mut marks: Vec<usize> = Vec::new();
    let mut text = String::new();
    // No `trim_text`: it would also eat the spaces around `&amp;` pieces.
    // Leaf text is trimmed as a whole instead.
    let mut reader = Reader::from_reader(xml);
    // 1800's engine XML closes elements with </>.
    reader.config_mut().check_end_names = false;
    loop {
        let pos = reader.buffer_position();
        match reader
            .read_event()
            .with_context(|| format!("XML error near byte {pos}"))?
        {
            Event::Start(e) => {
                marks.push(path.len());
                if !path.is_empty() {
                    path.push('/');
                }
                path.push_str(std::str::from_utf8(e.name().as_ref())?);
                text.clear();
                visit(Xml::Open {
                    path: &path,
                    depth: marks.len(),
                })?;
            }
            Event::Text(t) => text.push_str(&t.xml_content()?),
            Event::GeneralRef(r) => match r.resolve_char_ref()? {
                Some(c) => text.push(c),
                None => text.push(match r.as_ref() {
                    b"amp" => '&',
                    b"lt" => '<',
                    b"gt" => '>',
                    b"quot" => '"',
                    b"apos" => '\'',
                    other => bail!("unknown entity &{};", String::from_utf8_lossy(other)),
                }),
            },
            Event::End(_) => {
                let leaf = text.trim();
                if !leaf.is_empty() {
                    visit(Xml::Leaf {
                        path: &path,
                        text: leaf,
                    })?;
                }
                text.clear();
                visit(Xml::Close { depth: marks.len() })?;
                let Some(m) = marks.pop() else {
                    bail!("unbalanced XML end tag near byte {pos}")
                };
                path.truncate(m);
                if part && marks.is_empty() {
                    break;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

// `(index into assets, Text/OasisId)` for display-name lookup.
type OasisRefs = Vec<(usize, i64)>;

// What a parse gives: the assets, their text ids, and the leaves of the
// kept ones by index into `assets`.
#[derive(Debug, Default, PartialEq)]
struct Parsed {
    assets: Vec<Asset>,
    oasis: OasisRefs,
    kept: Vec<(usize, Vec<(String, String)>)>,
}

// Top-level assets live at `AssetList/Groups/Group/Assets/Asset` (groups
// nest). `<Asset>` elsewhere is a GUID reference inside some value list.
//
// Each top-level asset parses on its own, across cores: one thread took
// 0.66 s for 1800's 275 MB and 96k assets, while objects showed GUIDs.
// `asset_starts` cuts the document before each one; the
// `cut_assets_parse_as_the_whole_document_does` test holds the cut to
// the whole-document parse on both games' files.
fn parse_assets(xml: &[u8], schema: &Schema) -> Result<Parsed> {
    use rayon::prelude::*;
    let starts = asset_starts(xml);
    let parts: Vec<Parsed> = (0..starts.len())
        .into_par_iter()
        .map(|i| {
            let end = starts.get(i + 1).copied().unwrap_or(xml.len());
            parse_asset_list(&xml[starts[i]..end], true, schema)
        })
        .collect::<Result<_>>()?;
    let mut out = Parsed {
        assets: Vec::with_capacity(parts.len()),
        ..Parsed::default()
    };
    for part in parts {
        out.append(part);
    }
    Ok(out)
}

impl Parsed {
    /// `part`'s assets after these, its indices moved along.
    fn append(&mut self, part: Parsed) {
        let at = self.assets.len();
        (self.oasis).extend(part.oasis.into_iter().map(|(i, id)| (at + i, id)));
        (self.kept).extend(part.kept.into_iter().map(|(i, v)| (at + i, v)));
        self.assets.extend(part.assets);
    }
}

/// Where each top-level `<Asset>` starts: one no other asset encloses,
/// counting every `<Asset>` and `</Asset>` in order (117 assets hold
/// whole assets inline), and followed by an element, as every asset's
/// `<Template>` or `<Values>` is. A GUID reference holds text instead
/// (`<Asset>1500</Asset>`).
fn asset_starts(xml: &[u8]) -> Vec<usize> {
    const OPEN: &[u8] = b"<Asset>";
    let mut opens = memchr::memmem::find_iter(xml, OPEN).peekable();
    let mut closes = memchr::memmem::find_iter(xml, b"</Asset>").peekable();
    let mut starts = Vec::new();
    let mut depth = 0usize;
    loop {
        let open = match (opens.peek().copied(), closes.peek().copied()) {
            (Some(open), Some(close)) if close < open => None,
            (Some(open), _) => Some(open),
            (None, Some(_)) => None,
            (None, None) => break,
        };
        let Some(open) = open else {
            closes.next();
            depth = depth.saturating_sub(1);
            continue;
        };
        opens.next();
        let rest = &xml[open + OPEN.len()..];
        let mut child = rest.iter().skip_while(|b| b.is_ascii_whitespace());
        let element = matches!(child.next(), Some(b'<')) && !matches!(child.next(), Some(b'/'));
        if depth == 0 && element {
            starts.push(open);
        }
        depth += 1;
    }
    starts
}

/// The assets in `xml`: a whole document, or with `part` a slice that
/// starts at a top-level asset.
fn parse_asset_list(xml: &[u8], part: bool, schema: &Schema) -> Result<Parsed> {
    let mut assets = Vec::new();
    let mut oasis = Vec::new();
    let mut kept = Vec::new();
    // The open asset's leaves, while its template is a kept one.
    let mut keeping: Option<Vec<(String, String)>> = None;
    // (depth of the open top-level <Asset>, byte length of its path)
    let mut current: Option<(usize, usize)> = None;
    let mut asset = Asset::default();
    let mut has_guid = false;
    let mut text_id = None;
    walk(xml, part, |ev| {
        match ev {
            Xml::Open { path, depth } => {
                let top = path.ends_with("/Assets/Asset") || (part && path == "Asset");
                if current.is_none() && top {
                    current = Some((depth, path.len()));
                    asset = Asset::default();
                    has_guid = false;
                    text_id = None;
                    keeping = None;
                }
            }
            Xml::Leaf { path, text } => {
                let Some((_, plen)) = current else {
                    return Ok(());
                };
                let rel = path.get(plen + 1..).unwrap_or("");
                if let Some(leaves) = &mut keeping {
                    leaves.push((rel.to_owned(), text.to_owned()));
                } else if rel == "Template" && schema.kept.contains(&text) {
                    keeping = Some(Vec::new());
                }
                let Some(field) = schema.field(rel) else {
                    return Ok(());
                };
                match field {
                    Field::Template => asset.template = text.to_owned(),
                    Field::Base => asset.base = Some(parse_guid(text)?),
                    Field::Guid => {
                        asset.guid = parse_guid(text)?;
                        has_guid = true;
                    }
                    Field::GuidText => {
                        asset.guid = parse_guid(text)?;
                        has_guid = true;
                        text_id = Some(i64::from(asset.guid));
                    }
                    Field::Name => asset.name = text.to_owned(),
                    Field::Icon => asset.icon = Some(text.to_owned()),
                    Field::Cfg => asset.cfgs.push(text.to_owned()),
                    Field::EditorOnly => asset.editor_only = Some(text == "1"),
                    Field::TextId => text_id = text.parse().ok(),
                    Field::Text => asset.text = Some(text.to_owned()),
                    Field::NeedsSlot => asset.needs_slot = Some(text.to_owned()),
                    Field::SlotType => asset.slot_type = Some(text.to_owned()),
                    Field::WorkArea => asset.work_area = Some(parse_guid(text)?),
                    Field::AreaPath => asset.area_path = Some(text.to_owned()),
                    Field::AssociatedRegions => asset.associated_regions = Some(text.to_owned()),
                    Field::Replacement => asset.replacements.push(parse_guid(text)?),
                    Field::AllowedRegion => asset.allowed_regions = Some(text.to_owned()),
                    Field::Fertility => asset.fertilities.push(parse_guid(text)?),
                    Field::MapPath => {
                        asset.map.get_or_insert_with(MapSettings::default).path =
                            text.replace('\\', "/")
                    }
                    Field::MapRegion => {
                        asset.map.get_or_insert_with(MapSettings::default).region = text.to_owned()
                    }
                    Field::MapSize => {
                        asset.map.get_or_insert_with(MapSettings::default).size = text.to_owned()
                    }
                    Field::MapIslandSize => {
                        asset
                            .map
                            .get_or_insert_with(MapSettings::default)
                            .island_size = text.to_owned()
                    }
                    Field::MapType => {
                        asset.map.get_or_insert_with(MapSettings::default).map_type =
                            text.to_owned()
                    }
                    Field::IslandPath => {
                        asset
                            .island
                            .get_or_insert_with(IslandSettings::default)
                            .path = text.replace('\\', "/")
                    }
                    Field::IslandRegion => {
                        asset
                            .island
                            .get_or_insert_with(IslandSettings::default)
                            .region = text.to_owned()
                    }
                    Field::IslandKind => {
                        asset
                            .island
                            .get_or_insert_with(IslandSettings::default)
                            .kind = text.to_owned()
                    }
                }
            }
            Xml::Close { depth } => {
                if current.is_some_and(|(d, _)| d == depth) {
                    current = None;
                    if has_guid {
                        if let Some(id) = text_id {
                            oasis.push((assets.len(), id));
                        }
                        if let Some(leaves) = keeping.take() {
                            kept.push((assets.len(), leaves));
                        }
                        assets.push(std::mem::take(&mut asset));
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok(Parsed {
        assets,
        oasis,
        kept,
    })
}

// A few hundred generated GUIDs are written as negative i32; save files store
// the same 32 bits unsigned.
fn parse_guid(text: &str) -> Result<u32> {
    let v: i64 = text.parse().with_context(|| format!("GUID {text:?}"))?;
    if let Ok(u) = u32::try_from(v) {
        Ok(u)
    } else if let Ok(i) = i32::try_from(v) {
        Ok(i as u32)
    } else {
        bail!("GUID {text} out of 32-bit range")
    }
}

// `TextExport/Texts/Text/{<key>,Text}`, or `id=text` lines; keeps only
// the ids asked for.
fn parse_texts(xml: &[u8], key: &str, wanted: &HashSet<i64>) -> Result<HashMap<i64, String>> {
    if !xml.trim_ascii_start().starts_with(b"<") {
        return Ok(parse_text_lines(xml, wanted));
    }
    let mut out = HashMap::new();
    let mut line = None;
    walk_xml(xml, |ev| {
        if let Xml::Leaf { path, text } = ev {
            match path.strip_prefix("TextExport/Texts/Text/") {
                Some(k) if k == key => {
                    line = text.parse::<i64>().ok().filter(|id| wanted.contains(id));
                }
                Some("Text") => {
                    if let Some(id) = line.take() {
                        out.insert(id, text.to_owned());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    })?;
    Ok(out)
}

// 2070's texts: UTF-16 LE behind its byte order mark (UTF-8 without one),
// a line per text as `id=text`, `#` starting a comment line. A line that
// isn't a number before its `=` is skipped.
fn parse_text_lines(bytes: &[u8], wanted: &HashSet<i64>) -> HashMap<i64, String> {
    let text = match bytes.strip_prefix(&[0xff, 0xfe]) {
        Some(utf16) => {
            let units = utf16
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c));
            char::decode_utf16(units)
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        }
        None => String::from_utf8_lossy(bytes).into_owned(),
    };
    (text.lines())
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let (id, text) = l.split_once('=')?;
            let id = id
                .trim()
                .parse::<i64>()
                .ok()
                .filter(|id| wanted.contains(id))?;
            Some((id, text.trim_end_matches('\r').to_owned()))
        })
        .collect()
}

#[cfg(test)]
impl Schema {
    /// Every spelling either game uses, for the parser's own tests and
    /// fixtures; a game's real table is its crate's `assets::SCHEMA`.
    pub(crate) const FIXTURE: Schema = Schema {
        assets_path: "",
        more_assets: &[],
        texts_path: "",
        templates_path: None,
        text_key: "LineId",
        languages: &[],
        fields: &[
            ("Template", Field::Template),
            ("BaseAssetGUID", Field::Base),
            ("Values/Standard/GUID", Field::Guid),
            ("Values/Standard/Name", Field::Name),
            ("Values/Standard/IconFilename", Field::Icon),
            ("Values/Object/Variations/Item/Filename", Field::Cfg),
            ("Values/Mesh/OnlyVisibleInEditor", Field::EditorOnly),
            ("Values/Text/OasisId", Field::TextId),
            ("Values/Text/LocaText/English/Text", Field::Text),
            ("Values/Fertility/NeedsSlot", Field::NeedsSlot),
            ("Values/Slot/SlotType", Field::SlotType),
            ("Values/Slot/WorkArea", Field::WorkArea),
            ("Values/WorkAreaPath/AreaPathConfig", Field::AreaPath),
            (
                "Values/Building/AssociatedRegions",
                Field::AssociatedRegions,
            ),
            (
                "Values/RandomMapObject/Replacements/Item/NewAsset",
                Field::Replacement,
            ),
            (
                "Values/RandomMapObject/DLCSlotReplacements/Item/Replacement",
                Field::Replacement,
            ),
            (
                "Values/ResourceSetCondition/AllowedRegion",
                Field::AllowedRegion,
            ),
            (
                "Values/FertilitySet/Fertilities/Item/Fertility",
                Field::Fertility,
            ),
            (
                "Values/FertilityPool/FertilityList/Item/Fertility",
                Field::Fertility,
            ),
            ("Values/MapTemplate/TemplateFilename", Field::MapPath),
            ("Values/MapTemplate/TemplateRegion", Field::MapRegion),
            ("Values/MapTemplate/TemplateSize", Field::MapSize),
            ("Values/MapTemplate/IslandSize", Field::MapIslandSize),
            ("Values/MapTemplate/TemplateMapType", Field::MapType),
            ("Values/RandomIsland/FilePath", Field::IslandPath),
            ("Values/RandomIsland/IslandRegion", Field::IslandRegion),
            ("Values/RandomIsland/IslandType", Field::IslandKind),
        ],
        default_work_area_slot: "WorkAreaMountain",
        kept: &["Kept"],
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2070's spelling: the GUID keys the display name in a UTF-16 texts
    // file of `id=text` lines with comment lines, and an asset in a later
    // file replaces one of the same GUID in an earlier file.
    #[test]
    fn guid_keyed_text_lines_and_later_files_win() {
        const SCHEMA: Schema = Schema {
            text_key: "",
            fields: &[
                ("Template", Field::Template),
                ("Values/Standard/GUID", Field::GuidText),
                ("Values/Standard/Name", Field::Name),
                ("Values/Object/Variations/Item/Filename", Field::Cfg),
            ],
            ..Schema::FIXTURE
        };
        let base = br#"<AssetList><Groups><Group><Assets>
<Asset><Template>Nature</Template><Values><Object><Variations><Item><Filename>data\a.cfg</Filename></Item></Variations></Object><Standard><Name>tree</Name><GUID>313</GUID></Standard></Values></Asset>
<Asset><Template>Building</Template><Values><Standard><Name>old</Name><GUID>9</GUID></Standard></Values></Asset>
</Assets></Group></Groups></AssetList>"#;
        // The add-on's file opens with a bare <Group>.
        let addon = br#"<Group><Groups><Group><Assets>
<Asset><Template>Building</Template><Values><Object><Variations><Item><Filename>data\b.cfg</Filename></Item></Variations></Object><Standard><Name>new</Name><GUID>9</GUID></Standard></Values></Asset>
</Assets></Group></Groups></Group>"#;
        let lines = "# guids, valid guid range: 0 - 999999\r\n\r\n313=Jungle Tree\r\n9=Markthalle = Markt\r\nx=no\r\n";
        let mut texts = vec![0xff, 0xfe];
        texts.extend(lines.encode_utf16().flat_map(u16::to_le_bytes));
        let db = Assets::parse_files(&[&base[..], &addon[..]], Some(&texts), &SCHEMA).unwrap();
        let tree = db.get(313).unwrap();
        assert_eq!(
            (tree.name.as_str(), tree.text.as_deref(), &tree.cfgs[..]),
            ("tree", Some("Jungle Tree"), &["data\\a.cfg".to_owned()][..])
        );
        let building = db.get(9).unwrap();
        assert_eq!(building.name, "new");
        assert_eq!(building.cfgs, ["data\\b.cfg"]);
        // Only the first `=` splits.
        assert_eq!(building.text.as_deref(), Some("Markthalle = Markt"));
        assert_eq!(db.search("jungle", 5).len(), 1);
    }

    // A random mine slot's mountain work area takes the rail every mountain
    // mine brings; its cave one coal's rail or cement's nothing (a tie, the
    // lower building GUID wins); a picked resource narrows to its building.
    #[test]
    fn work_area_paths_follow_slot_buildings() {
        let xml = br#"<AssetList><Groups><Group><Assets>
<Asset><Template>Slot</Template><Values><Standard><GUID>1</GUID></Standard><Slot><SlotType>Random</SlotType></Slot><RandomMapObject><Replacements><Item><NewAsset>2</NewAsset></Item><Item><NewAsset>3</NewAsset></Item><Item><NewAsset>4</NewAsset></Item></Replacements></RandomMapObject></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>2</GUID></Standard><Slot><SlotType>Iron</SlotType></Slot></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>3</GUID></Standard><Slot><SlotType>Coal</SlotType></Slot></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>4</GUID></Standard><Slot><SlotType>Cement</SlotType></Slot></Values></Asset>
<Asset><Template>WorkAreaSlot</Template><Values><Standard><GUID>10</GUID></Standard><Slot><SlotType>WorkAreaMountain</SlotType></Slot></Values></Asset>
<Asset><Template>WorkAreaSlot</Template><Values><Standard><GUID>11</GUID></Standard><Slot><SlotType>WorkAreaCave</SlotType></Slot></Values></Asset>
<Asset><Template>SlotFactoryBuilding7</Template><Values><Standard><GUID>20</GUID></Standard><Slot><SlotType>Iron</SlotType><WorkArea>30</WorkArea></Slot></Values></Asset>
<Asset><Template>SlotFactoryBuilding7</Template><Values><Standard><GUID>21</GUID></Standard><Slot><SlotType>Coal</SlotType><WorkArea>31</WorkArea></Slot></Values></Asset>
<Asset><Template>SlotFactoryBuilding7</Template><Values><Standard><GUID>22</GUID></Standard><Slot><SlotType>Cement</SlotType><WorkArea>32</WorkArea></Slot></Values></Asset>
<Asset><Template>WorkArea</Template><Values><Standard><GUID>30</GUID></Standard><WorkAreaPath><AreaPathConfig>rail.cfg</AreaPathConfig></WorkAreaPath></Values></Asset>
<Asset><Template>WorkArea</Template><Values><Standard><GUID>31</GUID></Standard><Slot><SlotType>WorkAreaCave</SlotType></Slot><WorkAreaPath><AreaPathConfig>rail.cfg</AreaPathConfig></WorkAreaPath></Values></Asset>
<Asset><Template>WorkArea</Template><Values><Standard><GUID>32</GUID></Standard><Slot><SlotType>WorkAreaCave</SlotType></Slot><WorkAreaPath><AreaPathConfig /></WorkAreaPath></Values></Asset>
</Assets></Group></Groups></AssetList>"#;
        let assets = Assets::parse(xml, None, &Schema::FIXTURE).unwrap();
        assert_eq!(
            assets.work_area_path(1, 10, "Moderate", None),
            Some("rail.cfg")
        );
        assert_eq!(
            assets.work_area_path(1, 11, "Moderate", None),
            Some("rail.cfg")
        );
        assert_eq!(assets.work_area_path(1, 11, "Moderate", Some(4)), None);
        assert_eq!(
            assets.work_area_path(1, 10, "Moderate", Some(3)),
            None,
            "coal brings a cave work area"
        );
    }

    #[test]
    fn slot_replacements_respect_inherited_regions_and_dlc_lists() {
        let xml = br#"<AssetList><Groups><Group><Assets>
<Asset><Template>Slot</Template><Values><Standard><GUID>1</GUID></Standard><RandomMapObject><Replacements><Item><NewAsset>2</NewAsset></Item><Item><NewAsset>3</NewAsset></Item></Replacements></RandomMapObject></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>2</GUID></Standard><Building><AssociatedRegions>Moderate</AssociatedRegions></Building></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>3</GUID></Standard><Building><AssociatedRegions>Colony01</AssociatedRegions></Building></Values></Asset>
<Asset><BaseAssetGUID>2</BaseAssetGUID><Values><Standard><GUID>4</GUID></Standard></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>5</GUID></Standard><RandomMapObject><DLCSlotReplacements><Item><Replacement>4</Replacement></Item></DLCSlotReplacements></RandomMapObject></Values></Asset>
</Assets></Group></Groups></AssetList>"#;
        let assets = Assets::parse(xml, None, &Schema::FIXTURE).unwrap();
        assert_eq!(
            assets
                .slot_replacements(1, "Moderate")
                .iter()
                .map(|a| a.guid)
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(
            assets
                .slot_replacements(1, "Colony01")
                .iter()
                .map(|a| a.guid)
                .collect::<Vec<_>>(),
            [3]
        );
        assert_eq!(
            assets
                .slot_replacements(5, "Moderate")
                .iter()
                .map(|a| a.guid)
                .collect::<Vec<_>>(),
            [4]
        );
        assert!(assets.slot_replacements(5, "Arctic").is_empty());
    }

    const XML: &str = r#"<AssetList>
<Groups>
<Group>
<Groups>
<Group>
<Assets>
<Asset>
<Template>Collectable</Template>
<Values>
<Object>
<Variations>
<Item><Filename>data/a_01.cfg</Filename></Item>
<Item><Filename>data/a_02.cfg</Filename></Item>
</Variations>
</Object>
<Standard>
<GUID>1000212</GUID>
<Name>Flotsam &amp; Jetsam</Name>
<IconFilename>data/ui/icon.png</IconFilename>
</Standard>
<Text><OasisId>-6917297453044695070</OasisId></Text>
<AssetPool><AssetList><Item><Asset>42</Asset></Item></AssetList></AssetPool>
</Values>
</Asset>
<Asset>
<BaseAssetGUID>1000212</BaseAssetGUID>
<Values>
<Standard>
<GUID>-11342281</GUID>
<Name>Derived</Name>
</Standard>
<Empty />
</Values>
</Asset>
</Assets>
</Group>
</Groups>
</Group>
</Groups>
</AssetList>"#;

    const TEXTS: &str = r#"<TextExport>
  <Texts>
    <Text>
      <LineId>-6917297453044695070</LineId>
      <Text>Flotsam &lt;sea&gt;</Text>
    </Text>
    <Text>
      <LineId>1</LineId>
      <Text>Unused</Text>
    </Text>
  </Texts>
</TextExport>"#;

    #[test]
    fn editor_only_comes_from_the_asset_its_base_or_its_template() {
        let xml = br#"<AssetList><Groups><Group><Assets>
<Asset><Template>Herd</Template><Values><Standard><GUID>1</GUID></Standard></Values></Asset>
<Asset><Template>Herd</Template><Values><Standard><GUID>2</GUID></Standard><Mesh><OnlyVisibleInEditor>0</OnlyVisibleInEditor></Mesh></Values></Asset>
<Asset><Template>Prop</Template><Values><Standard><GUID>3</GUID></Standard><Mesh><OnlyVisibleInEditor>1</OnlyVisibleInEditor></Mesh></Values></Asset>
<Asset><BaseAssetGUID>3</BaseAssetGUID><Values><Standard><GUID>4</GUID></Standard></Values></Asset>
<Asset><Template>Prop</Template><Values><Standard><GUID>5</GUID></Standard></Values></Asset>
</Assets></Group></Groups></AssetList>"#;
        let templates = br#"<Templates><Group><Templates>
<Template><Name>Herd</Name><Properties><Mesh><OnlyVisibleInEditor>1</OnlyVisibleInEditor></Mesh></Properties></Template>
<Template><Name>Prop</Name><Properties><Mesh><Name>x</Name></Mesh></Properties></Template>
</Templates></Group></Templates>"#;
        let mut assets = Assets::parse(xml, None, &Schema::FIXTURE).unwrap();
        assets.apply_templates(templates).unwrap();
        let editor_only = |g| assets.get(g).unwrap().editor_only;
        assert_eq!(
            [1, 2, 3, 4, 5].map(editor_only),
            [Some(true), Some(false), Some(true), Some(true), None]
        );
    }

    #[test]
    fn parses_assets_with_inheritance_and_texts() {
        let db = Assets::parse(XML.as_bytes(), Some(TEXTS.as_bytes()), &Schema::FIXTURE).unwrap();
        assert_eq!(db.len(), 2);
        let a = db.get(1000212).unwrap();
        assert_eq!(a.name, "Flotsam & Jetsam");
        assert_eq!(a.template, "Collectable");
        assert_eq!(a.cfgs, ["data/a_01.cfg", "data/a_02.cfg"]);
        assert_eq!(a.icon.as_deref(), Some("data/ui/icon.png"));
        assert_eq!(a.text.as_deref(), Some("Flotsam <sea>"));
        assert!(db.get(42).is_none(), "nested <Asset> is a reference");

        let d = db.get(-11342281i32 as u32).unwrap();
        assert_eq!(d.name, "Derived");
        assert_eq!(d.base, Some(1000212));
        assert_eq!(d.template, "Collectable");
        assert_eq!(d.cfgs, a.cfgs);
        assert_eq!(d.text, a.text);
    }

    // A kept asset's leaves come back in file order, nested list items
    // included; other assets keep none.
    #[test]
    fn kept_assets_keep_every_leaf() {
        let xml = br#"<AssetList><Groups><Group><Assets>
<Asset><Template>Kept</Template><Values><Standard><GUID>1</GUID></Standard><Ring><Item><Slot>5</Slot></Item><Item><Slot>6</Slot></Item></Ring></Values></Asset>
<Asset><Template>Other</Template><Values><Standard><GUID>2</GUID></Standard><Ring><Item><Slot>7</Slot></Item></Ring></Values></Asset>
</Assets></Group></Groups></AssetList>"#;
        let db = Assets::parse(xml, None, &Schema::FIXTURE).unwrap();
        let leaf = |p: &str, t: &str| (p.to_owned(), t.to_owned());
        assert_eq!(
            db.values(1),
            [
                leaf("Values/Standard/GUID", "1"),
                leaf("Values/Ring/Item/Slot", "5"),
                leaf("Values/Ring/Item/Slot", "6"),
            ]
        );
        assert!(db.values(2).is_empty());
        assert_eq!(db.get(1).unwrap().template, "Kept");
    }

    #[test]
    fn search_matches_guid_name_and_text() {
        let db = Assets::parse(XML.as_bytes(), Some(TEXTS.as_bytes()), &Schema::FIXTURE).unwrap();
        let guids = |q: &str| db.search(q, 10).iter().map(|a| a.guid).collect::<Vec<_>>();
        assert_eq!(guids("1000212"), [1000212]);
        assert_eq!(guids("JETSAM"), [1000212]);
        assert_eq!(guids("<sea>").len(), 2);
        assert_eq!(guids("collectable").len(), 2);
        assert_eq!(db.search("collectable", 1).len(), 1);
        assert!(guids("  ").is_empty());
    }

    // The parse cut at each top-level asset against one walk over the
    // whole document, on each game's real assets.xml: the same assets in
    // the same order, with the same text references.
    #[test]
    fn cut_assets_parse_as_the_whole_document_does() {
        let games = [
            ("ANNO117_DIR", "data/base/config/export/assets.xml"),
            ("ANNO1800_DIR", "data/config/export/main/asset/assets.xml"),
        ];
        for (var, path) in games {
            let Some(dir) = crate::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let xml = Install::open(dir, crate::vfs::Install::DATA_FIRST)
                .unwrap()
                .read(path)
                .unwrap();
            let t = std::time::Instant::now();
            let cut = parse_assets(&xml, &Schema::FIXTURE).unwrap();
            let cut_time = t.elapsed();
            let t = std::time::Instant::now();
            let whole = parse_asset_list(&xml, false, &Schema::FIXTURE).unwrap();
            eprintln!(
                "{var}: {} assets, cut {cut_time:?}, whole {:?}",
                whole.assets.len(),
                t.elapsed()
            );
            assert_eq!(cut.assets.len(), whole.assets.len(), "{var} asset count");
            assert!(cut.assets == whole.assets, "{var} assets differ");
            assert_eq!(cut.oasis, whole.oasis, "{var} text references");
        }
    }
}
