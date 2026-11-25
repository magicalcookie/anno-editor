// Owns the models the viewports draw as GPU placements: the piece table
// built from the loaded models, and the placement rows, one per placed piece,
// with the item each belongs to so picks and drags map back. Rows are
// rebuilt from the document on structural edits; a drag rewrites only its
// items' rows. The renderer culls and picks detail levels on the GPU, and
// bends the pieces the game bends to the terrain (gpu::adapting).
use super::{Item, models};
use gpui_editor_kit::{
    gpu::{Piece, Renderer, adapting},
    math::{self, Mat4, Vec3},
};
use rayon::prelude::*;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// A loaded model's pieces in the renderer's piece table: `count` from
/// `first`. A model whose parts all were skipped (bone-linked, no mesh) has
/// none and shares `first` with the next model.
#[derive(Clone, Copy)]
pub struct Model {
    first: u32,
    count: u32,
}

#[derive(Default)]
pub struct Placements {
    /// Loaded models by `.cfg` path; filled by `set_pieces`.
    models: foldhash::HashMap<String, Model>,
    /// Each piece's matrix in its model, by piece index. Shared with
    /// `Plan`s, which build rows off the UI thread.
    local: Arc<Vec<Mat4>>,
    /// Per piece: it rides the ground at its own origin (a cfg sub-file
    /// with `AdaptTerrainHeight`).
    follow: Arc<Vec<bool>>,
    /// Each piece's full mesh's own bounds, for the rows' bounds.
    piece_bounds: Arc<Vec<[Vec3; 2]>>,
    /// One per row, struct of arrays: the item (none for the vegetation
    /// preview, which is not pickable), piece index and transform.
    pub item: Vec<Option<Item>>,
    piece_of: Vec<u32>,
    transform: Vec<Mat4>,
    /// First row of the vegetation preview: `set` places it last, so a
    /// stroke can swap those rows alone (`set_grown`).
    grown: usize,
    generation: u64,
    /// Changes with every change to the rows, so a `Plan` built from older
    /// ones is not applied over newer.
    revision: u64,
}

/// Rows laid out on the UI thread for `build` to finish off it: which rows
/// there are (cheap: a model lookup per item), and their matrices and
/// bounds still to work out.
pub struct Plan {
    item: Vec<Option<Item>>,
    piece_of: Vec<u32>,
    /// Per placed item: its model's first piece, piece count, world matrix
    /// and whether it may bend.
    chosen: Vec<(u32, u32, Mat4, bool)>,
    local: Arc<Vec<Mat4>>,
    follow: Arc<Vec<bool>>,
    piece_bounds: Arc<Vec<[Vec3; 2]>>,
    revision: u64,
}

/// A finished `Plan`, for `Placements::apply`.
pub struct Built {
    item: Vec<Option<Item>>,
    piece_of: Vec<u32>,
    transform: Vec<Mat4>,
    bounds: [Vec3; 2],
    revision: u64,
}

impl Plan {
    /// The rows' matrices and their bounds, across cores. Off the UI thread
    /// in the editor: 64k rows took 8-60 ms on a loaded machine, as long
    /// as the cores were busy elsewhere.
    pub fn build(self) -> Built {
        let Plan {
            item,
            piece_of,
            chosen,
            local,
            follow,
            piece_bounds,
            revision,
        } = self;
        let transform: Vec<Mat4> = (chosen.par_iter())
            .flat_map_iter(|&(first, count, world, allow)| {
                let (local, follow) = (&local, &follow);
                (first..first + count).map(move |piece| row(local, follow, &world, piece, allow))
            })
            .collect();
        // Chunks merged after: mins with mins, maxes with maxes, so an
        // empty chunk's inverted box does not count as points.
        let bounds = (piece_of.par_chunks(4096).zip(transform.par_chunks(4096)))
            .map(|(ps, ms)| {
                math::bounds(ps.iter().zip(ms).flat_map(|(&p, m)| {
                    math::corners(piece_bounds[p as usize]).map(|c| math::transform(m, c))
                }))
            })
            .reduce(
                || math::bounds(std::iter::empty()),
                |a, b| {
                    [0, 1].map(|k| {
                        std::array::from_fn(|i| match k {
                            0 => a[0][i].min(b[0][i]),
                            _ => a[1][i].max(b[1][i]),
                        })
                    })
                },
            );
        Built {
            item,
            piece_of,
            transform,
            bounds,
            revision,
        }
    }
}

/// Row `piece` of an item at `world`. `allow`: the game would let it bend
/// to the terrain (an island prop with `AdaptTerrainHeight`, any object);
/// only pieces whose cfg asks for it do.
fn row(local: &[Mat4], follow: &[bool], world: &Mat4, piece: u32, allow: bool) -> Mat4 {
    let m = math::multiply(world, &local[piece as usize]);
    adapting(m, allow, follow[piece as usize], m[13] - world[13])
}

impl Placements {
    /// Hands the loaded models' pieces to the renderer: each piece's full
    /// mesh and its coarser levels with the screen sizes they take over at,
    /// and with `animate` the pose it moves by. Clears the rows: they name
    /// pieces by index.
    pub fn set_pieces(
        &mut self,
        loaded: &HashMap<String, models::Loaded>,
        animate: bool,
        renderer: &Renderer,
    ) {
        self.models.clear();
        let (mut local, mut follow, mut piece_bounds) = (Vec::new(), Vec::new(), Vec::new());
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
                local.push(piece.local);
                follow.push(piece.follow);
                let pose = piece.pose.clone().filter(|_| animate);
                // A posed piece's rows reach as far as its pose takes it.
                piece_bounds.push(pose.as_ref().map_or(piece.mesh.bounds(), |p| p.bounds));
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
                    impostor: piece.foliage.then(|| {
                        // Heights in metres: some trees are modelled small
                        // and scaled up by their part.
                        let [lo, hi] = piece.mesh.bounds();
                        let up = math::length([l[4], l[5], l[6]]);
                        models::impostor_ppm((hi[1] - lo[1]) * up)
                    }),
                    pose,
                });
            }
        }
        renderer.set_pieces(pieces);
        self.local = Arc::new(local);
        self.follow = Arc::new(follow);
        self.piece_bounds = Arc::new(piece_bounds);
        self.generation += 1;
        self.revision += 1;
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

    fn row(&self, world: &Mat4, piece: u32, allow: bool) -> Mat4 {
        row(&self.local, &self.follow, world, piece, allow)
    }

    /// Replaces every row from `placed` (item, model, world matrix, may
    /// bend to the terrain) and sends them to the renderer, in one go.
    /// Returns the items whose model is not loaded, for markers.
    #[cfg(test)]
    pub fn set<'a>(
        &mut self,
        placed: impl IntoIterator<Item = (Option<Item>, &'a str, Mat4, bool)>,
        renderer: &Renderer,
    ) -> Vec<Item> {
        let placed =
            (placed.into_iter()).map(|(it, cfg, world, allow)| (it, self.model(cfg), world, allow));
        let (plan, missing) = self.plan_models(placed);
        let built = gpui_editor_kit::task::interactive(|| plan.build());
        self.apply(built, renderer);
        missing
    }

    /// The loaded model at `cfg`, looked up once for every item that uses
    /// it: by name per item, 68k props and plants took an 1800 island's
    /// first scene several milliseconds of hashing.
    pub fn model(&self, cfg: &str) -> Option<Model> {
        self.models.get(cfg).copied()
    }

    /// The rows `placed` (item, its model from `model`, world matrix, may
    /// bend) lay out, to `build` anywhere and `apply` here; and the items
    /// whose model is not loaded, for markers.
    pub fn plan_models(
        &self,
        placed: impl IntoIterator<Item = (Option<Item>, Option<Model>, Mat4, bool)>,
    ) -> (Plan, Vec<Item>) {
        let placed = placed.into_iter();
        let hint = placed.size_hint().0;
        let (mut item, mut piece_of) = (Vec::with_capacity(hint), Vec::with_capacity(hint));
        let mut chosen = Vec::with_capacity(hint);
        let mut missing = Vec::new();
        for (it, model, world, allow) in placed {
            let Some(model) = model else {
                missing.extend(it);
                continue;
            };
            for piece in model.first..model.first + model.count {
                item.push(it);
                piece_of.push(piece);
            }
            chosen.push((model.first, model.count, world, allow));
        }
        let plan = Plan {
            item,
            piece_of,
            chosen,
            local: self.local.clone(),
            follow: self.follow.clone(),
            piece_bounds: self.piece_bounds.clone(),
            revision: self.revision,
        };
        (plan, missing)
    }

    /// Takes `built`'s rows and sends them to the renderer, unless the rows
    /// changed since it was planned (a drag, a stroke, new pieces): then
    /// false, and the caller plans again.
    pub fn apply(&mut self, built: Built, renderer: &Renderer) -> bool {
        if built.revision != self.revision {
            return false;
        }
        self.item = built.item;
        self.piece_of = built.piece_of;
        self.transform = built.transform;
        self.grown = self
            .item
            .iter()
            .rposition(Option::is_some)
            .map_or(0, |k| k + 1);
        self.generation += 1;
        self.revision += 1;
        renderer.set_placements_bounded(
            self.piece_of.clone(),
            self.transform.clone(),
            built.bounds,
        );
        true
    }

    /// Replaces the vegetation rows with `grown` (model, world matrix),
    /// keeping every item's rows where they are. Models not loaded yet are
    /// skipped, as `set` skips them. Grown plants may bend, as the game's
    /// ground vegetation does.
    pub fn set_grown<'a>(
        &mut self,
        grown: impl IntoIterator<Item = (&'a str, Mat4)>,
        renderer: &Renderer,
    ) {
        self.revision += 1;
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
                self.transform.push(self.row(&world, piece, true));
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
        self.revision += 1;
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
                    // A move keeps whether the item may bend; `set` changes it.
                    let allow = self.transform[r][3] > 0.5;
                    self.transform[r] = self.row(&world, self.piece_of[r], allow);
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
            element: None,
            group: 0,
            lods: Vec::new(),
            ppm: Vec::new(),
            foliage: false,
            follow: false,
            pose: None,
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
        p.set_pieces(&loaded, false, &renderer);
        let at = |x: f32| math::translate([x, 0., 0.]);
        let missing = p.set(
            [
                (Some(Item::Prop(1)), "tree.cfg", at(10.), false),
                (Some(Item::Object(2)), "house.cfg", at(20.), true),
                (Some(Item::Prop(3)), "unloaded.cfg", at(30.), false),
                (None, "tree.cfg", at(40.), true),
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
        // Rows carry whether they may bend and each piece's rise over its
        // item's origin through the move.
        assert_eq!((p.transform[0][3], p.transform[2][3]), (0., 1.));
    }

    // Rows built off the UI thread land only over the rows they were
    // planned from: a drag in between makes the build stale, and the next
    // plan takes the dragged position. The bounds built with them are the
    // renderer's own.
    #[test]
    fn a_plan_built_elsewhere_applies_only_over_the_rows_it_came_from() {
        let piece = |x: f32| Piece {
            mesh: Arc::new(cube(0xffffff, Material::Flat)),
            rdm: "".into(),
            local: math::translate([x, 0., 0.]),
            element: None,
            group: 0,
            lods: Vec::new(),
            ppm: Vec::new(),
            foliage: false,
            follow: false,
            pose: None,
        };
        let loaded =
            HashMap::from([("house.cfg".to_owned(), Arc::new(vec![piece(0.), piece(1.)]))]);
        let renderer = Renderer::default();
        let mut p = Placements::default();
        p.set_pieces(&loaded, false, &renderer);
        let at = |x: f32| math::translate([x, 0., 0.]);
        let house = |x| [(Some(Item::Object(1)), "house.cfg", at(x), false)];
        p.set(house(0.), &renderer);
        // Planned as refresh_scene plans: the model looked up once.
        let model = p.model("house.cfg");
        assert!(model.is_some());
        let planned = |x| [(Some(Item::Object(1)), model, at(x), false)];

        let (plan, missing) = p.plan_models(planned(10.));
        assert!(missing.is_empty());
        let built = std::thread::spawn(move || plan.build()).join().unwrap();
        // A drag lands first.
        p.move_items(&mut [(Item::Object(1), at(5.))], &renderer);
        assert!(!p.apply(built, &renderer), "stale rows are not applied");
        assert_eq!(math::translation(&p.transform[0]), [5., 0., 0.]);

        let (plan, _) = p.plan_models(planned(10.));
        assert!(p.apply(plan.build(), &renderer));
        assert_eq!(math::translation(&p.transform[1]), [11., 0., 0.]);
        // The cube spans -0.5..0.5 around each piece's origin.
        let (plan, _) = p.plan_models(planned(10.));
        let built = plan.build();
        assert_eq!(built.bounds, [[9.5, -0.5, -0.5], [11.5, 0.5, 0.5]]);
    }
}
