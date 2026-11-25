// Owns what the game's Phoenix sources (`data/ui/pwl`, shipped beside the
// compiled UI) say and the compiled assets do not: enum entry names (the
// compiled EnumAssets keep only ids, so `1505389363` is Orientation's
// Horizontal only here), and the widget types (Button, Slider, List…),
// components and effects with their descriptions and properties.
// One `Sources` per install, read where its screens are, as palettes are.
use super::Guid;
use crate::vfs::Install;

/// Where a game keeps its Phoenix sources: install folders, each read for
/// its `.json` files.
#[derive(Debug, Clone, Copy)]
pub struct SourceDirs {
    /// EnumAsset sources.
    pub enums: &'static str,
    /// Widget type, component and effect sources.
    pub templates: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumEntry {
    pub id: u32,
    pub name: String,
    description: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumInfo {
    /// The EnumAsset's guid: what an enum-typed property names.
    guid: Guid,
    pub name: String,
    pub entries: Vec<EnumEntry>,
}

/// A widget type (what a template's root behaves as: a Button takes the
/// focus and the player's presses), a component (AlphaTween) or an effect,
/// as its source describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateInfo {
    guid: Guid,
    /// WidgetTypeTemplate, ComponentTemplate or EffectTemplate.
    class: String,
    pub name: String,
    pub description: String,
    /// Where the Phoenix editor files it (Interactions, Containers…).
    folder: String,
    /// The type it derives from (Button from InteractiveWidget).
    parent: Option<Guid>,
    properties: Vec<TypeProperty>,
}

/// One of a widget type's, component's or effect's own properties:
/// OnPress, IsChecked, StartAlpha…
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TypeProperty {
    uid: u32,
    name: String,
    /// The source's type name without its prefix: Event, Boolean, Float…
    kind: String,
    description: String,
}

/// One enum source file: its EnumAsset with entry names and descriptions.
fn parse_enum_source(bytes: &[u8]) -> Option<EnumInfo> {
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
fn parse_template_source(bytes: &[u8]) -> Option<TemplateInfo> {
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

/// One install's Phoenix sources: its enums and its widget types,
/// components and effects. 117 ships them and 1800 does not, so they
/// belong to the install that read them, as palettes do (one table for the
/// process went empty under a 117 screen whenever an 1800 one loaded).
#[derive(Debug, Default)]
pub struct Sources {
    enums: Vec<EnumInfo>,
    templates: Vec<TemplateInfo>,
}

impl Sources {
    /// Reads `install`'s sources from `dirs`; empty for an install without
    /// them.
    pub fn load(install: &Install, dirs: &SourceDirs) -> Sources {
        fn read<T>(install: &Install, dir: &str, parse: fn(&[u8]) -> Option<T>) -> Vec<T> {
            install
                .list(dir)
                .iter()
                .filter(|p| p.ends_with(".json"))
                .filter_map(|p| parse(&install.read(p).ok()?))
                .collect()
        }
        Sources {
            enums: read(install, dirs.enums, parse_enum_source),
            templates: (dirs.templates.iter())
                .flat_map(|dir| read(install, dir, parse_template_source))
                .collect(),
        }
    }

    /// The widget type `guid`.
    pub fn widget_type(&self, guid: Guid) -> Option<&TemplateInfo> {
        self.templates
            .iter()
            .find(|t| t.guid == guid && t.class == "WidgetTypeTemplate")
    }

    /// What property `uid` of the widget type, component or effect `owner`
    /// is for, as its source says ("Dispatched when the player presses…"):
    /// up a widget type's parents for one it inherits.
    pub fn property_description(&self, owner: Guid, uid: u32) -> Option<&str> {
        let mut at = Some(owner);
        // Chains are a few deep; the bound stops a cycle.
        for _ in 0..8 {
            let t = self.templates.iter().find(|t| Some(t.guid) == at)?;
            if let Some(p) = t.properties.iter().find(|p| p.uid == uid) {
                return Some(p.description.as_str()).filter(|d| !d.is_empty());
            }
            at = t.parent;
        }
        None
    }

    /// The name of entry `id` of the enum `enum_asset`.
    pub fn enum_entry_name(&self, enum_asset: Guid, id: u32) -> Option<&str> {
        let e = self.enum_info(enum_asset)?;
        Some(e.entries.iter().find(|x| x.id == id)?.name.as_str())
    }

    /// The name of `id` without knowing its enum: only for the hashed ids
    /// (LayoutAlignment, Orientation, EaseType, DataViewIdentifier entries
    /// are crc-sized) that one enum alone has. Small ids (AlignmentX's 0
    /// to 3) mean nothing without their enum.
    pub fn unique_entry_name(&self, id: u32) -> Option<&str> {
        // Hashed ids are crc-sized; listed ids stay far below this.
        const HASHED: u32 = 1 << 20;
        if id < HASHED {
            return None;
        }
        let mut hits = (self.enums.iter())
            .flat_map(|e| &e.entries)
            .filter(|x| x.id == id);
        let first = hits.next()?;
        hits.next().is_none().then_some(first.name.as_str())
    }

    /// The enum `enum_asset`, for a list of choices or a typed name.
    pub fn enum_info(&self, enum_asset: Guid) -> Option<&EnumInfo> {
        self.enums.iter().find(|e| e.guid == enum_asset)
    }

    /// The enum whose type a native enum value names by crc
    /// (`Value::Enum::ty`, crc32 of the type's name: a label's
    /// HorizontalOverflow).
    pub fn enum_of_type(&self, ty: u32) -> Option<&EnumInfo> {
        (self.enums.iter()).find(|e| super::crc32(e.name.as_bytes()) == ty)
    }

    /// The enum called `name` (`AlignmentX`): a label's builtin enum
    /// properties name no EnumAsset of their own.
    pub fn enum_named(&self, name: &str) -> Option<Guid> {
        self.enums.iter().find(|e| e.name == name).map(|e| e.guid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Anno 117's folders, as games/anno117 hands them in.
    const DIRS: SourceDirs = SourceDirs {
        enums: "data/ui/pwl/enums",
        templates: &[
            "data/ui/pwl/widgettypes",
            "data/ui/pwl/components",
            "data/ui/pwl/effects",
        ],
    };

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

    // Needs the game: every shipped source reads, and the ids the graph
    // showed as numbers read as names.
    #[test]
    fn the_install_names_its_enums() {
        let Some(dir) = crate::vfs::install_dir("ANNO117_DIR") else {
            return;
        };
        let sources = Sources::load(
            &Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap(),
            &DIRS,
        );
        assert_eq!(sources.enums.len(), 14);
        let templates = sources.templates.len();
        assert!(
            templates > 70,
            "16 widget types, 49 components, 7 effects: {templates}"
        );
        assert_eq!(sources.unique_entry_name(1505389363), Some("Horizontal"));
        assert_eq!(sources.unique_entry_name(1403909053), Some("Begin"));
        assert_eq!(
            sources.unique_entry_name(1),
            None,
            "small ids need their enum"
        );
        let x = sources.enum_named("AlignmentX").unwrap();
        assert_eq!(sources.enum_entry_name(x, 1), Some("Center"));
        assert_eq!(sources.enum_info(x).unwrap().entries.len(), 4);
        let overflow = sources
            .enum_of_type(crate::phx::name_crc(b"HorizontalOverflow"))
            .unwrap();
        assert_eq!(overflow.entries[4].name, "Ellipsis");
        // The Button type: its events, described, and what it derives from.
        let button_guid = Guid::parse("4fdbe42a-f548-4eb8-818e-fcbafc9ad83e").unwrap();
        let button = sources.widget_type(button_guid).unwrap();
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
        let parent = sources.widget_type(button.parent.unwrap()).unwrap();
        assert_eq!(parent.name, "InteractiveWidget");
        // Descriptions, a button's own and one it inherits (Focusable is
        // InteractiveWidget's), and a component's.
        let said =
            |owner: &str, uid| sources.property_description(Guid::parse(owner).unwrap(), uid);
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

        // 1800 ships none: why the sources are an install's (one table
        // for the process went empty when an 1800 screen loaded, and a 117
        // screen's buttons stopped grouping as buttons).
        if let Some(dir) = crate::vfs::install_dir("ANNO1800_DIR") {
            let none = Sources::load(
                &Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap(),
                &DIRS,
            );
            assert!(none.enums.is_empty() && none.templates.is_empty());
        }
    }
}
