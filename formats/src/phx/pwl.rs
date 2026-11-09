// Owns what the game's Phoenix sources (`data/ui/pwl`, shipped beside the
// compiled UI) say and the compiled assets do not: enum entry names (the
// compiled EnumAssets keep only ids, so `1505389363` is Orientation's
// Horizontal only here), and the widget types (Button, Slider, List…),
// components and effects with their descriptions and properties.
// Registered once per install and read everywhere, as palettes are.
use super::Guid;
use crate::vfs::Install;
use std::sync::RwLock;

/// Where the game keeps its Phoenix sources.
pub const ENUM_SOURCES: &str = "data/ui/pwl/enums";
pub const TEMPLATE_SOURCES: [&str; 3] = [
    "data/ui/pwl/widgettypes",
    "data/ui/pwl/components",
    "data/ui/pwl/effects",
];

#[derive(Debug, Clone, PartialEq)]
pub struct EnumEntry {
    pub id: u32,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumInfo {
    /// The EnumAsset's guid: what an enum-typed property names.
    pub guid: Guid,
    pub name: String,
    pub entries: Vec<EnumEntry>,
}

/// A widget type (what a template's root behaves as: a Button takes the
/// focus and the player's presses), a component (AlphaTween) or an effect,
/// as its source describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateInfo {
    pub guid: Guid,
    /// WidgetTypeTemplate, ComponentTemplate or EffectTemplate.
    pub class: String,
    pub name: String,
    pub description: String,
    /// Where the Phoenix editor files it (Interactions, Containers…).
    pub folder: String,
    /// The type it derives from (Button from InteractiveWidget).
    pub parent: Option<Guid>,
    pub properties: Vec<TypeProperty>,
}

/// One of a widget type's, component's or effect's own properties:
/// OnPress, IsChecked, StartAlpha…
#[derive(Debug, Clone, PartialEq)]
pub struct TypeProperty {
    pub uid: u32,
    pub name: String,
    /// The source's type name without its prefix: Event, Boolean, Float…
    pub kind: String,
    pub description: String,
}

static ENUMS: RwLock<Vec<EnumInfo>> = RwLock::new(Vec::new());
static TEMPLATES: RwLock<Vec<TemplateInfo>> = RwLock::new(Vec::new());

/// One enum source file: its EnumAsset with entry names and descriptions.
pub fn parse_enum_source(bytes: &[u8]) -> Option<EnumInfo> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let o = &v["PhoenixAssetData"]["Object"];
    if o["Class"] != "EnumAsset" {
        return None;
    }
    let entries = o["EnumEntries"]
        .as_array()?
        .iter()
        .filter_map(|e| {
            let e = &e["Object"];
            Some(EnumEntry {
                id: u32::try_from(e["UniqueId"].as_u64()?).ok()?,
                name: e["EntryName"].as_str()?.to_owned(),
                description: e["Description"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect();
    Some(EnumInfo {
        guid: Guid::parse(o["ObjectID"].as_str()?)?,
        name: o["Name"].as_str()?.to_owned(),
        entries,
    })
}

/// One widget type, component or effect source file.
pub fn parse_template_source(bytes: &[u8]) -> Option<TemplateInfo> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let o = &v["PhoenixAssetData"]["Object"];
    let class = o["Class"].as_str()?;
    if !matches!(
        class,
        "WidgetTypeTemplate" | "ComponentTemplate" | "EffectTemplate"
    ) {
        return None;
    }
    let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_owned();
    let properties = o["CustomProperties"]
        .as_array()
        .map(|ps| {
            ps.iter()
                .filter_map(|p| {
                    let p = &p["Object"];
                    Some(TypeProperty {
                        uid: u32::try_from(p["UniqueId"].as_u64()?).ok()?,
                        name: p["PropertyName"].as_str()?.to_owned(),
                        kind: text(&p["PropertyType"])
                            .trim_start_matches("CustomPropertyType_")
                            .to_owned(),
                        description: text(&p["Description"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(TemplateInfo {
        guid: Guid::parse(o["ObjectID"].as_str()?)?,
        class: class.to_owned(),
        name: o["Name"].as_str()?.to_owned(),
        description: text(&o["Description"]),
        folder: text(&o["FolderPath"]),
        parent: o["ParentWidgetType"].as_str().and_then(Guid::parse),
        properties,
    })
}

/// Reads `install`'s Phoenix sources (enums; widget types, components and
/// effects) into the registry, replacing what an earlier install put
/// there. How many enums and templates it has now (none for an install
/// without sources).
pub fn register_sources(install: &Install) -> (usize, usize) {
    fn read<T>(install: &Install, dir: &str, parse: fn(&[u8]) -> Option<T>) -> Vec<T> {
        install
            .list(dir)
            .iter()
            .filter(|p| p.ends_with(".json"))
            .filter_map(|p| parse(&install.read(p).ok()?))
            .collect()
    }
    let enums = read(install, ENUM_SOURCES, parse_enum_source);
    let templates: Vec<TemplateInfo> = TEMPLATE_SOURCES
        .iter()
        .flat_map(|dir| read(install, dir, parse_template_source))
        .collect();
    let n = (enums.len(), templates.len());
    *ENUMS.write().expect("enum names lock") = enums;
    *TEMPLATES.write().expect("templates lock") = templates;
    n
}

fn with<T>(f: impl FnOnce(&[EnumInfo]) -> Option<T>) -> Option<T> {
    f(&ENUMS.read().ok()?)
}

/// The registered widget type `guid`.
pub fn widget_type_info(guid: Guid) -> Option<TemplateInfo> {
    TEMPLATES
        .read()
        .ok()?
        .iter()
        .find(|t| t.guid == guid && t.class == "WidgetTypeTemplate")
        .cloned()
}

/// What property `uid` of the widget type, component or effect `owner`
/// is for, as its source says ("Dispatched when the player presses…"):
/// up a widget type's parents for one it inherits.
pub fn property_description(owner: Guid, uid: u32) -> Option<String> {
    let all = TEMPLATES.read().ok()?;
    let mut at = Some(owner);
    // Chains are a few deep; the bound stops a cycle.
    for _ in 0..8 {
        let t = all.iter().find(|t| Some(t.guid) == at)?;
        if let Some(p) = t.properties.iter().find(|p| p.uid == uid) {
            return Some(p.description.clone()).filter(|d| !d.is_empty());
        }
        at = t.parent;
    }
    None
}

/// The name of entry `id` of the enum `enum_asset`.
pub fn enum_entry_name(enum_asset: Guid, id: u32) -> Option<String> {
    with(|all| {
        let e = all.iter().find(|e| e.guid == enum_asset)?;
        Some(e.entries.iter().find(|x| x.id == id)?.name.clone())
    })
}

/// The name of `id` without knowing its enum: only for the hashed ids
/// (LayoutAlignment, Orientation, EaseType, DataViewIdentifier entries are
/// crc-sized) that one registered enum alone has. Small ids (AlignmentX's
/// 0 to 3) mean nothing without their enum.
pub fn unique_entry_name(id: u32) -> Option<String> {
    if id < 1 << 20 {
        return None;
    }
    with(|all| {
        let mut hits = all.iter().flat_map(|e| &e.entries).filter(|x| x.id == id);
        let first = hits.next()?;
        hits.next().is_none().then(|| first.name.clone())
    })
}

/// The registered enum `enum_asset`, for a list of choices or a typed name.
pub fn enum_info(enum_asset: Guid) -> Option<EnumInfo> {
    with(|all| all.iter().find(|e| e.guid == enum_asset).cloned())
}

/// The registered enum whose type a native enum value names by crc
/// (`Value::Enum::ty`, crc32 of the type's name: a label's
/// HorizontalOverflow).
pub fn enum_of_type(ty: u32) -> Option<EnumInfo> {
    with(|all| {
        all.iter()
            .find(|e| super::crc32(e.name.as_bytes()) == ty)
            .cloned()
    })
}

/// The registered enum called `name` (`AlignmentX`): a label's builtin
/// enum properties name no EnumAsset of their own.
pub fn enum_named(name: &str) -> Option<Guid> {
    with(|all| all.iter().find(|e| e.name == name).map(|e| e.guid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_names_its_entries() {
        let json = br#"{"PhoenixAssetData":{"Object":{"Class":"EnumAsset",
            "ObjectID":"C70B4932-684B-4D11-B09F-4EC7E6352160","Name":"Orientation",
            "EnumEntries":[
              {"Object":{"Class":"EnumEntry","EntryName":"Horizontal","UniqueId":1505389363,"Description":"Side by side."}},
              {"Object":{"Class":"EnumEntry","EntryName":"Vertical","UniqueId":1410225626}}]}}}"#;
        let e = parse_enum_source(json).unwrap();
        assert_eq!(e.name, "Orientation");
        assert_eq!(
            e.guid,
            Guid::parse("c70b4932-684b-4d11-b09f-4ec7e6352160").unwrap()
        );
        assert_eq!(e.entries[0].name, "Horizontal");
        assert_eq!(e.entries[0].description, "Side by side.");
        assert_eq!(e.entries[1].id, 1410225626);
        assert!(
            parse_enum_source(br#"{"PhoenixAssetData":{"Object":{"Class":"Other"}}}"#).is_none()
        );
    }

    // Needs the game: every shipped source registers, and the ids the
    // graph showed as numbers read as names.
    #[test]
    fn the_install_names_its_enums() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            return;
        };
        let install = Install::open(dir).unwrap();
        let (enums, templates) = register_sources(&install);
        assert_eq!(enums, 14);
        assert!(
            templates > 70,
            "16 widget types, 49 components, 7 effects: {templates}"
        );
        assert_eq!(unique_entry_name(1505389363).as_deref(), Some("Horizontal"));
        assert_eq!(unique_entry_name(1403909053).as_deref(), Some("Begin"));
        assert_eq!(unique_entry_name(1), None, "small ids need their enum");
        let x = enum_named("AlignmentX").unwrap();
        assert_eq!(enum_entry_name(x, 1).as_deref(), Some("Center"));
        assert_eq!(enum_info(x).unwrap().entries.len(), 4);
        let overflow = enum_of_type(crate::phx::name_crc(b"HorizontalOverflow")).unwrap();
        assert_eq!(overflow.entries[4].name, "Ellipsis");
        // The Button type: its events, described, and what it derives from.
        let button =
            widget_type_info(Guid::parse("4fdbe42a-f548-4eb8-818e-fcbafc9ad83e").unwrap()).unwrap();
        assert_eq!(button.name, "Button");
        assert_eq!(button.folder, "Interactions");
        let press = button
            .properties
            .iter()
            .find(|p| p.name == "OnPress")
            .unwrap();
        assert_eq!(press.kind, "Event");
        assert!(
            press
                .description
                .starts_with("Dispatched when the player presses")
        );
        let parent = widget_type_info(button.parent.unwrap()).unwrap();
        assert_eq!(parent.name, "InteractiveWidget");
        // Descriptions, a button's own and one it inherits (Focusable is
        // InteractiveWidget's), and a component's.
        let said = |owner: &str, uid| property_description(Guid::parse(owner).unwrap(), uid);
        assert!(said("4fdbe42a-f548-4eb8-818e-fcbafc9ad83e", press.uid).is_some());
        let focusable = parent
            .properties
            .iter()
            .find(|p| p.name == "Focusable")
            .unwrap();
        assert!(said("4fdbe42a-f548-4eb8-818e-fcbafc9ad83e", focusable.uid).is_some());
        assert!(
            said("09d9ca6f-e3e8-4358-9b10-5ce63b067273", 2043943610)
                .is_some_and(|d| d.contains("starting the interpolation"))
        );
    }
}
