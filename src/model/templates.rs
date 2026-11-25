// Owns the entries the inspector's Add section makes from scratch: a
// shipped entry of the kind copied whole, its picked mesh or texture
// swapped in, standing at the origin on nothing. Copying keeps every field
// the game reads; a bare `<Config><ConfigType>…` holds none of them.

use super::{ModelEditor, hierarchy};
use anno_formats::model::{self as cfg, Transform, mesh_path, texture_path};
use anno_formats::rdm::Rdm;
use anno_formats::xml::{self, Doc, Id};
use anyhow::{Context as _, Result};

/// A kind of entry and the shipped cfgs holding one to copy: 117's, then
/// 1800's, the first the install has. Each is the cfg's first entry of the
/// kind, and stands on its own placement under no condition (counted from
/// every shipped cfg). 2070's cfgs take none: their lights are not read
/// here, and their meshes and decals have another shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    PointLight,
    SpotLight,
    /// White, with the engine's default maps: only the fallback for a
    /// mesh no shipped cfg near it draws.
    Mesh,
    Decal,
    Cloth,
}

impl Template {
    pub fn list(self) -> &'static str {
        match self {
            Template::PointLight | Template::SpotLight => "Lights",
            Template::Mesh => "Models",
            Template::Decal => "Decals",
            Template::Cloth => "Clothes",
        }
    }

    fn sources(self) -> [&'static str; 2] {
        match self {
            // A warm 1 m glow without a flicker; 1800's brazier light.
            Template::PointLight => [
                "data/base/graphics/library/modular_level_kits/diana_island/diana_kontor_main_01.cfg",
                "data/graphics/buildings/3rd_party/3rd_party_04_kontor/3rd_party_04_brazier_02.cfg",
            ],
            // 117 ships 57 spots, all in portraits and dust rays.
            Template::SpotLight => [
                "data/base/graphics/portrait/hdri/portrait_light_dancing_leaves.cfg",
                "data/graphics/buildings/production/food_01/food_01.cfg",
            ],
            Template::Mesh => [
                "data/base/graphics/ui/placeholder/constraint_dummy.cfg",
                "data/graphics/buildings/production/agriculture_09/agriculture_09_field_01.cfg",
            ],
            // 117's is the one its model writer copies too.
            Template::Decal => [
                "data/base/graphics/library/environment/props/decals/beach/beach_decal_02.cfg",
                "data/graphics/buildings/3rd_party/3rd_party_02/3rd_party_02_mansion.cfg",
            ],
            // A cloak on a line, and a quest object's flag: one material and
            // a small mesh each.
            Template::Cloth => [
                "data/base/graphics/celtic/buildings/production/fashion/cloak/fashion_cloak.cfg",
                "data/graphics/buildings/3rd_party/3rd_party_questobject/3rd_party_questobject.cfg",
            ],
        }
    }

    /// A light's `Type`: 0 point, 2 spot.
    fn light_type(self) -> Option<&'static str> {
        match self {
            Template::PointLight => Some("0"),
            Template::SpotLight => Some("2"),
            _ => None,
        }
    }
}

/// How many cfgs beside a picked mesh are read for an entry that draws it:
/// a building's folder holds a few dozen at most.
const NEAR_CFGS: usize = 48;

/// A texture map beside a picked one by the games' naming (`x_diff` and
/// `x_norm`), with the material switch that turns it on.
const SIBLINGS: [(&str, &str, &str); 3] = [
    ("cModelNormalTex", "NORMAL_ENABLED", "_norm"),
    ("cModelMetallicTex", "METALLIC_TEX_ENABLED", "_metal"),
    ("cHeightMap", "HEIGHT_MAP_ENABLED", "_height"),
];

impl ModelEditor {
    /// The shipped entry `t` copies, as its XML.
    pub fn template(&self, t: Template) -> Result<String> {
        anyhow::ensure!(
            self.doc.prefix().is_empty(),
            "2070's cfgs take no new {} entries",
            t.list()
        );
        let source = (t.sources().into_iter())
            .find(|p| self.install.exists(p))
            .with_context(|| format!("this game ships none of the cfgs new {} copy", t.list()))?;
        let text = String::from_utf8_lossy(&self.install.read(source)?).into_owned();
        let entry = (xml::configs(&text, t.list()).into_iter())
            .find(|e| {
                t.light_type()
                    .is_none_or(|k| xml::tag(e, "Type") == Some(k))
            })
            .with_context(|| format!("{source} holds no entry to copy"))?;
        Ok(entry.to_owned())
    }

    /// A point light, or a spot shining down its -Y, at the origin.
    pub fn add_light(&mut self, spot: bool) -> Result<Id> {
        let t = match spot {
            true => Template::SpotLight,
            false => Template::PointLight,
        };
        let xml = self.template(t)?;
        self.place_new(t.list(), &xml, |_, _| {})
    }

    /// A cloth as a shipped building hangs it, mesh and all, at the origin:
    /// its mesh and maps are the inspector's to change.
    pub fn add_cloth(&mut self) -> Result<Id> {
        let xml = self.template(Template::Cloth)?;
        self.place_new("Clothes", &xml, |_, _| {})
    }

    /// A mesh entry drawing `rdm`: a copy of the shipped entry that draws it
    /// in a cfg beside it, materials and all, else the white default
    /// material on every material slot it has. 88% of 117's shipped meshes
    /// and 93% of 1800's have such a cfg in their folder or the one above.
    pub fn add_mesh(&mut self, rdm: &str) -> Result<Id> {
        let rdm = mesh_path(rdm.trim());
        let mesh = (self.install.read(&rdm))
            .and_then(|b| Rdm::parse(&b))
            .with_context(|| format!("{rdm} does not read as a mesh"))?;
        let stem = (rdm.rsplit('/').next().unwrap_or(&rdm))
            .trim_end_matches(".rdm")
            .to_owned();
        if let Some(xml) = self.shipped_entry(&rdm) {
            return self.place_new("Models", &xml, |doc, entry| {
                doc.set_child_text(entry, "Name", &stem);
            });
        }
        let xml = self.template(Template::Mesh)?;
        // One material per slot its groups name, each with the mesh's own
        // vertex layout: the engine binds the material's to the buffer.
        let slots = (mesh.groups.iter().map(|g| g.material + 1).max()).unwrap_or(1);
        let format = mesh.format.to_string();
        let file = rdm.replace('/', "\\");
        self.place_new("Models", &xml, |doc, entry| {
            doc.set_child_text(entry, "Name", &stem);
            doc.set_child_text(entry, "FileName", &file);
            let Some(list) = doc.child(entry, "Materials") else {
                return;
            };
            let first = doc.children(list).find(|&c| doc.name(c) == "Config");
            let Some(first) = first else { return };
            for c in doc.children(list).skip(1).collect::<Vec<_>>() {
                doc.remove(c);
            }
            // Lit by the scene alone, untinted: the template is a yellow
            // placeholder that glows.
            for (name, value) in [
                ("cDiffuseColor.r", "1"),
                ("cDiffuseColor.g", "1"),
                ("cDiffuseColor.b", "1"),
                ("GLOW_ENABLED", "0"),
                ("VertexFormat", format.as_str()),
            ] {
                if doc.child(first, name).is_some() {
                    doc.set_child_text(first, name, value);
                }
            }
            for _ in 1..slots {
                doc.duplicate(first);
            }
        })
    }

    /// A terrain decal of `texture` (a shipped `.dds` or a cfg's source
    /// path), 2 m square: its normal, metal and height maps where the
    /// texture has them beside it, else switched off.
    pub fn add_decal(&mut self, texture: &str) -> Result<Id> {
        let texture = texture.trim().replace('\\', "/");
        let source = match texture.strip_suffix("_0.dds") {
            Some(stem) => format!("{stem}.psd"),
            None => texture.clone(),
        };
        anyhow::ensure!(
            self.install.exists(&texture_path(&source)),
            "{texture} is not in the game or the mod"
        );
        // `x_diff.psd` → `x`, `.psd`; the maps beside it share both.
        let (stem, ext) = source.rsplit_once('.').unwrap_or((&source, "psd"));
        let stem = stem.strip_suffix("_diff").unwrap_or(stem);
        let maps: Vec<(&str, &str, Option<String>)> = (SIBLINGS.iter())
            .map(|&(field, switch, suffix)| {
                let path = format!("{stem}{suffix}.{ext}");
                (
                    field,
                    switch,
                    self.install.exists(&texture_path(&path)).then_some(path),
                )
            })
            .collect();
        let xml = self.template(Template::Decal)?;
        self.place_new("Decals", &xml, |doc, entry| {
            doc.set_child_text(entry, "Extents.x", "1");
            doc.set_child_text(entry, "Extents.z", "1");
            let material = (doc.child(entry, "Materials"))
                .and_then(|l| doc.children(l).find(|&c| doc.name(c) == "Config"));
            let Some(m) = material else { return };
            doc.set_child_text(m, "cModelDiffTex", &source);
            // The template's glow reads its own metal map's channel.
            if doc.child(m, "GLOW_ENABLED").is_some() {
                doc.set_child_text(m, "GLOW_ENABLED", "0");
            }
            for (field, switch, path) in &maps {
                match path {
                    Some(p) => {
                        doc.set_child_text(m, field, p);
                        doc.set_child_text(m, switch, "1");
                    }
                    None if doc.child(m, switch).is_some() => {
                        doc.set_child_text(m, switch, "0");
                    }
                    None => {}
                }
            }
        })
    }

    /// The `<Models>` entry of a shipped cfg beside `rdm` (in its folder or
    /// the one above) that draws it.
    fn shipped_entry(&self, rdm: &str) -> Option<String> {
        let dir = rdm.rsplit_once('/')?.0;
        let up = dir.rsplit_once('/').map_or(dir, |(d, _)| d);
        let install = &self.install;
        let cfgs = (install
            .search(&format!("{up}/"), NEAR_CFGS * 40)
            .into_iter())
        .map(|f| install.file_path(f))
        .filter(|p| p.ends_with(".cfg") && p.starts_with(up))
        .take(NEAR_CFGS);
        for path in cfgs {
            let Ok(bytes) = install.read(&path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            for entry in xml::configs(&text, "Models") {
                let doc = Doc::parse(entry.as_bytes());
                let file = (doc.children(Id::ROOT).next())
                    .and_then(|c| doc.child(c, "FileName"))
                    .map(|f| doc.text(f));
                if file.is_some_and(|f| mesh_path(f) == rdm) {
                    return Some(entry.to_owned());
                }
            }
        }
        None
    }

    /// Inserts `xml` last in the MAIN config's `list` as one undo step,
    /// lets `fill` set its fields, and stands it at the origin on nothing
    /// under no condition: of its `<Transformer>` only its placement stays.
    fn place_new(&mut self, list: &str, xml: &str, fill: impl FnOnce(&mut Doc, Id)) -> Result<Id> {
        let main = self.main().context("the cfg has no MAIN config")?;
        self.checkpoint();
        let list_id = match self.doc.child(main, list) {
            Some(l) => l,
            None => self.doc.insert(main, None, &format!("<{list}></{list}>")),
        };
        let entry = self.doc.insert(list_id, None, xml);
        if let Some(t) = self.doc.child(entry, "Transformer") {
            let doc = &self.doc;
            let orientation = doc.children(t).find(|&c| {
                (doc.child(c, "ConfigType")).is_some_and(|k| doc.text(k) == "ORIENTATION_TRANSFORM")
            });
            let others: Vec<Id> = doc
                .children(t)
                .filter(|&c| Some(c) != orientation)
                .collect();
            for c in others {
                self.doc.remove(c);
            }
            if let Some(o) = orientation
                && self.doc.child(o, "Conditions").is_some()
            {
                self.doc.set_child_text(o, "Conditions", "0");
            }
        }
        fill(&mut self.doc, entry);
        cfg::set_placement(&mut self.doc, entry, &Transform::IDENTITY);
        self.tree.expand(hierarchy::Row::List(list_id));
        self.rebuild();
        self.select(Some(entry));
        Ok(entry)
    }
}
