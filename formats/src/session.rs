// Island-level session settings in `gamedata.data`
// (`GameSessionManager/SessionSettings`): the ambient (lighting and
// atmosphere preset) and the vegetation prop set the game scatters ground
// props from.
//
// Shipped islands store `GlobalAmbientName` (UTF-16), `PlayableArea`
// (4 × i32: x0, y0, x1, y1 in metres) and `VegetationPropSetName` (8-bit,
// no terminator); two extra-large islands add `NorthAmbientName` or
// `SouthAmbientName`, a few celtic ones `EditorSessionGUID`. Fields this
// type does not edit are left as stored.

use anyhow::{Context, Result};

use crate::adf::{Adf, ItemId, utf16_bytes};

const NODE: &str = "GameSessionManager/SessionSettings";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSettings {
    /// `data/*/config/engine/ambientsettings/<name>.xml`.
    pub global_ambient: String,
    /// Extra ambients some extra-large islands blend toward at one edge.
    pub north_ambient: Option<String>,
    pub south_ambient: Option<String>,
    /// A `<PropSet>` name from the selected game's `prop_sets.xml`.
    pub vegetation_prop_set: String,
    /// x0, y0, x1, y1 in metres; always the whole map on shipped islands.
    pub playable_area: [i32; 4],
}

impl SessionSettings {
    /// Reads the settings; an island without the node gets empty names.
    pub fn read(gd: &Adf) -> Result<Self> {
        let Some(node) = gd.node(gd.root(), NODE) else {
            return Ok(Self::default());
        };
        let utf16 = |name: &str| -> Result<Option<String>> {
            gd.child_value(node, name)
                .map(|v| gd.read_utf16(v))
                .transpose()
                .with_context(|| format!("SessionSettings/{name}"))
        };
        let area = gd
            .child_value(node, "PlayableArea")
            .map(|v| gd.payload(v))
            .unwrap_or_default();
        let mut playable_area = [0; 4];
        for (dst, c) in playable_area.iter_mut().zip(area.as_chunks::<4>().0) {
            *dst = i32::from_le_bytes(*c);
        }
        Ok(Self {
            global_ambient: utf16("GlobalAmbientName")?.unwrap_or_default(),
            north_ambient: utf16("NorthAmbientName")?,
            south_ambient: utf16("SouthAmbientName")?,
            vegetation_prop_set: gd
                .child_value(node, "VegetationPropSetName")
                .map(|v| String::from_utf8_lossy(gd.payload(v)).into_owned())
                .unwrap_or_default(),
            playable_area,
        })
    }

    /// Writes the ambient and vegetation set back, adding non-empty values
    /// the tree lacks. Unchanged payloads are not touched.
    pub fn write(&self, gd: &mut Adf) -> Result<()> {
        let Some(node) = gd.node(gd.root(), NODE) else {
            anyhow::ensure!(
                self.global_ambient.is_empty() && self.vegetation_prop_set.is_empty(),
                "gamedata has no SessionSettings"
            );
            return Ok(());
        };
        put(
            gd,
            node,
            "GlobalAmbientName",
            &utf16_bytes(&self.global_ambient),
        )?;
        put(
            gd,
            node,
            "VegetationPropSetName",
            self.vegetation_prop_set.as_bytes(),
        )
    }
}

fn put(gd: &mut Adf, node: ItemId, name: &str, bytes: &[u8]) -> Result<()> {
    match gd.child_value(node, name) {
        Some(v) if gd.payload(v) == bytes => {}
        Some(v) => gd.set_payload(v, bytes),
        None if bytes.is_empty() => {}
        None => {
            gd.add_value(node, Some(name), bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SessionSettings;
    use crate::{island::Island, rda::Archive, vfs::Install};

    // A shipped island's values read as stored; a changed vegetation set
    // and ambient survive an encode and a fresh parse.
    #[test]
    fn settings_read_and_survive_a_save() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = Install::open(dir).unwrap();
        let path = install
            .search("roman_island_medium_03.a7m", 4)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .find(|p| p.ends_with(".a7m"))
            .expect("roman medium_03 ships");
        let a = Archive::from_bytes(install.read(&path).unwrap(), path.as_str()).unwrap();
        let mut island = Island::parse(
            &a.read_path("gamedata.data").unwrap(),
            &a.read_path("rd3d.data").unwrap(),
        )
        .unwrap();
        let shipped = SessionSettings::read(island.gamedata()).unwrap();
        assert_eq!(shipped.global_ambient, "amb_roman_01");
        assert_eq!(shipped.vegetation_prop_set, "ls_heartlands_01_arid");
        assert_eq!(shipped.playable_area, [0, 0, 320, 320]);

        let edited = SessionSettings {
            global_ambient: "amb_roman_02".into(),
            vegetation_prop_set: "ls_heartlands_01_volcanic".into(),
            ..shipped.clone()
        };
        edited.write(island.gamedata_mut()).unwrap();
        let (gd, rd) = island.to_members().unwrap();
        let reread = Island::parse(&gd, &rd).unwrap();
        assert_eq!(SessionSettings::read(reread.gamedata()).unwrap(), edited);
    }
}
