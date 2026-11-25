// Shell-side terrain reads of the editable island document.
pub use anno_core::island::{
    A7m, Doc, Layer, MaterialChange, Materials, Paint, Prop, decompose, island_data_dir, trs,
};

impl crate::terrain::MaterialWords for Layer {
    fn size(&self) -> [usize; 2] {
        self.size()
    }

    fn word(&self, x: usize, y: usize) -> u32 {
        self.get(x, y)
    }

    fn row_into(&self, y: usize, xs: std::ops::Range<usize>, out: &mut Vec<u32>) {
        self.row_into(y, xs, out);
    }
}

/// The id that shows most at a world position once the game has laid
/// the slots over each other; without the set, the heaviest.
pub(super) fn shown(
    materials: &Materials,
    p: [f32; 2],
    palette: Option<&crate::terrain::Palette>,
) -> u8 {
    let Some(palette) = palette else {
        return materials.dominant(p);
    };
    let cover = materials.cover(p);
    let mut stacked = cover;
    let n = palette.shares(&mut stacked);
    let share = |id: u8| stacked[..n].iter().find(|s| s.0 == id).map_or(0., |s| s.1);
    // In the texel's own order, so ties go to the later slot as they did.
    (cover.iter().map(|c| (c.0, share(c.0))))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|c| c.1 > 0.)
        .map_or_else(|| materials.dominant(p), |c| c.0)
}

#[cfg(test)]
mod tests {
    use super::{Layer, island_data_dir};
    use anno_formats::a7m::island;
    use gpui_editor_kit::heightfield::{Brush, Sculpt};
    use std::sync::Arc;

    // Needs the game: a level 0 map read band by band into its chunks
    // holds what decoding the whole file and copying it into chunks held,
    // and keeps the same frame around the words for saves.
    #[test]
    fn level0_maps_read_into_chunks_as_decoded() {
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let Some(install) = anno_formats::vfs::install_dir("ANNO117_DIR")
            .map(|d| anno_formats::vfs::Install::open(d, families).unwrap())
        else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let path = super::super::test_support::PIRATE;
        let base = island_data_dir(path).unwrap();
        for name in ["matid", "matalpha"] {
            let file = format!("{base}/materialmaps/0/{name}.dds");
            let (layer, frame) = Layer::read(&install, &file).unwrap();
            let mut dds = install.read(&file).unwrap();
            let grid = island::MaterialMap::parse(&dds).unwrap();
            assert!(layer == Layer::new(&grid), "{name}: chunks differ");
            dds.drain(
                island::MaterialMap::HEADER..island::MaterialMap::HEADER + grid.data.len() * 4,
            );
            assert_eq!(frame, dds, "{name}: frame differs");
        }
    }

    // Needs the game: the island's trees go after loading and are read
    // again to save, from the archives when they serve the island (the
    // app's install given) or from the archive read at load. Both save the
    // members an island holding its trees saves.
    #[test]
    fn parked_sources_save_as_held_ones() {
        use super::super::IslandEditor;
        use super::super::test_support::{PIRATE, assets, install, saved, staged};
        let Some(install) = install() else {
            return;
        };
        let install = Arc::new(install);
        let game = crate::games::of(&install);
        let shared = Some((install.clone(), assets(&install)));
        let mut editors = [
            IslandEditor::load_with(crate::games::module(game), &install, PIRATE, shared).unwrap(),
            IslandEditor::load(crate::games::module(game), &install, PIRATE).unwrap(),
            IslandEditor::load(crate::games::module(game), &install, PIRATE).unwrap(),
        ];
        editors[2].island.a7m_mut().unwrap().unpark().unwrap();
        let brush = Brush {
            radius: 10.,
            strength: 3.,
            ..Brush::default()
        };
        let members = editors.each_mut().map(|editor| {
            Arc::make_mut(&mut editor.island.doc.field).sculpt([120., 120.], &brush, Sculpt::Raise);
            let (_config, project) = staged(&install, editor);
            saved(&project, PIRATE).encode().unwrap()
        });
        assert!(members[0] == members[2], "read again from the archives");
        assert!(members[1] == members[2], "read again from the archive kept");
    }

    // Needs the game: saves hand back the shipped material files exactly
    // while unedited, though level 0 keeps only its frame; an edit
    // attaches the shipped map as the original, the edit over it.
    #[test]
    fn material_levels_save_exactly_and_attach_on_edit() {
        use anno_formats::vfs::Install;
        use gpui_editor_kit::heightfield::Brush;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = Install::open(dir, families).unwrap();
        let path = "data/base/provinces/roman/islands/3rdparty/roman_island_3rdparty_pirate_01/roman_island_3rdparty_pirate_01.a7m";
        let editor = super::super::IslandEditor::load(
            crate::games::module(crate::games::of(&install)),
            &install,
            path,
        )
        .unwrap();
        let base = island_data_dir(path).unwrap();
        let shipped = |level: usize, name: &str| {
            install
                .read(&format!("{base}/materialmaps/{level}/{name}.dds"))
                .unwrap()
        };
        let mut source = editor.island.a7m().unwrap().clone();
        source.unpark().unwrap();
        let levels = editor
            .island
            .doc
            .write_into(&mut source, Some(&install))
            .unwrap();
        assert!(!levels.is_empty());
        for (level, (ids, alphas)) in levels.iter().enumerate() {
            assert!(*ids == shipped(level, "matid"), "matid {level}");
            assert!(*alphas == shipped(level, "matalpha"), "matalpha {level}");
        }
        assert!(source.full_material_ids().is_none());

        let mut doc = editor.island.doc.clone();
        let m = doc.materials.as_mut().unwrap();
        let brush = Brush {
            radius: 6.,
            ..Brush::default()
        };
        assert!(m.paint([60., 60.], &brush, 3));
        let mut source = editor.island.a7m().unwrap().clone();
        source.unpark().unwrap();
        let levels = doc.write_into(&mut source, Some(&install)).unwrap();
        assert!(levels[0].1 != shipped(0, "matalpha"));
        let materials = doc.materials.unwrap();
        let paint = materials.paint.a7m().unwrap();
        assert!(*source.full_material_alphas_mut().unwrap() == paint.alphas.grid());
    }
}
