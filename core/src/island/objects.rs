// The objects an island places, as the editor edits them in every game, and
// the store that maps them to and from 117's and 1800's gamedata records
// (`a7m::objects`). 2070's `.isd` records map in anno2070.

use std::sync::Arc;

use anno_formats::a7m::objects as gamedata;
use anno_formats::adf::Adf;
use anyhow::{Result, bail};

// The path shape and the yaw quaternion are the same in the editor as in
// the gamedata record.
pub use gamedata::{PathPoint, bezier_handles, yaw_rotation};

/// The list an object's record is stored in: an `AreaObjectManager` list
/// (117, 1800), or one of 2070's `m_GOPManager/Objects` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    /// Session objects: buildings, units, slots, markers, audio spots, and
    /// on shipped islands mostly `SimpleCfg` decoration with a
    /// [`Object::config`] model (vegetation, decals, kit pieces).
    GameObject,
    /// Always empty on shipped islands.
    NaturePreset,
    /// Editor-layer decoration (mostly `SimpleCfg` + config), coastlines,
    /// rivers, lakes, herds.
    EditorObject,
    EditorOnlyObject,
    /// `Prop`-template objects: a `Prop` transform and no `Mesh`.
    Prop,
    /// 2070: buildings, harbour walls, resources, units, rocks that block.
    Handle,
    /// 2070: animated feedback units (residents, herd members).
    Feedback,
    /// 2070: unselectable decoration and effects.
    Simple,
}

impl Section {
    /// The list's name in the file.
    pub fn name(self) -> &'static str {
        match self {
            Section::GameObject => "GameObject",
            Section::NaturePreset => "NaturePreset",
            Section::EditorObject => "EditorObject",
            Section::EditorOnlyObject => "EditorOnlyObject",
            Section::Prop => "Prop",
            Section::Handle => "Handle",
            Section::Feedback => "Feedback",
            Section::Simple => "Simple",
        }
    }

    fn of_gamedata(s: gamedata::Section) -> Section {
        match s {
            gamedata::Section::GameObject => Section::GameObject,
            gamedata::Section::NaturePreset => Section::NaturePreset,
            gamedata::Section::EditorObject => Section::EditorObject,
            gamedata::Section::EditorOnlyObject => Section::EditorOnlyObject,
            gamedata::Section::Prop => Section::Prop,
        }
    }

    fn gamedata(self) -> Option<gamedata::Section> {
        Some(match self {
            Section::GameObject => gamedata::Section::GameObject,
            Section::NaturePreset => gamedata::Section::NaturePreset,
            Section::EditorObject => gamedata::Section::EditorObject,
            Section::EditorOnlyObject => gamedata::Section::EditorOnlyObject,
            Section::Prop => gamedata::Section::Prop,
            Section::Handle | Section::Feedback | Section::Simple => return None,
        })
    }
}

/// Where a 2070 object's `.isd` record comes from on save. Indices count
/// the island's object records (`Handle`, `Feedback`, then `Simple`) in
/// file order, as loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// The loaded record itself.
    Source(u32),
    /// A new record, copied from loaded record `.0` and then overwritten.
    Template(u32),
    /// A new record, copied from another island's and then overwritten:
    /// that record alone, as an `.isd` document of one `Object` element.
    Foreign(Arc<[u8]>),
}

/// Where an object's record comes from on save.
#[derive(Debug, Clone, PartialEq)]
enum Origin {
    /// A 117 or 1800 gamedata record, which [`Objects::write`] writes.
    Gamedata(gamedata::Origin),
    /// A 2070 record, which anno2070 writes from the island's file.
    Record(Record),
}

/// One placed object. Positions are island metres in the same frame as
/// the island's props and height grid (`x` = grid column, `z` = grid row,
/// both times 2 for the 0.5 m height texels; `y` = height).
#[derive(Debug, Clone, PartialEq)]
pub struct Object {
    /// `AreaManagerData` key of the area that stores the object (0x0001 is
    /// the main area; 0x2001, 0x4001 are further areas). 0 for 2070's.
    pub area: u16,
    pub section: Section,
    /// Unique 64-bit object ID.
    pub id: u64,
    pub guid: u32,
    pub position: [f32; 3],
    /// Yaw in radians (`Direction`, 0 when absent).
    pub direction: f32,
    /// Full rotation as a quaternion (x, y, z, w). Shipped yaw-only objects
    /// store `(0, -sin(d/2), 0, cos(d/2))`; terrain-aligned ones add tilt.
    pub rotation: [f32; 4],
    /// Uniform for meshes, per axis for props; 1 when absent.
    pub scale: [f32; 3],
    /// Index into the asset's `cfgs` (`Variation`, 0 when absent). 600
    /// shipped 2070 records store -1, kept here as its bits (`u32::MAX`).
    pub variation: u32,
    pub skin: u32,
    /// `OwnerProfile`: participant profile slot. 2070: `m_PlayerID`, which
    /// is never 0 where present, so 0 stands for a record without one.
    pub owner: u32,
    /// Editor folder (`ObjectFolderID`).
    pub folder: Option<u32>,
    /// A `.cfg` that overrides the asset's model. Stored with backslashes;
    /// `Install::file` accepts them. Generic assets such as `SimpleCfg`
    /// rely on it.
    pub config: Option<String>,
    /// `BezierPath` points, for objects with a path.
    pub path: Option<Vec<PathPoint>>,
    origin: Origin,
}

impl Object {
    /// A 2070 object, saving into `record`: `guid` at the origin, unturned,
    /// in no area. Set the rest of its fields after.
    pub fn of_record(record: Record, section: Section, id: u64, guid: u32) -> Object {
        Object {
            area: 0,
            section,
            id,
            guid,
            position: [0.; 3],
            direction: 0.,
            rotation: yaw_rotation(0.),
            scale: [1.; 3],
            variation: 0,
            skin: 0,
            owner: 0,
            folder: None,
            config: None,
            path: None,
            origin: Origin::Record(record),
        }
    }

    fn of_gamedata(r: gamedata::Object) -> Object {
        Object {
            area: r.area,
            section: Section::of_gamedata(r.section),
            id: r.id,
            guid: r.guid,
            position: r.position,
            direction: r.direction,
            rotation: r.rotation,
            scale: r.scale,
            variation: r.variation,
            skin: r.skin,
            owner: r.owner,
            folder: r.folder,
            config: r.config,
            path: r.path,
            origin: Origin::Gamedata(r.origin),
        }
    }

    // The gamedata record this object writes as; an error for a 2070
    // object or one in a 2070 list.
    fn gamedata(&self) -> Result<gamedata::Object> {
        let Origin::Gamedata(origin) = &self.origin else {
            bail!(
                "object {:#x} is a 2070 record, which its own codec writes",
                self.id
            )
        };
        let Some(section) = self.section.gamedata() else {
            bail!(
                "object {:#x}: gamedata has no {:?} list",
                self.id,
                self.section
            )
        };
        Ok(gamedata::Object {
            area: self.area,
            section,
            id: self.id,
            guid: self.guid,
            position: self.position,
            direction: self.direction,
            rotation: self.rotation,
            scale: self.scale,
            variation: self.variation,
            skin: self.skin,
            owner: self.owner,
            folder: self.folder,
            config: self.config.clone(),
            path: self.path.clone(),
            origin: origin.clone(),
        })
    }

    /// The `.isd` record this object saves into, for 2070's objects.
    pub fn record(&self) -> Option<Record> {
        match &self.origin {
            Origin::Record(r) => Some(r.clone()),
            Origin::Gamedata(_) => None,
        }
    }

    /// A copy under `id` that saves as a new record modelled on this
    /// one's: a loaded record is copied, a new one keeps its own origin.
    pub fn copied(&self, id: u64) -> Object {
        let mut o = self.clone();
        o.id = id;
        o.origin = match &self.origin {
            Origin::Gamedata(g) => Origin::Gamedata(g.copied()),
            Origin::Record(Record::Source(k)) => Origin::Record(Record::Template(*k)),
            other => other.clone(),
        };
        o
    }

    /// The `.cfg` to draw: the per-object override, else the asset's
    /// variation. Out-of-range variations wrap (none on pirate_01); 2070's
    /// -1 draws the first.
    // TODO(rearch-l3): what the game draws for -1; the first model stands
    // in until a capture shows it.
    pub fn model<'a>(&'a self, asset_cfgs: &'a [String]) -> Option<&'a str> {
        if let Some(c) = &self.config {
            return Some(c);
        }
        if asset_cfgs.is_empty() {
            return None;
        }
        let k = (self.variation as i32).max(0) as usize;
        Some(&asset_cfgs[k % asset_cfgs.len()])
    }

    /// Sets the yaw and turns `rotation` by the same delta, keeping any
    /// terrain tilt.
    pub fn set_direction(&mut self, yaw: f32) {
        let delta = yaw - self.direction;
        let (s, c) = (-delta / 2.0).sin_cos();
        // q' = (0, s, 0, c) * q
        let [x, y, z, w] = self.rotation;
        self.rotation = [c * x + s * z, c * y + s * w, c * z - s * x, c * w - s * y];
        self.direction = yaw;
    }

    /// True for objects not yet in the file (copies and foreign records).
    pub fn is_new(&self) -> bool {
        match &self.origin {
            Origin::Gamedata(g) => !g.is_loaded(),
            Origin::Record(r) => !matches!(r, Record::Source(_)),
        }
    }
}

/// An object record to place on another island, for islands with nothing
/// of that kind to copy from (a new island has no slots or coastlines).
/// For 2070 the object alone, whose [`Record`] names what it copies.
#[derive(Debug, Clone)]
pub struct Template {
    pub object: Object,
}

impl Template {
    /// Places copies of `object`, a 2070 record object.
    pub fn of_record(object: Object) -> Template {
        assert!(object.record().is_some(), "a record object");
        Template { object }
    }
}

/// All placed objects of a 117 or 1800 island, with the gamedata areas
/// they were read from. Edit `objects` freely (move, rotate, delete, push
/// copies) and call [`Objects::write`].
#[derive(Debug, Clone)]
pub struct Objects {
    pub objects: Vec<Object>,
    areas: gamedata::Areas,
}

impl Objects {
    /// Decodes every area of a gamedata tree (`Island::gamedata()`).
    pub fn read(gamedata: &Adf) -> Result<Objects> {
        let (areas, records) = gamedata::Areas::read(gamedata)?;
        let objects = records.into_iter().map(Object::of_gamedata).collect();
        Ok(Objects { objects, areas })
    }

    // A fresh ID modelled on `like`'s, unused on this island.
    fn new_id(&self, like: u64) -> u64 {
        gamedata::new_id(self.objects.iter().map(|o| o.id), like)
    }

    /// The float fields of one component of `o`'s record (`River`, `Lake`
    /// …) by name, each as many floats as its payload holds. Empty for
    /// components the record lacks and for 2070's records.
    pub fn component_floats(&self, o: &Object, component: &str) -> Vec<(String, Vec<f32>)> {
        match &o.origin {
            Origin::Gamedata(g) => self.areas.component_floats(g, component),
            Origin::Record(_) => Vec::new(),
        }
    }

    /// The record of `objects[index]`, to place on another island with
    /// [`Objects::add_template`].
    pub fn template(&self, index: usize) -> Template {
        let o = &self.objects[index];
        let Origin::Gamedata(g) = &o.origin else {
            panic!("templates come from loaded objects")
        };
        Template {
            object: Object {
                origin: Origin::Gamedata(self.areas.carried(g)),
                ..o.clone()
            },
        }
    }

    /// Places a copy of another island's record at `position` in `area`,
    /// with an ID that is fresh on this island. Push the result.
    pub fn add_template(&self, t: &Template, area: u16, position: [f32; 3]) -> Object {
        let mut o = t.object.copied(self.new_id(t.object.id));
        o.area = area;
        o.position = position;
        o
    }

    /// Copy of `objects[index]` with a fresh ID, to push onto `objects`.
    pub fn duplicate(&self, index: usize) -> Object {
        let src = &self.objects[index];
        src.copied(self.new_id(src.id))
    }

    /// Writes all edits into `gamedata` (the tree this was read from, or an
    /// unedited copy of it). Areas without changes are left untouched, so an
    /// unedited write is byte-identical. Deleted IDs are also dropped from
    /// the object groups and `GameObjectLabelMap`.
    pub fn write(&self, gamedata: &mut Adf) -> Result<()> {
        let records = (self.objects.iter())
            .map(Object::gamedata)
            .collect::<Result<Vec<_>>>()?;
        self.areas.write(&records, gamedata)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 4], b: [f32; 4]) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn set_direction_turns_tilted_rotation_about_world_up() {
        let mut o = Object::of_record(Record::Source(0), Section::EditorObject, 1, 1);
        o.set_direction(1.0);
        assert!(close(o.rotation, yaw_rotation(1.0)));

        let tilted = [0.05, -0.38, 0.04, -0.92];
        let n = tilted.iter().map(|v| v * v).sum::<f32>().sqrt();
        o.rotation = tilted.map(|v| v / n);
        let before = o.rotation;
        o.set_direction(2.5);
        assert!(!close(o.rotation, before));
        o.set_direction(1.0);
        assert!(close(o.rotation, before));
    }

    #[test]
    fn model_prefers_config_override() {
        let cfgs = vec!["a.cfg".to_owned(), "b.cfg".to_owned()];
        let mut o = Object::of_record(Record::Source(0), Section::GameObject, 1, 1);
        assert_eq!(o.model(&[]), None);
        o.variation = 1;
        assert_eq!(o.model(&cfgs), Some("b.cfg"));
        o.config = Some("data\\x.cfg".into());
        assert_eq!(o.model(&cfgs), Some("data\\x.cfg"));
    }

    // A 2070 object is new once copied, and refuses to write as gamedata.
    #[test]
    fn a_record_object_copies_as_a_template_and_stays_out_of_gamedata() {
        let o = Object::of_record(Record::Source(3), Section::Handle, 7, 335);
        assert!(!o.is_new());
        let copy = o.copied(8);
        assert_eq!(copy.record(), Some(Record::Template(3)));
        assert!(copy.is_new());
        assert!(o.gamedata().is_err());
    }
}
