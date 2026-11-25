// Anno's file formats: archives, models, asset tables, island and world
// documents and compiled UI. No GPU or UI code: the editor, tests and
// command-line tools share these.

/// `.a7m` islands of 117 and 1800 and the files beside them.
pub mod a7m;
/// ADF record trees, the binary container most 117 and 1800 data sits in.
pub mod adf;
/// The asset database: `assets.xml` GUIDs, templates, models and names.
pub mod assets;
/// Wwise sound banks and media, and the events models play.
pub mod audio;
/// Cloth: what `<Clothes>` entries name and how it moves.
pub mod cloth;
/// DDS headers of the textures the editor writes.
pub mod dds;
/// 2070's `.efc` particle effects: emitters, read, edited and baked.
pub mod efc;
/// 2070's `.isd` islands and `.www` worlds.
pub mod isd;
/// Model configs (`.cfg`, `.prp`): meshes, materials, props, decals,
/// particles, lights, joints and sequences.
pub mod model;
/// Compiled Phoenix UI assets.
pub mod phx;
/// RDA archives, read and packed in every version.
pub mod rda;
/// `.rdm` meshes, their skeletons, and animation files.
pub mod rdm;
/// Baked particle effects (`.rdp`).
pub mod rdp;
/// The u32 varint ADF and Phoenix files share.
mod varint;
/// The install as one layered file tree over its archives.
pub mod vfs;
/// Anno's hand-written engine XML: whole files that write back byte for
/// byte, and single values in text.
pub mod xml;
