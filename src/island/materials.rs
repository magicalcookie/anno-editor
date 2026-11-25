// Ground material sets: reading and writing the set xml, listing the sets the
// game ships, and adding, replacing or removing an island's materials. A
// shipped set is shared by every island that names it, so the first change
// copies it into the mod as the island's own set (`port::material_set_path`),
// and the island saves that copy with itself.
use super::IslandEditor;
use super::port;
use crate::terrain::Palette;
use anno_formats::vfs::Install;
use anno_formats::xml::set;
use anno_formats::xml::tag;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

/// Every material set the game ships, by path. 1800 keeps them in
/// `data/config/engine/material_sets/`, 117 in `environment/material_sets/`.
pub fn shipped_sets(install: &Install) -> Vec<String> {
    let mut out: Vec<String> = (install.search("material_sets/", 100_000).into_iter())
        .map(|id| install.file_path(id).to_owned())
        .filter(|p| p.ends_with(".xml") && p.contains("/material_sets/"))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// One `<Material>` of a material set.
#[derive(Clone, Debug, PartialEq)]
pub struct SetEntry {
    pub id: u8,
    /// The material xml.
    pub file: String,
    pub repetition: f32,
    pub sharpness: f32,
    /// `<GroundType>`: what the game's scatter grows on it.
    pub ground_type: String,
}

/// The materials of set `xml`, in file order (the stack, bottom first).
pub fn set_entries(xml: &str) -> Vec<SetEntry> {
    (xml.split("<Material>").skip(1))
        .filter_map(|b| {
            let num = |name: &str, default: f32| {
                tag(b, name).and_then(|v| v.parse().ok()).unwrap_or(default)
            };
            Some(SetEntry {
                id: tag(b, "ID")?.parse().ok().filter(|&id: &u8| id < 32)?,
                file: tag(b, "FileName").filter(|f| !f.is_empty())?.to_owned(),
                repetition: num("Repetition", 4.),
                sharpness: num("AlphaSharpness", 1.),
                ground_type: tag(b, "GroundType").unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// The set's first `<Material>` block, used as the template for new ones, and
/// the offset where materials start.
fn block_template(xml: &str) -> Result<(&str, usize)> {
    let start = xml.find("<Material>").context("the set has no materials")?;
    let blocks: Vec<&str> = xml[start..].split("<Material>").skip(1).collect();
    // The first block ends at the next `<Material>`. If it's the only one, it
    // ends at the `</>` closing the set.
    let first = blocks[0];
    let first = match blocks.len() {
        1 => &first[..first.rfind("</>").context("set")?],
        _ => first,
    };
    Ok((first, start))
}

fn material_block(template: &str, e: &SetEntry) -> String {
    let mut b = format!("<Material>{template}");
    b = set(&b, "ID", &e.id.to_string());
    b = set(&b, "FileName", &e.file);
    b = set(&b, "Repetition", &format!("{:.6}", e.repetition));
    b = set(&b, "AlphaSharpness", &format!("{:.6}", e.sharpness));
    b = set(&b, "GroundType", &e.ground_type);
    set(&b, "ObjectFileName", "")
}

/// Writes a set holding `entries`, in order, bottom of the stack first. The
/// header comes from `template` (shader type plus grit, cliff and slush
/// textures, which the game's terrain shader expects), and the template's first
/// material fills in the fields we never set.
pub fn write_material_set(template: &str, entries: &[SetEntry]) -> Result<String> {
    let (block, start) = block_template(template)?;
    let mut out = template[..start].to_owned();
    for e in entries {
        out.push_str(&material_block(block, e));
    }
    out.push_str("</>\n");
    Ok(out)
}

/// Appends each of `added` to set `xml` at the lowest free id (sets hold ids
/// 0..32), last in the file so it sits on top. `added`'s ids are ignored. New
/// entries copy the set's first material for the fields we never set. Returns
/// the xml and the ids given, in `added` order.
pub fn extend_material_set(xml: &str, added: &[SetEntry]) -> Result<(String, Vec<u8>)> {
    let used: Vec<u8> = set_entries(xml).iter().map(|e| e.id).collect();
    let mut free = (0u8..32).filter(|id| !used.contains(id));
    let (block, _) = block_template(xml)?;
    let mut ids = Vec::new();
    let mut entries = String::new();
    for e in added {
        let id = free
            .next()
            .with_context(|| format!("the set has no free material id for {}", e.file))?;
        entries.push_str(&material_block(block, &SetEntry { id, ..e.clone() }));
        ids.push(id);
    }
    let end = xml.rfind("</>").context("the set is not closed")?;
    Ok((format!("{}{entries}{}", &xml[..end], &xml[end..]), ids))
}

/// Set `xml` with material `id` swapped for `entry`'s file, tiling, sharpness
/// and ground type, in the same place.
pub fn replace_in_set(xml: &str, id: u8, entry: &SetEntry) -> Result<String> {
    let mut out = String::with_capacity(xml.len());
    let mut found = false;
    let mut parts = xml.split("<Material>");
    out.push_str(parts.next().unwrap_or_default());
    for b in parts {
        out.push_str("<Material>");
        if tag(b, "ID").and_then(|v| v.parse::<u8>().ok()) == Some(id) {
            found = true;
            let mut b = set(b, "FileName", &entry.file);
            b = set(&b, "Repetition", &format!("{:.6}", entry.repetition));
            b = set(&b, "AlphaSharpness", &format!("{:.6}", entry.sharpness));
            b = set(&b, "GroundType", &entry.ground_type);
            out.push_str(&b);
        } else {
            out.push_str(b);
        }
    }
    ensure!(found, "the set has no material {id}");
    Ok(out)
}

/// `xml` (a material set) without the material at `id`.
pub fn remove_from_set(xml: &str, id: u8) -> Result<String> {
    let mut out = String::with_capacity(xml.len());
    let mut found = false;
    let mut parts = xml.split("<Material>").peekable();
    out.push_str(parts.next().unwrap_or_default());
    while let Some(b) = parts.next() {
        if tag(b, "ID").and_then(|v| v.parse::<u8>().ok()) != Some(id) {
            out.push_str("<Material>");
            out.push_str(b);
            continue;
        }
        found = true;
        // The last block also holds the `</>` that closes the set. Keep it.
        if parts.peek().is_none() {
            let end = b.rfind("</>").context("set")?;
            let before = b[..end].trim_end();
            let block_end = before.rfind("</>").context("set")?;
            out.push_str(b[block_end + 3..].trim_start());
        }
    }
    ensure!(found, "the set has no material {id}");
    Ok(out)
}

/// A material set read with its textures, for picking materials from.
pub struct SetView {
    pub path: String,
    /// The set's materials that load, in its order.
    pub entries: Vec<SetEntry>,
    pub thumbs: Vec<(u8, Arc<gpui_editor_kit::gpui::RenderImage>)>,
}

impl SetView {
    pub fn load(install: &Install, path: &str) -> Result<SetView> {
        let xml = String::from_utf8_lossy(&install.read(path)?).into_owned();
        let palette = Palette::load(install, None, path)?;
        let thumbs = (palette.materials.iter())
            .map(|(&id, m)| (id, m.preview.render_image()))
            .collect();
        // Skip materials that don't load (some shipped sets name test files the
        // game doesn't ship), and list each file once, since sets repeat a file
        // under several ids.
        let mut seen = std::collections::HashSet::new();
        let entries = (set_entries(&xml).into_iter())
            .filter(|e| palette.materials.contains_key(&e.id))
            .filter(|e| seen.insert(e.file.clone()))
            .collect();
        Ok(SetView {
            path: path.to_owned(),
            entries,
            thumbs,
        })
    }
}

/// The Ground material pane's browser of the game's sets.
#[derive(Default)]
pub struct SetBrowser {
    pub sets: Vec<String>,
    /// The set whose materials are shown, once read.
    pub open: Option<Arc<SetView>>,
    /// A set or a material being read.
    pub busy: Option<String>,
    /// The island material the picked texture replaces (else it is added).
    pub replacing: Option<u8>,
}

/// Adding or replacing one material. The set and palette as they'll be, except
/// for the texture, which `run` loads off the UI thread.
pub struct Job {
    entry: SetEntry,
    id: u8,
    /// The material at `id` gives way to `entry` (else `entry` is added).
    replace: bool,
    set_path: String,
    xml: String,
    palette: Palette,
}

impl Job {
    pub fn run(mut self, install: &Install) -> Result<Added> {
        let e = &self.entry;
        let tiling = (e.repetition, e.sharpness);
        match self.replace {
            true => {
                self.palette
                    .replace_material(install, self.id, &e.file, tiling, &e.ground_type)?
            }
            false => {
                self.palette
                    .add_material(install, self.id, &e.file, tiling, &e.ground_type)?
            }
        }
        Ok(Added {
            id: self.id,
            set_path: self.set_path,
            xml: self.xml,
            palette: self.palette,
        })
    }
}

/// A material read and ready for `IslandEditor::apply_material`.
pub struct Added {
    pub id: u8,
    set_path: String,
    xml: String,
    palette: Palette,
}

impl IslandEditor {
    /// The island's set as it stands now: its own copy if edited, else the file
    /// it names.
    fn material_set_xml(&self, install: &Install) -> Result<String> {
        if let Some(xml) = &self.island.own_set {
            return Ok(xml.clone());
        }
        let path = &self.island.material_set;
        Ok(String::from_utf8_lossy(&install.read(path)?).into_owned())
    }

    /// Whether `file` (a material xml) is in the island's set already.
    pub fn has_material(&self, file: &str) -> bool {
        (self.palette.as_ref()).is_some_and(|p| p.materials.values().any(|m| m.file == file))
    }

    /// Starts adding `entry`, a material from another set of this game, at the
    /// lowest free id on top of the stack, with that set's tiling, sharpness
    /// and ground type. Run the job off the UI thread and pass the result to
    /// `apply_material`.
    pub fn material_job(&self, install: &Install, entry: &SetEntry) -> Result<Job> {
        ensure!(
            !self.has_material(&entry.file),
            "the island's set has this material already"
        );
        let xml = self.material_set_xml(install)?;
        let (xml, ids) = extend_material_set(&xml, std::slice::from_ref(entry))?;
        let id = *ids.first().context("no id given")?;
        Ok(Job {
            entry: entry.clone(),
            id,
            replace: false,
            set_path: self.own_set_path()?,
            xml,
            palette: (self.palette.as_deref()).map_or_else(Palette::empty, Palette::clone),
        })
    }

    /// Where the island's set gets written once it changes. A shipped set stays
    /// as it is for the other islands, so this island names its own copy.
    fn own_set_path(&self) -> Result<String> {
        let current = self.island.material_set.as_str();
        if self.island.own_set.is_some() || port::is_ported(current) {
            return Ok(current.to_owned());
        }
        let name = self.path.rsplit('/').next().unwrap_or(&self.path);
        port::material_set_path(self.module, self.module, name.trim_end_matches(".a7m"))
            .context("this game's projects have no folder for material sets")
    }

    /// Starts swapping material `id` for `entry`. The id stays, so ground
    /// painted with it shows the new texture. Run the job off the UI thread and
    /// pass the result to `apply_material`.
    pub fn replace_job(&self, install: &Install, id: u8, entry: &SetEntry) -> Result<Job> {
        ensure!(
            !self.has_material(&entry.file),
            "the island's set has this material already"
        );
        let xml = replace_in_set(&self.material_set_xml(install)?, id, entry)?;
        Ok(Job {
            entry: entry.clone(),
            id,
            replace: true,
            set_path: self.own_set_path()?,
            xml,
            palette: (self.palette.as_deref()).map_or_else(Palette::empty, Palette::clone),
        })
    }

    /// How much of the island's ground material `id` shows, 0..1.
    pub fn material_share(&self, id: u8) -> f64 {
        let (Some(m), Some(p)) = (&self.island.doc.materials, &self.palette) else {
            return 0.;
        };
        let w = super::convert::material_weights(m, &p.stack);
        let total: f64 = w.values().sum();
        w.get(&id).map_or(0., |v| v / total.max(1e-9))
    }

    /// Takes material `id` out of the island's set and repaints its ground with
    /// `into`. The set, palette and ground change together, so the undo history
    /// starts over: undoing past this would bring back ground that names a
    /// material the set no longer has.
    pub fn remove_material(&mut self, install: &Install, id: u8, into: u8) -> Result<()> {
        let palette = self
            .palette
            .clone()
            .context("the island has no material set")?;
        ensure!(id != into, "pick another material to paint its ground with");
        ensure!(
            palette.materials.contains_key(&into),
            "no material {into} in the set"
        );
        ensure!(
            palette.materials.len() > 1,
            "a set keeps at least one material"
        );
        let xml = remove_from_set(&self.material_set_xml(install)?, id)?;
        let set_path = self.own_set_path()?;
        let mut after = (*palette).clone();
        after.remove_material(id);
        if let Some(m) = &mut self.island.doc.materials {
            // 2070 paint has no material set; its layers come from the
            // ground texture registry.
            ensure!(
                m.paint.a7m().is_some(),
                "this island's ground isn't painted from a material set"
            );
            let before = m.clone();
            let map = (palette.materials.keys())
                .map(|&k| (k, if k == id { into } else { k }))
                .collect();
            let size = self.island.doc.field.grid.size()[0];
            super::convert::transfer_materials(
                (&before, &palette.stack),
                (m, &after.stack),
                &map,
                0.,
                size,
                into,
            );
        }
        self.island.material_set = set_path;
        self.island.own_set = Some(xml);
        self.thumbs.retain(|t| t.0 != id);
        self.palette = Some(Arc::new(after));
        if self.paint == id {
            self.paint = into;
        }
        self.ground_stale = true;
        self.splat_stale = true;
        self.listing_dirty = true;
        self.history = gpui_editor_kit::history::History::new(true);
        Ok(())
    }
    /// Puts a loaded material into the island's set and palette. The next save
    /// writes the island and its set.
    pub fn apply_material(&mut self, added: Added) {
        self.island.material_set = added.set_path;
        self.island.own_set = Some(added.xml);
        self.thumbs = (added.palette.materials.iter())
            .map(|(&id, m)| (id, m.preview.render_image()))
            .collect();
        self.palette = Some(Arc::new(added.palette));
        self.paint = added.id;
        self.ground_stale = true;
        self.splat_stale = true;
        self.listing_dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::island::test_support::{install_1800, reopen, staged};

    /// A shipped Old World island on the shipped `terra_01` set.
    fn old_world_island(install: &Install) -> String {
        (install.search("moderate_l_01.a7m", 3).into_iter())
            .map(|id| install.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .unwrap()
    }

    // Existing materials keep their ids; added ones take the lowest free ids,
    // copy the first entry's other fields, and go last.
    #[test]
    fn brought_materials_take_free_ids_on_top() {
        let set = "<MaterialSet><ShaderType>0</>\n\
<Material><ID>0</>\n<FileName>a.xml</>\n<Repetition>4</>\n<AlphaSharpness>5</>\n<GroundType>Grass</>\n<GritEnabled>0</>\n</>\n\
<Material><ID>1</>\n<FileName>b.xml</>\n<Repetition>4</>\n<AlphaSharpness>5</>\n<GroundType>None</>\n<GritEnabled>0</>\n</>\n\
<Material><ID>3</>\n<FileName>c.xml</>\n<Repetition>4</>\n<AlphaSharpness>5</>\n<GroundType>None</>\n<GritEnabled>0</>\n</>\n</>\n";
        let entry = |file: &str, repetition, sharpness| SetEntry {
            id: 0,
            file: file.to_owned(),
            repetition,
            sharpness,
            ground_type: "None".to_owned(),
        };
        let added = [entry("x.xml", 2., 3.), entry("y.xml", 8., 1.)];
        let (xml, ids) = extend_material_set(set, &added).unwrap();
        assert_eq!(ids, [2, 4]);
        let blocks: Vec<&str> = xml.split("<Material>").skip(1).collect();
        assert_eq!(blocks.len(), 5);
        assert_eq!(tag(blocks[3], "FileName"), Some("x.xml"));
        assert_eq!(tag(blocks[3], "ID"), Some("2"));
        assert_eq!(tag(blocks[3], "Repetition"), Some("2.000000"));
        assert_eq!(tag(blocks[3], "GroundType"), Some("None"));
        assert_eq!(tag(blocks[3], "GritEnabled"), Some("0"));
        assert_eq!(tag(blocks[4], "ID"), Some("4"));
        assert!(xml.trim_end().ends_with("</>"));
        // A full set refuses.
        let full: String = (0..32)
            .map(|i| format!("<Material><ID>{i}</>\n<FileName>m.xml</>\n</>\n"))
            .collect();
        assert!(extend_material_set(&format!("<MaterialSet>{full}</>"), &added).is_err());
    }

    // Removing works wherever the entry sits and leaves the set closed.
    // Replacing keeps the id and the position.
    #[test]
    fn set_entries_are_removed_and_replaced_in_place() {
        let set = "<MaterialSet><ShaderType>0</>\n\
<Material><ID>0</>\n<FileName>a.xml</>\n<Repetition>4</>\n<AlphaSharpness>5</>\n<GroundType>Grass</>\n</>\n\
<Material><ID>1</>\n<FileName>b.xml</>\n<Repetition>4</>\n<AlphaSharpness>5</>\n<GroundType>None</>\n</>\n\
<Material><ID>3</>\n<FileName>c.xml</>\n<Repetition>4</>\n<AlphaSharpness>5</>\n<GroundType>None</>\n</>\n</>\n";
        let ids = |xml: &str| set_entries(xml).iter().map(|e| e.id).collect::<Vec<_>>();
        for (gone, left) in [(0, vec![1, 3]), (1, vec![0, 3]), (3, vec![0, 1])] {
            let xml = remove_from_set(set, gone).unwrap();
            assert_eq!(ids(&xml), left, "{xml}");
            assert!(xml.starts_with("<MaterialSet><ShaderType>0</>"));
            assert!(xml.trim_end().ends_with("</>\n</>"), "{xml}");
            // The set still extends, so the closing tag is where the writer
            // expects it.
            let more = SetEntry {
                id: 0,
                file: "z.xml".into(),
                repetition: 1.,
                sharpness: 1.,
                ground_type: "None".into(),
            };
            let (xml, _) = extend_material_set(&xml, &[more]).unwrap();
            assert_eq!(set_entries(&xml).len(), 3);
        }
        assert!(remove_from_set(set, 7).is_err());
        let new = SetEntry {
            id: 9,
            file: "n.xml".into(),
            repetition: 2.,
            sharpness: 3.,
            ground_type: "Grass".into(),
        };
        let xml = replace_in_set(set, 1, &new).unwrap();
        let e = set_entries(&xml);
        assert_eq!(e.iter().map(|e| e.id).collect::<Vec<_>>(), [0, 1, 3]);
        assert_eq!(
            (
                e[1].file.as_str(),
                e[1].repetition,
                e[1].ground_type.as_str()
            ),
            ("n.xml", 2., "Grass")
        );
        assert_eq!(e[0].file, "a.xml");
        assert!(replace_in_set(set, 7, &new).is_err());
    }

    // Needs 1800. An Old World island takes a material from another shipped
    // set. It gets its own set in the mod while the shipped one stays put, the
    // material goes on top at a free id, the palette still has it after
    // reopening from the mod, and adding it twice fails. The set name lives
    // on the editor: the codec source saves and reopens byte-identical.
    #[test]
    fn a_changed_material_set_survives_save_and_reopen_and_leaves_the_source_as_loaded() {
        let Some(install) = install_1800() else {
            return;
        };
        let path = old_world_island(&install);
        let mut island = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            &path,
        )
        .unwrap();
        let source_members = |editor: &IslandEditor| {
            let mut source = editor.island.a7m().unwrap().clone();
            source.unpark().unwrap();
            source.encode().unwrap()
        };
        let loaded = source_members(&island);
        let shipped = island.island.material_set.clone();
        // What the codec source names, which saves leave as loaded (1800's
        // Old World islands name none).
        let named = (island.island.a7m().unwrap().material_set_file_name()).map(str::to_owned);
        let sets = shipped_sets(&install);
        assert!(
            sets.contains(&shipped),
            "{shipped} not in {} sets",
            sets.len()
        );
        // The first other set with a material the island lacks.
        let (other, entry) = (sets.iter())
            .filter(|s| **s != shipped)
            .find_map(|s| {
                let view = SetView::load(&install, s).ok()?;
                let e = view
                    .entries
                    .iter()
                    .find(|e| !island.has_material(&e.file))?
                    .clone();
                Some((s.clone(), e))
            })
            .unwrap();
        eprintln!("adding {} from {other}", entry.file);
        let before = island.palette.as_ref().unwrap().ground_layers.len();
        let added = island
            .material_job(&install, &entry)
            .unwrap()
            .run(&install)
            .unwrap();
        let id = added.id;
        island.apply_material(added);
        let own = island.island.material_set.clone();
        assert!(
            own.starts_with("data/editor/anno1800/material_sets/"),
            "{own}"
        );
        let p = island.palette.as_ref().unwrap();
        assert_eq!(p.ground_layers.len(), before + 1);
        assert_eq!(p.materials[&id].file, entry.file);
        assert_eq!(p.stack.last(), Some(&id));
        assert!(island.dirty());
        assert!(island.material_job(&install, &entry).is_err());
        let (_config, mut project) = staged(&install, &mut island);
        assert!(project.files.contains_key(&own));
        let again = reopen(&install, &mut project, &path);
        assert_eq!(
            again.island.a7m().unwrap().material_set_file_name(),
            Some(&*own)
        );
        assert_eq!(again.island.material_set, own);
        assert_eq!(
            again.palette.as_ref().unwrap().materials[&id].file,
            entry.file
        );
        assert_eq!(
            island.island.a7m().unwrap().material_set_file_name(),
            named.as_deref()
        );
        assert!(
            source_members(&island) == loaded,
            "the codec source changed"
        );
        // The shipped set is unchanged for every other island.
        let shipped_xml = String::from_utf8(install.read(&shipped).unwrap()).unwrap();
        assert!(!shipped_xml.contains(&entry.file));
    }

    // Needs 1800. On a shipped Old World island, replacing a used material
    // leaves the ground layer alone and shows the new texture. Removing one
    // repaints its ground with the chosen material. After saving and reopening
    // from the mod, neither the set nor the ground has it.
    #[test]
    fn materials_are_replaced_and_removed() {
        use crate::island::convert::material_weights;
        let Some(install) = install_1800() else {
            return;
        };
        let path = old_world_island(&install);
        let mut island = IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            &path,
        )
        .unwrap();
        let weights = |i: &IslandEditor| {
            material_weights(
                i.island.doc.materials.as_ref().unwrap(),
                &i.palette.as_ref().unwrap().stack,
            )
        };
        let before = weights(&island);
        let mut used: Vec<(u8, f64)> = before.iter().map(|(&k, &v)| (k, v)).collect();
        used.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (big, other) = (used[0].0, used[1].0);
        // Replace the second most used with a texture from another set.
        let entry = (shipped_sets(&install).iter())
            .find_map(|s| {
                let v = SetView::load(&install, s).ok()?;
                v.entries
                    .into_iter()
                    .find(|e| !island.has_material(&e.file))
            })
            .unwrap();
        let paint_of = |i: &IslandEditor| {
            let a = i
                .island
                .doc
                .materials
                .as_ref()
                .unwrap()
                .paint
                .a7m()
                .unwrap();
            a.ids.clone()
        };
        let ids_before = paint_of(&island);
        let added = island
            .replace_job(&install, other, &entry)
            .unwrap()
            .run(&install)
            .unwrap();
        island.apply_material(added);
        assert_eq!(
            island.palette.as_ref().unwrap().materials[&other].file,
            entry.file
        );
        assert!(paint_of(&island) == ids_before, "replace repainted");
        // Remove the most used; its ground goes to `other`.
        let share = island.material_share(big);
        assert!(share > 0.05, "{share}");
        let into_before = island.material_share(other);
        island.remove_material(&install, big, other).unwrap();
        assert!(
            !island
                .palette
                .as_ref()
                .unwrap()
                .materials
                .contains_key(&big)
        );
        assert!(!weights(&island).contains_key(&big));
        let into_after = island.material_share(other);
        assert!(
            (into_after - (into_before + share)).abs() < 0.02,
            "{into_before} + {share} -> {into_after}"
        );
        let own = island.island.material_set.clone();
        assert!(
            own.starts_with("data/editor/anno1800/material_sets/"),
            "{own}"
        );
        let (_config, mut project) = staged(&install, &mut island);
        let again = reopen(&install, &mut project, &path);
        let p = again.palette.as_ref().unwrap();
        assert!(!p.materials.contains_key(&big));
        assert_eq!(p.materials[&other].file, entry.file);
        assert!(!weights(&again).contains_key(&big));
    }
}
