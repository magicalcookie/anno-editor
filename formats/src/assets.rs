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
    pub replacements: Vec<u32>,
    pub allowed_regions: Option<String>,
    pub associated_regions: Option<String>,
    pub map: Option<MapSettings>,
    pub island: Option<IslandSettings>,
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
    pub region: String,
    pub kind: String,
}

/// GUID-indexed view of `assets.xml`.
#[derive(Debug, Default)]
pub struct Assets {
    assets: Vec<Asset>,
    by_guid: HashMap<u32, usize>,
    // Lowercased "name\ntext\ntemplate" per asset for `search`.
    haystack: Vec<String>,
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

    /// Reads the VFS winners of the assets and English texts files.
    pub fn load(install: &Install) -> Result<Assets> {
        let profile = install.game().profile();
        let xml = install.read(profile.assets_path)?;
        let texts = install.read(profile.texts_path).ok();
        Assets::parse(&xml, texts.as_deref())
    }

    /// Parses `assets.xml` bytes; `texts` is a `texts_<language>.xml`.
    pub fn parse(xml: &[u8], texts: Option<&[u8]>) -> Result<Assets> {
        let (mut assets, oasis) = parse_assets(xml).context("assets xml")?;
        if let Some(texts) = texts {
            let wanted: HashSet<i64> = oasis.iter().map(|&(_, id)| id).collect();
            let lines = parse_texts(texts, &wanted).context("texts xml")?;
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
            .iter()
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
        Ok(Assets {
            assets,
            by_guid,
            haystack,
        })
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
            if dst.replacements.is_empty() {
                dst.replacements.clone_from(&src.replacements);
            }
            if dst.allowed_regions.is_none() {
                dst.allowed_regions.clone_from(&src.allowed_regions);
            }
            if dst.associated_regions.is_none() {
                dst.associated_regions.clone_from(&src.associated_regions);
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
fn walk_xml(xml: &[u8], mut visit: impl FnMut(Xml<'_>) -> Result<()>) -> Result<()> {
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
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

// `(index into assets, Text/OasisId)` for display-name lookup.
type OasisRefs = Vec<(usize, i64)>;

// Top-level assets live at `AssetList/Groups/Group/Assets/Asset` (groups
// nest). `<Asset>` elsewhere is a GUID reference inside some value list.
fn parse_assets(xml: &[u8]) -> Result<(Vec<Asset>, OasisRefs)> {
    let mut assets = Vec::new();
    let mut oasis = Vec::new();
    // (depth of the open top-level <Asset>, byte length of its path)
    let mut current: Option<(usize, usize)> = None;
    let mut asset = Asset::default();
    let mut has_guid = false;
    let mut text_id = None;
    walk_xml(xml, |ev| {
        match ev {
            Xml::Open { path, depth } => {
                if current.is_none() && path.ends_with("/Assets/Asset") {
                    current = Some((depth, path.len()));
                    asset = Asset::default();
                    has_guid = false;
                    text_id = None;
                }
            }
            Xml::Leaf { path, text } => {
                let Some((_, plen)) = current else {
                    return Ok(());
                };
                match path.get(plen + 1..).unwrap_or("") {
                    "Template" => asset.template = text.to_owned(),
                    "BaseAssetGUID" => asset.base = Some(parse_guid(text)?),
                    "Values/Standard/GUID" => {
                        asset.guid = parse_guid(text)?;
                        has_guid = true;
                    }
                    "Values/Standard/Name" => asset.name = text.to_owned(),
                    "Values/Standard/IconFilename" => asset.icon = Some(text.to_owned()),
                    "Values/Object/Variations/Item/Filename" => asset.cfgs.push(text.to_owned()),
                    "Values/Text/OasisId" | "Values/Text/LocaText" => text_id = text.parse().ok(),
                    "Values/Fertility/NeedsSlot" => asset.needs_slot = Some(text.to_owned()),
                    "Values/Slot/SlotType" => asset.slot_type = Some(text.to_owned()),
                    "Values/Building/AssociatedRegions" => {
                        asset.associated_regions = Some(text.to_owned())
                    }
                    "Values/RandomMapObject/Replacements/Item/NewAsset"
                    | "Values/RandomMapObject/DLCSlotReplacements/Item/Replacement" => {
                        asset.replacements.push(parse_guid(text)?)
                    }
                    "Values/ResourceSetCondition/AllowedRegion" => {
                        asset.allowed_regions = Some(text.to_owned())
                    }
                    "Values/Text/LocaText/English/Text" => asset.text = Some(text.to_owned()),
                    p if p.starts_with("Values/MapTemplate/") => {
                        let map = asset.map.get_or_insert_with(MapSettings::default);
                        match p.trim_start_matches("Values/MapTemplate/") {
                            "TemplateFilename" => map.path = text.replace('\\', "/"),
                            "TemplateRegion" => map.region = text.to_owned(),
                            "TemplateSize" => map.size = text.to_owned(),
                            "IslandSize" => map.island_size = text.to_owned(),
                            "TemplateMapType" => map.map_type = text.to_owned(),
                            _ => {}
                        }
                    }
                    p if p.starts_with("Values/RandomIsland/") => {
                        let island = asset.island.get_or_insert_with(IslandSettings::default);
                        match p.trim_start_matches("Values/RandomIsland/") {
                            "FilePath" => island.path = text.replace('\\', "/"),
                            "IslandRegion" => island.region = text.to_owned(),
                            "IslandType" => island.kind = text.to_owned(),
                            _ => {}
                        }
                    }
                    "Values/FertilitySet/Fertilities/Item/Fertility"
                    | "Values/FertilityPool/FertilityList/Item/Fertility" => {
                        asset.fertilities.push(parse_guid(text)?)
                    }
                    _ => {}
                }
            }
            Xml::Close { depth } => {
                if current.is_some_and(|(d, _)| d == depth) {
                    current = None;
                    if has_guid {
                        if let Some(id) = text_id {
                            oasis.push((assets.len(), id));
                        }
                        assets.push(std::mem::take(&mut asset));
                    }
                }
            }
        }
        Ok(())
    })?;
    Ok((assets, oasis))
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

// `TextExport/Texts/Text/{LineId,Text}`; keeps only the ids asked for.
fn parse_texts(xml: &[u8], wanted: &HashSet<i64>) -> Result<HashMap<i64, String>> {
    let mut out = HashMap::new();
    let mut line = None;
    walk_xml(xml, |ev| {
        if let Xml::Leaf { path, text } = ev {
            match path {
                "TextExport/Texts/Text/LineId" | "TextExport/Texts/Text/GUID" => {
                    line = text.parse::<i64>().ok().filter(|id| wanted.contains(id));
                }
                "TextExport/Texts/Text/Text" => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_replacements_respect_inherited_regions_and_dlc_lists() {
        let xml = br#"<AssetList><Groups><Group><Assets>
<Asset><Template>Slot</Template><Values><Standard><GUID>1</GUID></Standard><RandomMapObject><Replacements><Item><NewAsset>2</NewAsset></Item><Item><NewAsset>3</NewAsset></Item></Replacements></RandomMapObject></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>2</GUID></Standard><Building><AssociatedRegions>Moderate</AssociatedRegions></Building></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>3</GUID></Standard><Building><AssociatedRegions>Colony01</AssociatedRegions></Building></Values></Asset>
<Asset><BaseAssetGUID>2</BaseAssetGUID><Values><Standard><GUID>4</GUID></Standard></Values></Asset>
<Asset><Template>Slot</Template><Values><Standard><GUID>5</GUID></Standard><RandomMapObject><DLCSlotReplacements><Item><Replacement>4</Replacement></Item></DLCSlotReplacements></RandomMapObject></Values></Asset>
</Assets></Group></Groups></AssetList>"#;
        let assets = Assets::parse(xml, None).unwrap();
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
    fn parses_assets_with_inheritance_and_texts() {
        let db = Assets::parse(XML.as_bytes(), Some(TEXTS.as_bytes())).unwrap();
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

    #[test]
    fn search_matches_guid_name_and_text() {
        let db = Assets::parse(XML.as_bytes(), Some(TEXTS.as_bytes())).unwrap();
        let guids = |q: &str| db.search(q, 10).iter().map(|a| a.guid).collect::<Vec<_>>();
        assert_eq!(guids("1000212"), [1000212]);
        assert_eq!(guids("JETSAM"), [1000212]);
        assert_eq!(guids("<sea>").len(), 2);
        assert_eq!(guids("collectable").len(), 2);
        assert_eq!(db.search("collectable", 1).len(), 1);
        assert!(guids("  ").is_empty());
    }

    #[test]
    fn loads_installed_assets() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let t = std::time::Instant::now();
        let db = Assets::load(&install).unwrap();
        eprintln!("Assets::load: {} assets in {:?}", db.len(), t.elapsed());
        assert!(db.len() > 30_000);

        let tower = db.get(87583).unwrap();
        assert_eq!(tower.name, "Harbor Heartlands Pirate01 Platform Scorpios");
        assert_eq!(tower.template, "MilitaryTowerUnit");
        assert_eq!(tower.text.as_deref(), Some("Local Raider Defences"));
        assert_eq!(
            tower.cfgs,
            [
                "data/base/graphics/library/modular_level_kits/generic/stone_construction_v03/stone_construction_semicircular.cfg"
            ]
        );

        let kontor = db.get(3402).unwrap();
        assert_eq!(kontor.template, "HarborWarehouse");
        assert_eq!(kontor.text.as_deref(), Some("Trading Post"));
        assert!(kontor.cfgs[0].ends_with("kontor_01.cfg"));

        let iron = db.get(4049).unwrap();
        assert_eq!(iron.template, "Fertility");
        assert_eq!(iron.needs_slot.as_deref(), Some("Mountain"));
        let pool = db.get(31349).unwrap();
        assert_eq!(pool.name, "Fertility Pool Roman Iron Marble");
        assert_eq!(pool.fertilities, [4049, 4062]);

        let worker = db.get(28639).unwrap();
        assert_eq!(worker.template, "Inhabitant");
        assert_eq!(worker.cfgs.len(), 2);

        // Some cfgs are dead references (dev placeholders, cut quest
        // props); about 17% of 3.4k in this build.
        let all: Vec<&String> = db.iter().flat_map(|a| &a.cfgs).collect();
        let missing = all.iter().filter(|c| install.file(c).is_none()).count();
        assert!(
            missing * 4 < all.len(),
            "{missing} of {} cfgs missing",
            all.len()
        );
        assert!(install.file(&tower.cfgs[0]).is_some());
    }
}
