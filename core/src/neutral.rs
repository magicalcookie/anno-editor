// Owns the model every game's converter reads into and writes from: the
// static meshes a model draws, each with its materials and placement, its
// projector decals and the particle effects and lights it carries, all
// naming files of the game it came from. A game reads its own models into
// one and writes its own from one, so each game needs two converters
// instead of one per other game. Animated, skinned and bone-linked parts have no
// place in it: readers leave them out with a note.

use anno_formats::model::{Alpha, Transform};
use anyhow::Result;

/// A model of one game, in the shape every game's converter shares.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub pieces: Vec<Piece>,
    pub effects: Vec<Effect>,
    /// What the reader could not express, with why, for the user.
    pub left_out: Vec<String>,
}

/// One drawable piece: a mesh or a decal, placed in the model.
#[derive(Clone, Debug)]
pub struct Piece {
    /// The source path the piece's files are named after, without an
    /// extension: the model's own, or with `_part<k>` / `_decal<k>` when it
    /// has several. Two pieces with one name are one shared piece (a torch
    /// on five houses).
    pub name: String,
    /// How notes name it: "house.cfg roof_lod0".
    pub label: String,
    /// Where the piece sits in the model.
    pub at: Transform,
    pub shape: Shape,
    /// In the mesh's material order; one for a decal.
    pub materials: Vec<Material>,
    /// The tints a placement may pick from (117's `Colors`), linear RGB.
    pub tints: Vec<[f32; 3]>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// The source's `rdm` (its `_lod<N>` levels sit beside it by name),
    /// drawn under `transform`: a cfg part's own placement, which a game
    /// without part placements bakes into its copy of the mesh.
    Mesh { rdm: String, transform: Transform },
    /// A box `extents` across each way from its centre, placed by
    /// `transform`, that paints `uv_rect` of its material's diffuse onto
    /// whatever lies below.
    Decal {
        extents: [f32; 3],
        transform: Transform,
        uv_rect: [f32; 4],
        /// The source's own `DECAL` config, as a 117 cfg holds it (its
        /// transformer is `transform`). A writer for that cfg dialect copies
        /// it whole, so its type, opacity, fades, height map and grass come
        /// along; other writers go by the fields above.
        cfg: Option<String>,
    },
}

/// How a piece's surface is drawn. Textures are named as the source names
/// them (`x.psd`, or the shipped `x_0.dds`); see [`texture_source`].
#[derive(Clone, Debug, PartialEq)]
pub struct Material {
    pub diffuse: Option<String>,
    pub normal: Option<String>,
    pub metallic: Option<String>,
    /// Multiplies the diffuse, linear RGB.
    pub color: [f32; 3],
    pub double_sided: bool,
    pub alpha: Alpha,
    /// Shaded like the ground it stands on (117's terrain shader): rocks
    /// and ruins that take the ground's tint and grit.
    pub ground_shaded: bool,
    /// How strongly the ground's tint colours it, 0 for not at all.
    pub terrain_tint: f32,
}

/// A particle effect or light the model carries, which each game draws
/// with its own effects.
#[derive(Clone, Debug)]
pub struct Effect {
    /// What a game matches its own effects by: the source's path, and for
    /// a particle or light inside a model, the cfg's path and the particle
    /// file or light name after it.
    pub name: String,
    pub at: Transform,
    /// The note when the target game has nothing like it.
    pub missing: String,
    /// A light rather than particles. Games whose effects carry their own
    /// lights (1800's torch) don't place it.
    pub light: bool,
    /// The source's own `PARTICLE` or `LIGHT` config, as a 117 cfg holds it,
    /// for a writer of that cfg dialect to copy whole (colour, time scale,
    /// flicker). Its transformer is replaced by `at`.
    pub cfg: Option<String>,
}

/// Where a model's files come from and where they go in the mod.
pub trait Source {
    /// A file of the source game, by its path.
    fn read(&self, path: &str) -> Result<Vec<u8>>;
    /// Where the copy of source file `path` (or a file named after it)
    /// goes in the mod.
    fn place(&self, path: &str) -> String;
}

/// What writing one model made: every file to stage and what to place.
#[derive(Debug, Default)]
pub struct Ported {
    pub files: Vec<(String, Vec<u8>)>,
    /// What one placement of the source becomes: each written model and
    /// its offset in the source model. A plain prop is one piece at the
    /// origin; a 117 house is its own mesh plus a torch at each torch spot.
    pub pieces: Vec<(String, Transform)>,
    /// The target game's effect cfgs standing in for the source's particle
    /// effects (torch fire, smoke, waterfall foam), with their offsets. A
    /// prop can't carry particles, so these get placed as objects.
    pub effects: Vec<(String, Transform)>,
    /// Pieces and effects that could not come over, with why.
    pub left_out: Vec<String>,
}

/// A texture as a model names it: the artist's source (`x.psd`). A shipped
/// `x_0.dds` level names its source `x.psd`; any other name is kept.
pub fn texture_source(named: &str) -> String {
    let named = named.replace('\\', "/");
    match named.strip_suffix("_0.dds") {
        Some(stem) => format!("{stem}.psd"),
        None => named,
    }
}

/// The `<stem>_N.dds` levels the games read for texture `source` (as
/// [`texture_source`] names it), each where it goes in the mod.
pub fn texture_files(from: &dyn Source, source: &str) -> Vec<(String, Vec<u8>)> {
    let stem = source.rsplit_once('.').map_or(source, |(s, _)| s);
    (0..4)
        .map_while(|level| {
            let path = format!("{stem}_{level}.dds");
            let bytes = from.read(&path).ok()?;
            Some((from.place(&path), bytes))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_texture_levels_name_their_source() {
        assert_eq!(
            texture_source("data\\x\\bark_diff_0.dds"),
            "data/x/bark_diff.psd"
        );
        assert_eq!(
            texture_source("data/x/bark_diff.psd"),
            "data/x/bark_diff.psd"
        );
        assert_eq!(texture_source("data/x/bark.png"), "data/x/bark.png");
    }
}
