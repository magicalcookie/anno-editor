// Owns the models the viewports draw as GPU placements: the piece table
// built from the loaded models, and the placement rows, one per placed piece,
// with the item each belongs to so picks and drags map back. Rows are
// rebuilt from the document on structural edits; a drag rewrites only its
// items' rows. The renderer culls and picks detail levels on the GPU.
use super::{Item, models};
use gpui_editor_kit::{
    gpu::{Piece, Renderer},
    math::{self, Mat4, Vec3},
};
use std::collections::{BTreeSet, HashMap};

/// A loaded model's pieces in the renderer's piece table: `count` from
/// `first`. A model whose parts all were skipped (bone-linked, no mesh) has
/// none and shares `first` with the next model.
struct Model {
    first: u32,
    count: u32,
}

#[derive(Default)]
pub struct Placements {
    /// Loaded models by `.cfg` path; filled by `set_pieces`.
    models: HashMap<String, Model>,
    /// Each piece's matrix in its model, by piece index.
    local: Vec<Mat4>,
    /// One per row, struct of arrays: the item (none for the vegetation
    /// preview, which is not pickable), piece index and transform.
    pub item: Vec<Option<Item>>,
    piece_of: Vec<u32>,
    transform: Vec<Mat4>,
    /// First row of the vegetation preview: `set` places it last, so a
    /// stroke can swap those rows alone (`set_grown`).
    grown: usize,
    generation: u64,
}

impl Placements {
    /// Hands the loaded models' pieces to the renderer: each piece's full
    /// mesh and its coarser levels with the screen sizes they take over at.
    /// Clears the rows: they name pieces by index.
    pub fn set_pieces(&mut self, loaded: &HashMap<String, models::Loaded>, renderer: &Renderer) {
        self.models.clear();
        self.local.clear();
        let mut pieces = Vec::new();
        // Sorted, so a model keeps its piece indices across reloads.
        let mut cfgs: Vec<&String> = loaded.keys().collect();
        cfgs.sort();
        for cfg in cfgs {
            let model = &loaded[cfg];
            self.models.insert(
                cfg.clone(),
                Model {
                    first: pieces.len() as u32,
                    count: model.len() as u32,
                },
            );
            for piece in model.iter() {
                self.local.push(piece.local);
                // Level k draws from piece.ppm[k] pixels per metre up; the
                // coarsest below every threshold. The renderer measures
                // metres of the mesh's own space for all but foliage, so
                // the part's scale (maritime pines are modelled 5x too
                // small) carries over into the thresholds.
                let l = &piece.local;
                let part = if piece.foliage {
                    1.
                } else {
                    [0, 4, 8]
                        .map(|i| math::length([l[i], l[i + 1], l[i + 2]]))
                        .into_iter()
                        .fold(0f32, f32::max)
                };
                let mut levels = vec![(piece.mesh.clone(), 0.)];
                for (k, mesh) in piece.lods.iter().enumerate() {
                    levels[k].1 = piece.ppm[k] * part;
                    levels.push((mesh.clone(), 0.));
                }
                pieces.push(Piece {
                    levels,
                    impostor: piece.foliage.then_some(models::IMPOSTOR_PPM),
                });
            }
        }
        renderer.set_pieces(pieces);
        self.generation += 1;
        self.grown = 0;
        self.item.clear();
        self.piece_of.clear();
        self.transform.clear();
    }

    /// Whether `cfg` is loaded and has pieces to place.
    #[cfg(test)]
    pub fn has(&self, cfg: &str) -> bool {
        self.models.contains_key(cfg)
    }

    #[cfg(test)]
    pub fn models(&self) -> usize {
        self.models.len()
    }

    /// Replaces every row from `placed` (item, model, world matrix) and
    /// sends them to the renderer. Returns the items whose model is not
    /// loaded, for markers.
    pub fn set<'a>(
        &mut self,
        placed: impl IntoIterator<Item = (Option<Item>, &'a str, Mat4)>,
        renderer: &Renderer,
    ) -> Vec<Item> {
        self.item.clear();
        self.piece_of.clear();
        self.transform.clear();
        let mut missing = Vec::new();
        for (item, cfg, world) in placed {
            let Some(model) = self.models.get(cfg) else {
                missing.extend(item);
                continue;
            };
            for piece in model.first..model.first + model.count {
                self.item.push(item);
                self.piece_of.push(piece);
                self.transform
                    .push(math::multiply(&world, &self.local[piece as usize]));
            }
        }
        self.grown = self
            .item
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |k| k + 1);
        self.generation += 1;
        renderer.set_placements(&self.piece_of, &self.transform);
        missing
    }

    /// Replaces the vegetation rows with `grown` (model, world matrix),
    /// keeping every item's rows where they are. Models not loaded yet are
    /// skipped, as `set` skips them.
    pub fn set_grown<'a>(
        &mut self,
        grown: impl IntoIterator<Item = (&'a str, Mat4)>,
        renderer: &Renderer,
    ) {
        self.item.truncate(self.grown);
        self.piece_of.truncate(self.grown);
        self.transform.truncate(self.grown);
        for (cfg, world) in grown {
            let Some(model) = self.models.get(cfg) else {
                continue;
            };
            for piece in model.first..model.first + model.count {
                self.item.push(None);
                self.piece_of.push(piece);
                self.transform
                    .push(math::multiply(&world, &self.local[piece as usize]));
            }
        }
        renderer.set_placements(&self.piece_of, &self.transform);
    }

    /// Rewrites the rows of the moved items for their new world matrices,
    /// in place on the renderer too. One pass over the rows: finding each
    /// item's rows with a scan of its own made a drag of 15k props cost
    /// 77 ms a move. Sorts `moves` by item.
    pub fn move_items(&mut self, moves: &mut [(Item, Mat4)], renderer: &Renderer) {
        // Rows apart by at most this many share one upload: a write per
        // item cost more than the 64 bytes a row between them adds.
        const GAP: usize = 256;
        moves.sort_unstable_by_key(|(item, _)| *item);
        let mut upload: Option<std::ops::Range<usize>> = None;
        let flush = |rows: std::ops::Range<usize>, transform: &[Mat4]| {
            renderer.move_placements(rows.start as u32, &transform[rows]);
        };
        let mut row = 0;
        // The vegetation tail belongs to no item.
        while row < self.grown {
            let item = self.item[row];
            let end = row
                + self.item[row..self.grown]
                    .iter()
                    .take_while(|&&i| i == item)
                    .count();
            let moved = item.and_then(|i| moves.binary_search_by_key(&i, |(i, _)| *i).ok());
            if let Some(k) = moved {
                let world = moves[k].1;
                for r in row..end {
                    let local = &self.local[self.piece_of[r] as usize];
                    self.transform[r] = math::multiply(&world, local);
                }
                upload = match upload.take() {
                    Some(u) if row - u.end <= GAP => Some(u.start..end),
                    Some(u) => {
                        flush(u, &self.transform);
                        Some(row..end)
                    }
                    None => Some(row..end),
                };
            }
            row = end;
        }
        if let Some(u) = upload {
            flush(u, &self.transform);
        }
    }

    /// The item a picked row belongs to.
    pub fn item_at(&self, row: u32) -> Option<Item> {
        self.item.get(row as usize).copied().flatten()
    }

    /// What the GPU outlines, marks and drags for `items` (one pass over
    /// the rows): every row of them, then per item its first row and the
    /// item's origin in that row's space, where its marker goes. Items with
    /// no rows (no model, or not loaded yet) are left to the caller.
    pub fn selection(&self, items: &BTreeSet<Item>) -> (Vec<u32>, Vec<(u32, Vec3)>) {
        let (mut rows, mut marks) = (Vec::new(), Vec::new());
        let mut last = None;
        for (row, item) in self.item[..self.grown].iter().enumerate() {
            let Some(item) = item.filter(|i| items.contains(i)) else {
                continue;
            };
            rows.push(row as u32);
            if last != Some(item) {
                last = Some(item);
                // The row draws world * local; the item's origin is where
                // local's inverse takes the row's own origin.
                let local = &self.local[self.piece_of[row] as usize];
                let origin =
                    math::inverse(local).map_or([0.; 3], |inv| [inv[12], inv[13], inv[14]]);
                marks.push((row as u32, origin));
            }
        }
        (rows, marks)
    }

    /// Changes whenever `set` or `set_pieces` lays the rows out again.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Rows placed, for profiling and tests.
    #[cfg(test)]
    pub fn rows(&self) -> usize {
        self.item.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{Item, Placements};
    use crate::island::models::Piece;
    use gpui_editor_kit::{
        gpu::{Material, Renderer, cube},
        math,
    };
    use std::{collections::HashMap, sync::Arc};

    #[test]
    fn rows_map_to_items_and_moves_rewrite_only_their_rows() {
        let piece = |x: f32| Piece {
            mesh: Arc::new(cube(0xffffff, Material::Flat)),
            rdm: "".into(),
            local: math::translate([x, 0., 0.]),
            lods: Vec::new(),
            ppm: Vec::new(),
            foliage: false,
        };
        // A two-piece house and a one-piece tree.
        // An empty model sorted right before the house shares its first
        // piece index, as an animated worker shares its `_pin` cfg's.
        let loaded = HashMap::from([
            ("house.cfg".to_owned(), Arc::new(vec![piece(0.), piece(1.)])),
            ("hous.cfg".to_owned(), Arc::new(Vec::new())),
            ("tree.cfg".to_owned(), Arc::new(vec![piece(0.)])),
        ]);
        let renderer = Renderer::default();
        let mut p = Placements::default();
        p.set_pieces(&loaded, &renderer);
        let at = |x: f32| math::translate([x, 0., 0.]);
        let missing = p.set(
            [
                (Some(Item::Prop(1)), "tree.cfg", at(10.)),
                (Some(Item::Object(2)), "house.cfg", at(20.)),
                (Some(Item::Prop(3)), "unloaded.cfg", at(30.)),
                (None, "tree.cfg", at(40.)),
            ],
            &renderer,
        );
        assert_eq!(missing, vec![Item::Prop(3)]);
        assert_eq!(p.rows(), 4);
        assert_eq!(p.item_at(0), Some(Item::Prop(1)));
        assert_eq!(p.item_at(2), Some(Item::Object(2)));
        assert_eq!(p.item_at(3), None, "vegetation is not pickable");
        assert_eq!(p.item_at(9), None);
        // The house's second piece sits one metre along its own x.
        assert_eq!(math::translation(&p.transform[2]), [21., 0., 0.]);

        let before = p.transform.clone();
        p.move_items(&mut [(Item::Object(2), at(50.))], &renderer);
        assert_eq!(math::translation(&p.transform[1]), [50., 0., 0.]);
        assert_eq!(math::translation(&p.transform[2]), [51., 0., 0.]);
        assert_eq!(p.transform[0], before[0]);
        assert_eq!(p.transform[3], before[3]);
    }
}
