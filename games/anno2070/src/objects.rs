// The objects a 2070 island places (`m_GOPManager/Objects`), as the
// document holds them: buildings, walls, units and decoration as objects,
// trees, rocks and grass as props, and their edits written back into the
// records of the file as loaded. The record codec is `isd::objects`.

use anno_core::island::objects::{self as neutral, Record};
use anno_core::island::{Prop, decompose, trs};
use anno_formats::assets::Assets;
use anno_formats::isd::objects::{self as records, Object, Section, read_record};
use anno_formats::isd::{Doc, NodeId};
use anyhow::{Context, Result, bail, ensure};

/// The document object list a record of `section` reads into; `None` for
/// the props.
fn document_section(section: Section) -> Option<neutral::Section> {
    match section {
        Section::Handle => Some(neutral::Section::Handle),
        Section::Feedback => Some(neutral::Section::Feedback),
        Section::Simple => Some(neutral::Section::Simple),
        Section::Nature | Section::Grass => None,
    }
}

/// The list a document object of `section` saves into.
fn record_section(section: neutral::Section) -> Option<Section> {
    (Section::ALL.into_iter()).find(|&s| document_section(s) == Some(section))
}

/// The world matrix record `o` stands at.
fn matrix(o: &Object) -> gpui_editor_kit::math::Mat4 {
    trs(o.position, o.rotation, [o.scale; 3])
}

/// The `.cfg` record `o`'s asset draws it with, by variation, wrapping as
/// the later games' objects do; `None` for an asset without a model.
pub fn model<'a>(assets: &'a Assets, o: &Object) -> Option<&'a str> {
    model_of(assets, o.guid, o.variation)
}

/// The `.cfg` asset `guid` draws at `variation`.
fn model_of(assets: &Assets, guid: u32, variation: i32) -> Option<&str> {
    let cfgs = &assets.get(guid)?.cfgs;
    // TODO: what the game draws for -1; the first model
    // stands in until a capture shows it.
    let k = usize::try_from(variation).unwrap_or(0);
    let cfg = cfgs.get(k % cfgs.len().max(1))?;
    Some(cfg.as_str())
}

/// What the trees, rocks and grass among `objects` give the document: a
/// model file per kind ([`kinds`]), then a prop per record in file order,
/// with ids 1 and up, so a prop's id less one is its record's index among
/// them, and its file index its kind. Two kinds can draw the same model and
/// so list the same file twice: the file index is what tells a copy which
/// GUID and variation its record takes. A kind without a model (an unknown
/// GUID, an asset that draws nothing) names the empty file [`NO_MODEL`]: it
/// is listed and picked by its marker, and a save can tell a removed record
/// from one that never drew.
pub fn props(objects: &[Object], assets: Option<&Assets>) -> (Vec<String>, Vec<Prop>) {
    let nature: Vec<&Object> = (objects.iter())
        .filter(|o| document_section(o.section).is_none())
        .collect();
    let (kinds, of) = kinds(&nature);
    let meshes = (kinds.iter())
        .map(|k| {
            let cfg = assets.and_then(|a| model_of(a, k.guid, k.variation));
            cfg.map_or(NO_MODEL.to_owned(), |c| c.replace('\\', "/"))
        })
        .collect();
    let props = (nature.iter().zip(of).enumerate())
        .map(|(k, (o, file))| Prop {
            id: k as u64 + 1,
            matrix: matrix(o),
            source: anno_formats::a7m::island::Prop {
                file,
                position: o.position,
                rotation: o.rotation,
                scale: [o.scale; 3],
                color: [1.; 4],
                adapt_terrain_height: None,
            },
            auto_cliff: false,
        })
        .collect();
    (meshes, props)
}

/// One GUID and variation that trees, rocks or grass records hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Kind {
    guid: u32,
    variation: i32,
    /// The record a new one of this kind copies.
    first: Copies,
}

/// Where the record a new tree, rock or grass copies comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Copies {
    /// An index into the list `kinds` read.
    Record(usize),
    /// An index into the island's records from other islands
    /// (`Island::foreign`).
    Foreign(usize),
    /// None: a prop of the kind can't save.
    Nothing,
}

/// The kinds among the trees, rocks and grass `nature`, in order of their
/// first record, and each record's kind. A load and a save of the same
/// file give the same table, so it needs no state of its own: a prop's
/// file index below its length is a kind, and one above is a model the
/// island never had a record of, whose kind `write` asks the database for.
fn kinds(nature: &[&Object]) -> (Vec<Kind>, Vec<u32>) {
    // A few hundred kinds per island at most, packed GUID over variation:
    // a scan of one small array beats hashing 11,000 records.
    let mut keys: Vec<u64> = Vec::new();
    let mut kinds: Vec<Kind> = Vec::new();
    let of = (nature.iter().enumerate())
        .map(|(r, o)| {
            let key = (u64::from(o.guid) << 32) | u64::from(o.variation as u32);
            let k = keys.iter().position(|&k| k == key).unwrap_or_else(|| {
                keys.push(key);
                kinds.push(Kind {
                    guid: o.guid,
                    variation: o.variation,
                    first: Copies::Record(r),
                });
                keys.len() - 1
            });
            k as u32
        })
        .collect();
    (kinds, of)
}

/// The asset templates of trees, rocks and grass: what shipped `Nature`
/// and `Grass` lists hold, and no other list does.
const PROP_TEMPLATES: [&str; 4] = [
    "Tree",
    "NatureMountainObject",
    "SimpleStoneObject",
    "Vegetation",
];

/// The GUID and variation of the tree, rock or grass asset that draws
/// `cfg`; `None` for a model none draws, which places as an object.
pub fn prop_kind(assets: &Assets, cfg: &str) -> Option<(u32, i32)> {
    prop_asset(assets, cfg).map(|(a, k)| (a.guid, k as i32))
}

fn prop_asset<'a>(
    assets: &'a Assets,
    cfg: &str,
) -> Option<(&'a anno_formats::assets::Asset, usize)> {
    let key = |c: &str| c.replace('\\', "/").to_ascii_lowercase();
    let want = key(cfg);
    (assets.iter())
        .filter(|a| PROP_TEMPLATES.contains(&a.template.as_str()))
        .find_map(|a| Some((a, a.cfgs.iter().position(|c| key(c) == want)?)))
}

/// The list a new record of `asset` goes in, as shipped islands place it:
/// trees and rocks in `Nature`, bushes, kelp and coral in `Grass`. Of the
/// vegetation, only the dead tree (324) and the water lily (318) ship in
/// `Nature`; no GUID ships in both lists.
fn list_of(asset: &anno_formats::assets::Asset) -> Section {
    match (asset.template.as_str(), asset.guid) {
        ("Vegetation", 318 | 324) => Section::Nature,
        ("Vegetation", _) => Section::Grass,
        _ => Section::Nature,
    }
}

/// The model file of an object that has none.
pub const NO_MODEL: &str = "";

/// The document objects the buildings, walls, units and decoration among
/// `objects` read into, in file order, each saving into its own record
/// ([`Record::Source`], counting these records only).
pub fn document_objects(objects: &[Object]) -> Vec<neutral::Object> {
    let placed = objects
        .iter()
        .filter_map(|o| Some((o, document_section(o.section)?)));
    (placed.enumerate())
        .map(|(k, (o, section))| document_object(o, Record::Source(k as u32), section))
        .collect()
}

/// The document object record `o` reads as, saving into `record`.
fn document_object(o: &Object, record: Record, section: neutral::Section) -> neutral::Object {
    let mut n = neutral::Object::of_record(record, section, o.id, o.guid);
    n.position = o.position;
    n.direction = o.yaw();
    n.rotation = o.rotation;
    n.scale = [o.scale; 3];
    n.variation = o.variation as u32;
    n.owner = o.player.unwrap_or(0);
    n
}

/// A new object of the asset that draws `cfg`, to place on an island
/// holding `placed`: with the GUID and variation the database gives `cfg`,
/// modelled on a record of its kind, so the game finds the components it
/// expects. The island's own record of that GUID comes first, then the
/// first one `install`'s islands ship, then one on the island of an asset
/// with the same template (a `ResidenceBuilding` for a residence). Where
/// none is, it fails: a record made up from nothing would guess the
/// components. Its position, turn and id are the caller's.
///
/// For a tree, rock or grass, which the document holds as a prop, it is
/// only the carrier of a shipped record of its asset (`Record::Foreign`,
/// in the `Prop` list), for `Island::foreign`: what a new prop of that
/// kind copies where the island holds no record to copy.
pub fn template(
    install: &anno_formats::vfs::Install,
    assets: &Assets,
    placed: &[neutral::Object],
    cfg: &str,
) -> Result<neutral::Object> {
    let key = |c: &str| c.replace('\\', "/").to_ascii_lowercase();
    let want = key(cfg);
    // Assets sharing a model all qualify; (asset, variation).
    let drawn: Vec<(&anno_formats::assets::Asset, usize)> = (assets.iter())
        .filter_map(|a| Some((a, a.cfgs.iter().position(|c| key(c) == want)?)))
        .collect();
    let (asset, _) = *drawn
        .first()
        .with_context(|| format!("no 2070 asset draws {cfg}"))?;
    let named = |mut o: neutral::Object, (a, k): (&anno_formats::assets::Asset, usize)| {
        o.guid = a.guid;
        o.variation = k as u32;
        o
    };
    if let Some((a, k)) = prop_asset(assets, cfg) {
        if let Some((o, d)) = shipped(install, &[(a, k)], true)? {
            return Ok(named(o, d));
        }
        // As a building copies one of its template: a tree another tree's
        // record, which `write` gives this one's GUID and variation.
        let kin: Vec<_> = (assets.iter())
            .filter(|b| b.template == a.template && list_of(b) == list_of(a))
            .map(|b| (b, 0))
            .collect();
        return match shipped(install, &kin, true)? {
            Some((o, d)) => Ok(named(o, d)),
            None => bail!(
                "no shipped 2070 island has a record of {} ({})",
                a.name,
                a.template
            ),
        };
    }
    let of = |guid: u32| drawn.iter().copied().find(|(a, _)| a.guid == guid);
    if let Some((o, d)) = placed.iter().find_map(|o| Some((o, of(o.guid)?))) {
        return Ok(named(o.copied(0), d));
    }
    if let Some((o, d)) = shipped(install, &drawn, false)? {
        return Ok(named(o, d));
    }
    let kin = placed.iter().find(|o| {
        assets
            .get(o.guid)
            .is_some_and(|a| a.template == asset.template)
    });
    match kin {
        Some(o) => Ok(named(o.copied(0), drawn[0])),
        None => bail!(
            "no shipped 2070 island has a record of {} ({}) to copy",
            asset.name,
            asset.template
        ),
    }
}

/// The first record of a `drawn` asset, in `drawn`'s order, that
/// `install`'s islands ship, as a new object copying it, and its asset.
/// With `props`, it looks among the trees, rocks and grass instead, and the
/// object only carries the record.
fn shipped<'a>(
    install: &anno_formats::vfs::Install,
    drawn: &[(&'a anno_formats::assets::Asset, usize)],
    props: bool,
) -> Result<Option<(neutral::Object, (&'a anno_formats::assets::Asset, usize))>> {
    let index = Shipped::of(install)?;
    for &(a, k) in drawn {
        let Some((section, bytes)) = index.record(a.guid) else {
            continue;
        };
        let kind = match (document_section(section), props) {
            (Some(kind), false) => kind,
            (None, true) => neutral::Section::Prop,
            _ => continue,
        };
        let r = read_record(&Doc::parse(bytes)?, section)?;
        return Ok(Some((
            document_object(&r, Record::Foreign(bytes.into()), kind),
            (a, k),
        )));
    }
    Ok(None)
}

/// A new object copying the first record of asset `guid` that `install`'s
/// islands ship (`Record::Foreign`), as it stands there; `None` where none
/// ships one in a building, unit or decoration list.
pub fn shipped_object(
    install: &anno_formats::vfs::Install,
    guid: u32,
) -> Result<Option<neutral::Object>> {
    let index = Shipped::of(install)?;
    let Some((section, bytes)) = index.record(guid) else {
        return Ok(None);
    };
    let Some(kind) = document_section(section) else {
        return Ok(None);
    };
    let r = read_record(&Doc::parse(bytes)?, section)?;
    Ok(Some(document_object(
        &r,
        Record::Foreign(bytes.into()),
        kind,
    )))
}

/// The first record of each asset the install's islands ship, and whether
/// an asset turns by quarter turns: what Place and
/// conversion copy for an asset an island lacks, and how Place turns it.
/// Searching the islands for one building took 1.8 s on the UI thread in
/// a debug build; this reads them once per install, on a thread of its own
/// from the first island open ([`Shipped::warm`]).
pub struct Shipped {
    /// Sorted; the other arrays follow it.
    guids: Vec<u32>,
    lists: Vec<Section>,
    /// The first record, a one-record document, as a range of `bytes`.
    spans: Vec<[u32; 2]>,
    bytes: Vec<u8>,
    /// Bit per GUID: it turns by quarter turns ([`Shipped::quarter_turns`]).
    quarter: Vec<u64>,
}

/// How far off a quarter turn a record may stand and still count as on
/// one: 1/100 of it. 46 shipped buildings sit up to 0.001 off, the drift of
/// a turn stored in fixed point; free-turned ones are tenths off.
const QUARTER_SLACK: i64 = Object::QUARTER_TURN / 100;

/// One index per install folder (a process opens one or two), each built
/// once, from the folder with 2070's archive order (`build`): what a caller
/// passes may be opened in another order or with a project mounted, and
/// whichever caller came first decided what the index held. `Islands`
/// hands its methods an install, not editor state, so the index can't
/// ride beside the database.
static SHIPPED: std::sync::Mutex<Vec<(std::path::PathBuf, ShippedSlot)>> =
    std::sync::Mutex::new(Vec::new());

/// How often this process read an index: once per install it opens.
static SHIPPED_READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

type ShippedSlot = std::sync::Arc<std::sync::OnceLock<Result<std::sync::Arc<Shipped>, String>>>;

impl Shipped {
    /// Reads every island `install` ships. One that doesn't parse is left
    /// out and said, rather than failing the index for the process.
    pub fn read(install: &anno_formats::vfs::Install) -> Result<Shipped> {
        SHIPPED_READS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // GUID to its index in the arrays below while they grow.
        let mut at: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
        let (mut guids, mut lists, mut spans, mut bytes) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // Per GUID: records, and how many stand off a quarter turn.
        let mut turns: Vec<[u32; 2]> = Vec::new();
        for f in 0..install.file_count() {
            let f = anno_formats::vfs::FileId(f as u32);
            let path = install.file_path(f);
            if !path.ends_with(".isd") {
                continue;
            }
            let parsed = (install.read_file(f))
                .and_then(|file| Ok(Doc::parse(&file)?))
                .and_then(|doc| {
                    let rs = (records::records(&doc).into_iter())
                        .map(|(section, o)| Ok((section, o, Object::read(&doc, o, section)?)))
                        .collect::<Result<Vec<_>>>()?;
                    Ok((doc, rs))
                });
            let (doc, rs) = match parsed {
                Ok(read) => read,
                Err(e) => {
                    eprintln!("{path}: left out of the shipped records: {e:#}");
                    continue;
                }
            };
            for (section, o, r) in rs {
                let d = r.direction.rem_euclid(Object::QUARTER_TURN);
                let off = u32::from(d.min(Object::QUARTER_TURN - d) > QUARTER_SLACK);
                match at.get(&r.guid) {
                    Some(&k) => turns[k] = [turns[k][0] + 1, turns[k][1] + off],
                    None => {
                        at.insert(r.guid, guids.len());
                        let one = records::excerpt(&doc, o)?;
                        let start = bytes.len() as u32;
                        bytes.extend_from_slice(&one);
                        guids.push(r.guid);
                        lists.push(section);
                        spans.push([start, bytes.len() as u32]);
                        turns.push([1, off]);
                    }
                }
            }
        }
        let mut order: Vec<usize> = (0..guids.len()).collect();
        order.sort_unstable_by_key(|&k| guids[k]);
        let mut quarter = vec![0u64; order.len().div_ceil(64)];
        for (n, &k) in order.iter().enumerate() {
            // Campaign and test islands turn a few buildings by hand (an
            // Ecos pioneer house of 143, a power plant of 54, one of the
            // 12,900 harbour walls); deposit buildings, units and props
            // turn freely on far more.
            let [records, off] = turns[k];
            if off * 8 <= records {
                quarter[n / 64] |= 1 << (n % 64);
            }
        }
        Ok(Shipped {
            guids: order.iter().map(|&k| guids[k]).collect(),
            lists: order.iter().map(|&k| lists[k]).collect(),
            spans: order.iter().map(|&k| spans[k]).collect(),
            bytes,
            quarter,
        })
    }

    /// The index of `install`'s folder: built here if no thread is
    /// building it, else waited for.
    pub fn of(install: &anno_formats::vfs::Install) -> Result<std::sync::Arc<Shipped>> {
        let (slot, _) = Self::slot(install.dir());
        let built = slot.get_or_init(|| Self::build(install.dir()));
        built.clone().map_err(|e| anyhow::anyhow!("{e}"))
    }

    // The folder's index, read from an install of it opened here in the
    // game's archive order, whoever asks first.
    fn build(dir: &std::path::Path) -> Result<std::sync::Arc<Shipped>, String> {
        let install = anno_formats::vfs::Install::open(dir, crate::ARCHIVE_FAMILIES)
            .map_err(|e| format!("{e:#}"))?;
        Shipped::read(&install)
            .map(std::sync::Arc::new)
            .map_err(|e| format!("{e:#}"))
    }

    /// `install`'s index if it is built; else `None`, and building it
    /// starts on a thread of its own (`warm`). For the UI thread, which
    /// must not wait the second it takes.
    pub fn ready(install: &anno_formats::vfs::Install) -> Option<Result<std::sync::Arc<Shipped>>> {
        let (slot, _) = Self::slot(install.dir());
        match slot.get() {
            Some(built) => Some(built.clone().map_err(|e| anyhow::anyhow!("{e}"))),
            None => {
                Self::warm(install);
                None
            }
        }
    }

    /// Starts building `install`'s index on a thread of its own, unless it
    /// is built or being built, so Place and conversion find it ready.
    pub fn warm(install: &anno_formats::vfs::Install) {
        let (slot, new) = Self::slot(install.dir());
        if !new {
            return;
        }
        let dir = install.dir().to_owned();
        let spawned = std::thread::Builder::new()
            .name("2070 shipped records".into())
            .spawn(move || {
                slot.get_or_init(|| Self::build(&dir));
            });
        if let Err(e) = spawned {
            eprintln!("2070 shipped records built on first use: {e}");
        }
    }

    /// The install folder's slot, and whether this call made it.
    fn slot(dir: &std::path::Path) -> (ShippedSlot, bool) {
        let mut all = SHIPPED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, slot)) = all.iter().find(|(d, _)| d == dir) {
            return (slot.clone(), false);
        }
        let slot = ShippedSlot::default();
        all.push((dir.to_owned(), slot.clone()));
        (slot, true)
    }

    fn find(&self, guid: u32) -> Option<usize> {
        self.guids.binary_search(&guid).ok()
    }

    /// The list and bytes (a one-record document) of the first record of
    /// `guid` the install ships.
    pub fn record(&self, guid: u32) -> Option<(Section, &[u8])> {
        let k = self.find(guid)?;
        let [a, b] = self.spans[k];
        Some((self.lists[k], &self.bytes[a as usize..b as usize]))
    }

    /// Whether `guid` turns by quarter turns: at most an eighth of its
    /// shipped records stand off one (193 of the 373 assets islands ship,
    /// 0 of the trees, rocks and grass). False for one no island ships.
    pub fn quarter_turns(&self, guid: u32) -> bool {
        self.find(guid)
            .is_some_and(|k| self.quarter[k / 64] & (1 << (k % 64)) != 0)
    }

    /// How many indexes this process has read.
    pub fn reads() -> usize {
        SHIPPED_READS.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many assets the install's islands ship.
    pub fn len(&self) -> usize {
        self.guids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guids.is_empty()
    }
}

/// Writes `props` and `placed`, the document's trees, rocks and grass and
/// its objects, into `out`, a fresh parse of the file they were read from.
/// A kept record changes only the transform, GUID, variation and owner
/// fields that differ from it. A removed one goes, and so do the links that
/// name its id. A new object is a copy of the record it was modelled on,
/// and a new tree, rock or grass a copy of the island's first record of its
/// kind, added at the end of that record's list under a fresh id: one
/// counter numbers all five lists, and the links name the same ids, so it
/// takes one past the largest the file uses.
///
/// A prop of a kind the file never placed (`meshes` past the loaded kinds,
/// as a conversion adds them) takes the GUID and variation of the tree,
/// rock or grass asset `assets` says draws its model. It copies the file's
/// record of that GUID, else one of `foreign` (records other islands ship,
/// `Island::foreign`), else a record of its list, else a tree's.
pub fn write<'a>(
    out: &mut Doc,
    props: impl IntoIterator<Item = &'a Prop>,
    meshes: &[String],
    placed: &[neutral::Object],
    foreign: &[std::sync::Arc<[u8]>],
    assets: Option<&Assets>,
    file: &str,
) -> Result<()> {
    let mut nodes = records::records(out);
    let loaded: Vec<Object> = (nodes.iter().enumerate())
        .map(|(k, &(section, o))| {
            Object::read(out, o, section).with_context(|| format!("{file}: {section:?} object {k}"))
        })
        .collect::<Result<_>>()?;
    // Indices into `nodes`: the object records, which `Record` counts, and
    // the trees, rocks and grass, which prop ids count from 1.
    let (objects, nature): (Vec<usize>, Vec<usize>) =
        (0..nodes.len()).partition(|&k| document_section(nodes[k].0).is_some());

    enum Fate<'a> {
        Removed,
        Object(&'a neutral::Object),
        Prop(&'a Prop, Kind),
    }
    let mut fate: Vec<Fate> = nodes.iter().map(|_| Fate::Removed).collect();
    // Other islands' records, parsed once each, with how each reads: first
    // the island's for new trees, rocks and grass (`foreign`), then those
    // new objects copy. A harbour lays hundreds of one tile.
    let mut shipped: Vec<(std::sync::Arc<[u8]>, Doc, Object)> = (foreign.iter())
        .map(|bytes| {
            let doc = Doc::parse(bytes)?;
            let was = read_record(&doc, Section::Nature)?;
            Ok((bytes.clone(), doc, was))
        })
        .collect::<Result<_>>()
        .with_context(|| format!("{file}: a record from another island"))?;
    let for_props = shipped.len();
    /// What a new record copies: one of the file's, or another island's
    /// (an index into `shipped`).
    enum Model {
        Record(usize),
        Foreign(usize),
    }
    // (its list, what it copies, the object), in document order.
    let mut added: Vec<(Section, Model, &neutral::Object)> = Vec::new();
    for o in placed {
        let (k, new) = match o.record() {
            Some(Record::Source(k)) => (k, false),
            Some(Record::Template(k)) => (k, true),
            Some(Record::Foreign(bytes)) => {
                let section = record_section(o.section).with_context(|| {
                    format!("{file}: object {} in a {:?} list", o.id, o.section)
                })?;
                let f = match (shipped.iter().skip(for_props)).position(|(b, ..)| *b == bytes) {
                    Some(f) => for_props + f,
                    None => {
                        let doc = Doc::parse(&bytes)?;
                        let was = read_record(&doc, section).with_context(|| file.to_owned())?;
                        shipped.push((bytes, doc, was));
                        shipped.len() - 1
                    }
                };
                added.push((section, Model::Foreign(f), o));
                continue;
            }
            None => bail!("{file}: object {:#x} is no 2070 record", o.id),
        };
        let &r = (objects.get(k as usize)).with_context(|| {
            format!(
                "{file}: object {} names record {k} of {}",
                o.id,
                objects.len()
            )
        })?;
        if new {
            added.push((nodes[r].0, Model::Record(r), o));
            continue;
        }
        ensure!(
            matches!(fate[r], Fate::Removed),
            "{file}: two objects save into record {k}"
        );
        fate[r] = Fate::Object(o);
    }
    // The island's own records from other islands, for props.
    let foreign = &shipped[..for_props];
    let (mut kinds, _) = kinds(&nature.iter().map(|&r| &loaded[r]).collect::<Vec<_>>());
    let held = kinds.len();
    // Models the file never placed, past the loaded kinds: a kind each,
    // from the database, copying a record of the list it belongs in.
    for cfg in meshes.iter().skip(held) {
        let Some((asset, variation)) = assets.and_then(|a| prop_asset(a, cfg)) else {
            // Kept so file indices stay put; a prop drawing it fails
            // below.
            kinds.push(Kind {
                guid: 0,
                variation: -2,
                first: Copies::Nothing,
            });
            continue;
        };
        let same = (nature.iter()).position(|&r| loaded[r].guid == asset.guid);
        let shipped = (foreign.iter()).position(|(.., was)| was.guid == asset.guid);
        let list = list_of(asset);
        // Another island's record of an asset of the same list: where this
        // island holds no record to copy, what Place found in its stead.
        let kin = || {
            (foreign.iter()).position(|(.., was)| {
                assets
                    .and_then(|a| a.get(was.guid))
                    .is_some_and(|b| list_of(b) == list)
            })
        };
        let first = match (same, shipped) {
            (Some(r), _) => Copies::Record(r),
            (None, Some(f)) => Copies::Foreign(f),
            (None, None) => (nature.iter())
                .position(|&r| nodes[r].0 == list)
                // Grass copies a tree's fields where the island holds no grass.
                .or_else(|| nature.iter().position(|&r| nodes[r].0 == Section::Nature))
                .map(Copies::Record)
                .or_else(|| kin().map(Copies::Foreign))
                .unwrap_or(Copies::Nothing),
        };
        kinds.push(Kind {
            guid: asset.guid,
            variation: variation as i32,
            first,
        });
    }
    // A prop's file index is its kind (`props`).
    let kind = |p: &Prop| {
        let k = (kinds.get(p.source.file as usize))
            .filter(|k| k.first != Copies::Nothing)
            .with_context(|| {
                let cfg = meshes
                    .get(p.source.file as usize)
                    .map_or("", String::as_str);
                if assets.is_none() {
                    return format!(
                        "{file}: prop {} draws {cfg:?}, a tree, rock or grass the island \
                         never had; it needs the asset database, which is still loading",
                        p.id
                    );
                }
                format!(
                    "{file}: prop {} draws {cfg:?}, which no 2070 tree, rock or grass asset \
                     draws, or the island holds no such record to copy",
                    p.id
                )
            })?;
        Ok::<_, anyhow::Error>(*k)
    };
    // (its list, the record it copies, the prop, its kind), in document
    // order.
    let mut new_props: Vec<(Section, Copies, &Prop, Kind)> = Vec::new();
    for p in props {
        let k = kind(p)?;
        match (p.id.checked_sub(1)).and_then(|j| nature.get(j as usize)) {
            Some(&r) => {
                ensure!(
                    matches!(fate[r], Fate::Removed),
                    "{file}: two props save into record {}",
                    p.id
                );
                fate[r] = Fate::Prop(p, k);
            }
            None => {
                // A kind the file never held goes in its asset's list.
                let asset_list = || assets.and_then(|a| a.get(k.guid)).map(list_of);
                let (copies, list) = match k.first {
                    Copies::Record(j) => {
                        let r = nature[j];
                        let list = match p.source.file as usize >= held {
                            true => asset_list().unwrap_or(nodes[r].0),
                            false => nodes[r].0,
                        };
                        (Copies::Record(r), list)
                    }
                    Copies::Foreign(f) => {
                        let list = asset_list().unwrap_or(Section::Nature);
                        (Copies::Foreign(f), list)
                    }
                    Copies::Nothing => unreachable!("refused by `kind`"),
                };
                new_props.push((list, copies, p, k));
            }
        }
    }

    // A kind the island never held may belong in a list the file lacks
    // (183 shipped islands have no `Grass`): it starts empty, in its place
    // among the five. Records keep their order, so only their ids move.
    let mut grew = false;
    for section in Section::ALL {
        if records::list(out, section).is_none() && new_props.iter().any(|(l, ..)| *l == section) {
            records::add_list(out, section).with_context(|| file.to_owned())?;
            grew = true;
        }
    }
    if grew {
        nodes = records::records(out);
    }

    let mut gone: Vec<u64> = (fate.iter().zip(&loaded))
        .filter(|(f, _)| matches!(f, Fate::Removed))
        .map(|(_, o)| o.id)
        .collect();
    gone.sort_unstable();
    let mut next = records::largest_id(out, &loaded) + 1;

    // New records first, from the last list back. Copies go at their
    // list's end, which moves only the nodes after it, so every record a
    // later copy is modelled on (in its own list, or an earlier one) keeps
    // its id; and a copy of a record the edit removes copies it before it
    // goes. Each node pass of the codec walks the whole document, so a
    // list's copies of this file's records go in one pass, and so do all
    // the removals below: a save stays linear in what it adds and drops.
    for section in Section::ALL.into_iter().rev() {
        let mut srcs = Vec::new();
        let mut copies = Vec::new();
        for (_, model, o) in added.iter().filter(|(s, _, _)| *s == section) {
            if let &Model::Record(r) = model {
                srcs.push(nodes[r].1);
                copies.push(Copy::Object(&loaded[r], o));
            }
        }
        let list = |out: &Doc| {
            records::list(out, section).with_context(|| format!("{file}: no {section:?} list"))
        };
        // Another island's record: its first copy goes in whole, at the
        // list's end, and the rest are copies of that one, which the batch
        // below appends after it. Each whole copy is a pass of its own.
        let mut firsts: Vec<(usize, NodeId, Copy)> = Vec::new();
        let mut from_shipped = |f: usize, copy, out: &mut Doc| -> Result<Option<NodeId>> {
            if let Some(&(_, t, _)) = firsts.iter().find(|(g, ..)| *g == f) {
                return Ok(Some(t));
            }
            let (_, doc, _) = &shipped[f];
            let t = records::append_record(out, section, doc)
                .with_context(|| format!("{file}: {section:?}"))?;
            firsts.push((f, t, copy));
            Ok(None)
        };
        for (_, model, o) in added.iter().filter(|(s, _, _)| *s == section) {
            if let &Model::Foreign(f) = model {
                let copy = Copy::Object(&shipped[f].2, o);
                if let Some(t) = from_shipped(f, copy, out)? {
                    srcs.push(t);
                    copies.push(copy);
                }
            }
        }
        for &(_, from, p, k) in new_props.iter().filter(|(s, ..)| *s == section) {
            match from {
                Copies::Record(r) => {
                    srcs.push(nodes[r].1);
                    copies.push(Copy::Prop(&loaded[r], p, k));
                }
                Copies::Foreign(f) => {
                    let copy = Copy::Prop(&shipped[f].2, p, k);
                    if let Some(t) = from_shipped(f, copy, out)? {
                        srcs.push(t);
                        copies.push(copy);
                    }
                }
                Copies::Nothing => unreachable!("refused by `kind`"),
            }
        }
        if !srcs.is_empty() {
            let at = out.append_copies(list(out)?, &srcs)?;
            // From the last back: a field a write adds or drops moves only
            // the copies after it, which are written.
            for (k, (&o, copy)) in at.iter().zip(&copies).enumerate().rev() {
                records::renumber(out, o, next + k as u64)?;
                copy.write(out, o)?;
            }
            next += at.len() as u64;
        }
        // From the last back, after their copies: the first copy of each
        // record from another island.
        for (k, &(_, t, copy)) in firsts.iter().enumerate().rev() {
            records::renumber(out, t, next + k as u64)?;
            copy.write(out, t)?;
        }
        next += firsts.len() as u64;
    }

    // The loaded records lead their lists, in order, ahead of the copies.
    // Which ones are still there, as `kept` gives each loaded record.
    let originals = |out: &Doc, kept: &dyn Fn(usize) -> bool| -> Result<Vec<(usize, NodeId)>> {
        let mut now = records::records(out).into_iter().peekable();
        let mut found = Vec::new();
        for section in Section::ALL {
            for r in (0..nodes.len()).filter(|&r| nodes[r].0 == section && kept(r)) {
                let (s, o) = now.next().context("a loaded record went missing")?;
                ensure!(s == section, "{file}: a {section:?} record went missing");
                found.push((r, o));
            }
            while now.next_if(|&(s, _)| s == section).is_some() {}
        }
        Ok(found)
    };
    let removed_record = |r: usize| matches!(fate[r], Fate::Removed);
    let mut removed: Vec<NodeId> = (originals(out, &|_| true)?.into_iter())
        .filter(|&(r, _)| removed_record(r))
        .map(|(_, o)| o)
        .collect();
    records::unlink(out, &gone, &mut removed).with_context(|| file.to_owned())?;
    out.remove_all(&removed);
    // From the last back, as the copies above.
    for (r, o) in originals(out, &|r| !removed_record(r))?.into_iter().rev() {
        match fate[r] {
            Fate::Removed => unreachable!("removed above"),
            Fate::Object(n) => Copy::Object(&loaded[r], n).write(out, o)?,
            Fate::Prop(p, k) => Copy::Prop(&loaded[r], p, k).write(out, o)?,
        }
    }
    Ok(())
}

/// What a document object or prop saves into its record: the record as it
/// reads, then what the document holds.
#[derive(Clone, Copy)]
enum Copy<'a> {
    Object(&'a Object, &'a neutral::Object),
    Prop(&'a Object, &'a Prop, Kind),
}

impl Copy<'_> {
    /// Writes into record `o`, which reads as the record this holds.
    fn write(self, out: &mut Doc, o: NodeId) -> Result<()> {
        let now = match self {
            Copy::Object(was, n) => Object {
                guid: n.guid,
                variation: n.variation as i32,
                position: n.position,
                direction: was.turned_to(n.direction),
                rotation: n.rotation,
                scale: n.scale[0],
                player: (n.owner != 0).then_some(n.owner),
                ..was.clone()
            },
            // Moving decomposes the matrix again, which leaves the last bits
            // of an unmoved prop loose: one whose matrix holds still keeps
            // its transform.
            Copy::Prop(was, p, kind)
                if (matrix(was).iter().zip(&p.matrix)).all(|(a, b)| a.to_bits() == b.to_bits()) =>
            {
                Object {
                    guid: kind.guid,
                    variation: kind.variation,
                    ..was.clone()
                }
            }
            Copy::Prop(was, p, kind) => {
                let (t, q, s) = decompose(&p.matrix);
                Object {
                    guid: kind.guid,
                    variation: kind.variation,
                    position: t,
                    // The yaw the shell gives objects: local x turns to
                    // (cos, 0, sin).
                    direction: was.turned_to(p.matrix[2].atan2(p.matrix[0])),
                    rotation: q,
                    scale: s[0],
                    ..was.clone()
                }
            }
        };
        let was = match self {
            Copy::Object(was, _) | Copy::Prop(was, ..) => was,
        };
        was.write(out, o, &now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::isd::Value;

    // A text field's number, read from the bytes.
    fn text_number(doc: &Doc, n: NodeId) -> Option<i64> {
        match doc.value(n) {
            Value::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
            _ => None,
        }
    }

    fn floats<const N: usize>(b: &[u8]) -> [f32; N] {
        std::array::from_fn(|k| f32::from_le_bytes(b[k * 4..][..4].try_into().unwrap()))
    }
    use anno_formats::vfs::Install;

    // Needs Anno 2070: n_m02's objects stand on its ground, which says how
    // a record's position reads: x, then z, then height, all in metres. Read
    // the other way round (z before x) they would float or sink. A tilted
    // rock keeps its stored quaternion; a yaw-only tree's stored one is its
    // direction's.
    #[test]
    fn n_m02_objects_stand_on_its_ground() {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let bytes = install.read(path).unwrap();
        let island = crate::island::from_bytes(
            bytes.clone().into(),
            path,
            crate::island::registry(&install).unwrap(),
            None,
        )
        .unwrap();
        let field = &island.doc.field;
        let objects = records::read(&Doc::parse(&bytes).unwrap(), path).unwrap();
        let nature: Vec<&Object> = objects
            .iter()
            .filter(|o| o.section == Section::Nature)
            .collect();
        // Most stand on the flat 0.7 m plateau, where both readings agree;
        // count where they don't.
        let (mut slopes, mut right, mut swapped) = (0, 0, 0);
        for o in &nature {
            let [x, y, z] = o.position;
            let (a, b) = (field.height([x, z]), field.height([z, x]));
            if (a - b).abs() > 1. {
                slopes += 1;
                right += usize::from((y - a).abs() < 0.25);
                swapped += usize::from((y - b).abs() < 0.25);
            }
        }
        eprintln!(
            "of {slopes} trees and rocks on slopes, {right} on the ground, {swapped} with x and z swapped"
        );
        // Rocks sink into steep ground: 665 of 837 within a quarter metre.
        assert!(
            slopes > 500 && right * 4 >= slopes * 3,
            "{right} of {slopes}"
        );
        assert!(swapped * 20 < slopes, "{swapped} of {slopes}");

        // The first nature record: GUID 313 at d = -651125.
        let tree = nature[0];
        assert_eq!(tree.guid, 313);
        let d = -651_125. / Object::QUARTER_TURN as f32 * std::f32::consts::FRAC_PI_2;
        let derived = [0., -(d / 2.).sin(), 0., (d / 2.).cos()];
        assert!(
            (0..4).all(|k| (tree.rotation[k] - derived[k]).abs() < 1e-4),
            "{:?}",
            tree.rotation
        );
        assert!((tree.scale - 1.057_109).abs() < 1e-6);
    }

    /// `<name>inner</name>` and its CRLF, as `.isd` files close elements.
    pub(crate) fn element(name: &str, inner: &[u8]) -> Vec<u8> {
        [
            format!("<{name}>").as_bytes(),
            inner,
            format!("</{name}>\r\n").as_bytes(),
        ]
        .concat()
    }

    /// A binary value: `CDATA[` and its length before the bytes.
    pub(crate) fn cdata(bytes: &[u8]) -> Vec<u8> {
        [
            b"CDATA[",
            &(bytes.len() as u32).to_le_bytes()[..],
            bytes,
            b"]",
        ]
        .concat()
    }

    pub(crate) fn position(p: [f32; 3]) -> Vec<u8> {
        // 4096 per metre, x, z, then height.
        let [x, y, z] = p.map(|v| (v * 4096.) as i64);
        cdata(&[x, z, y].map(i64::to_le_bytes).concat())
    }

    // A handle record without a player, turned a quarter, at variation -1,
    // whose renderer draws it tilted and at twice its size, and a tree
    // beside it: the handle reads as a document object with its record's
    // id, GUID and position, the render parameters' rotation and scale, and
    // -1 kept as its bits, drawing its asset's first model. The tree is a
    // prop.
    #[test]
    fn records_read_as_objects_and_trees_as_props() {
        let tilt = [0.1, -0.7, 0.05, 0.7];
        let render = [
            vec![0; 30],
            [3.5f32, 0.75, 9.25].map(f32::to_le_bytes).concat(),
            tilt.map(f32::to_le_bytes).concat(),
            2f32.to_le_bytes().to_vec(),
            vec![0; 16],
        ]
        .concat();
        let handle = [
            element("m_ID", b"41"),
            element("m_GUID", b"335"),
            element("m_Variation", b"-1"),
            element("m_Position", &position([3.5, 0.75, 9.25])),
            element("m_Direction", b"368640"),
            element("Blocking", b""),
            element(
                "PropertyMesh",
                &element("m_RenderParameters", &cdata(&render)),
            ),
        ]
        .concat();
        let tree = [
            element("m_ID", b"7"),
            element("m_GUID", b"313"),
            element("m_Position", &position([1., 0.5, 2.])),
            element("m_PlayerID", b"15"),
            element("m_Direction", b"0"),
            element("Nature", &element("m_Scale", b"0.5")),
        ]
        .concat();
        let list = |records: &[&[u8]]| {
            let inner: Vec<u8> = records.iter().flat_map(|r| element("Object", r)).collect();
            element("Objects", &inner)
        };
        let lists = [
            element("Handle", &list(&[&handle])),
            element("Nature", &list(&[&tree])),
        ]
        .concat();
        let bytes = element("m_GOPManager", &element("Objects", &lists));
        let records = records::read(&Doc::parse(&bytes).unwrap(), "fixture").unwrap();
        let placed = document_objects(&records);
        assert_eq!(placed.len(), 1);
        let o = &placed[0];
        assert_eq!(o.record(), Some(Record::Source(0)));
        assert_eq!(o.section, neutral::Section::Handle);
        assert_eq!((o.id, o.guid, o.owner), (41, 335, 0));
        assert_eq!(o.position, [3.5, 0.75, 9.25]);
        assert_eq!(o.rotation, tilt);
        assert_eq!(o.scale, [2.; 3]);
        assert!((o.direction - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert_eq!(o.variation, u32::MAX);
        let cfgs = ["first.cfg".to_owned(), "second.cfg".to_owned()];
        assert_eq!(o.model(&cfgs), Some("first.cfg"));

        let (meshes, props) = super::props(&records, None);
        assert_eq!(meshes, [NO_MODEL]);
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].id, 1);
        assert_eq!(props[0].source.position, [1., 0.5, 2.]);
        assert_eq!(props[0].source.scale, [0.5; 3]);
    }

    // Needs Anno 2070: n_m02's 29 handles and one simple record load as
    // the document's objects, each with its record's id, GUID, variation and
    // transform, and its 5,338 trees and rocks as props. The values are read
    // off the file's own text and bytes.
    #[test]
    fn n_m02_objects_load_as_objects() {
        let Some(dir) = anno_formats::vfs::install_dir(crate::PROFILE.env_var) else {
            return;
        };
        let install = Install::open(&dir, crate::ARCHIVE_FAMILIES).unwrap();
        let path = "data/levels/islands/normal/n_m02.isd";
        let island = crate::island::from_bytes(
            install.read(path).unwrap().into(),
            path,
            crate::island::registry(&install).unwrap(),
            None,
        )
        .unwrap();
        let objects = &island.doc.objects;
        assert_eq!(objects.len(), 30);
        assert_eq!(island.doc.props.len(), 5_338);
        let sections: Vec<_> = objects.iter().map(|o| o.section).collect();
        assert!(
            sections[..29]
                .iter()
                .all(|&s| s == neutral::Section::Handle)
        );
        assert_eq!(sections[29], neutral::Section::Simple);
        for (k, o) in objects.iter().enumerate() {
            assert_eq!(o.record(), Some(Record::Source(k as u32)));
        }
        let metres = |v: i64| v as f32 / 4096.;

        // <m_ID>18184</m_ID> <m_GUID>41027</m_GUID>, no variation, player
        // 15, direction 0; position fe2e09 8dfa08 330b.
        let first = &objects[0];
        assert_eq!((first.id, first.guid, first.variation), (18184, 41027, 0));
        assert_eq!(first.owner, 15);
        let at = [metres(0x09_2efe), metres(0x0b33), metres(0x08_fa8d)];
        assert_eq!(first.position, at);
        assert_eq!(first.direction, 0.);
        assert_eq!(first.scale, [1.; 3]);

        // 17617, GUID 335 at direction -99942: its render parameters' yaw,
        // (0, 0x3e58654e, 0, 0x3f7a37f1).
        let rock = &objects[1];
        assert_eq!((rock.id, rock.guid), (17617, 335));
        let q = [
            0.,
            f32::from_bits(0x3e58_654e),
            0.,
            f32::from_bits(0x3f7a_37f1),
        ];
        assert_eq!(rock.rotation, q);
        let yaw = -99_942. / 368_640. * std::f32::consts::FRAC_PI_2;
        assert!((rock.direction - yaw).abs() < 1e-6);

        // 18170, GUID 41001, <m_Variation>1</m_Variation>.
        let third = &objects[2];
        assert_eq!((third.id, third.guid, third.variation), (18170, 41001, 1));

        // The simple record: 18304, GUID 352, below the sea at -6932/4096.
        let simple = &objects[29];
        assert_eq!((simple.id, simple.guid), (18304, 352));
        let at = [metres(0x02_b3e9), metres(-6932), metres(0x04_77fd)];
        assert_eq!(simple.position, at);
    }

    /// A handle record: `extra` holds its children after `m_Direction`.
    fn handle(id: u64, guid: u32, at: [f32; 3], extra: &[u8]) -> Vec<u8> {
        [
            element("m_ID", id.to_string().as_bytes()),
            element("m_GUID", guid.to_string().as_bytes()),
            element("m_Position", &position(at)),
            element("m_PlayerID", b"15"),
            element("m_Direction", b"0"),
            extra.to_vec(),
        ]
        .concat()
    }

    /// `m_RenderParameters` with a block at `at`, unturned, at scale 1:
    /// the 78-byte form, or the 130-byte one with its second copy.
    fn render(at: [f32; 3], long: bool) -> Vec<u8> {
        let block = [
            at.map(f32::to_le_bytes).concat(),
            [0f32, 0., 0., 1., 1.].map(f32::to_le_bytes).concat(),
            vec![7; 16],
        ]
        .concat();
        let r = match long {
            true => [vec![0; 30], block.clone(), vec![0; 4], block].concat(),
            false => [vec![0; 30], block].concat(),
        };
        element("PropertyMesh", &element("m_RenderParameters", &cdata(&r)))
    }

    /// Three handles: 10 drawn by the 130-byte form, 11 a wall whose child
    /// is 10, 12 drawn by the 78-byte form; a tree, 7; 10 and 11 snapped
    /// together; names for 10 and for 30, which the island lacks; and
    /// groups {10, 11} and {10}.
    fn linked() -> Vec<u8> {
        let a = handle(10, 335, [3.5, 0.75, 9.25], &render([3.5, 0.75, 9.25], true));
        let wall = element(
            "Wall",
            &[
                element("m_ChildHID", b"10"),
                element("m_bInvisible", b"0"),
                element("m_bDisconnected", b"0"),
            ]
            .concat(),
        );
        let b = handle(
            11,
            342,
            [5., 0.75, 9.],
            &[wall, element("Blocking", b"")].concat(),
        );
        let c = handle(12, 343, [8., 0.75, 2.], &render([8., 0.75, 2.], false));
        let tree = [
            element("m_ID", b"7"),
            element("m_GUID", b"313"),
            element("m_Position", &position([1., 0.5, 2.])),
            element("m_Direction", b"0"),
            element(
                "Nature",
                &element(
                    "m_Orientation",
                    &cdata(&[0f32, 0., 0., 1.].map(f32::to_le_bytes).concat()),
                ),
            ),
        ]
        .concat();
        let list = |records: &[&[u8]]| {
            let inner: Vec<u8> = records.iter().flat_map(|r| element("Object", r)).collect();
            element("Objects", &inner)
        };
        let lists = [
            element("Handle", &list(&[&a, &b, &c])),
            element("Feedback", &element("Objects", b"")),
            element("Nature", &list(&[&tree])),
        ]
        .concat();
        let pairs = |pairs: &[(&str, &str)]| -> Vec<u8> {
            (pairs.iter())
                .flat_map(|(k, v)| {
                    [element("k", k.as_bytes()), element("v", v.as_bytes())].concat()
                })
                .collect()
        };
        let group = |ids: &[u32]| {
            element(
                "i",
                &cdata(
                    &ids.iter()
                        .flat_map(|i| i.to_le_bytes())
                        .collect::<Vec<u8>>(),
                ),
            )
        };
        let gop = [
            element("Objects", &lists),
            element("SnapPartner", &pairs(&[("10", "11"), ("11", "10")])),
            element("ObjectNames", &pairs(&[("10", "dam"), ("30", "gone")])),
            element("ObjectGroups", &[group(&[10, 11]), group(&[10])].concat()),
        ]
        .concat();
        [element("m_GOPManager", &gop), element("Width", b"16")].concat()
    }

    fn loaded(bytes: &[u8]) -> (Vec<Object>, Vec<Prop>, Vec<neutral::Object>) {
        let records = records::read(&Doc::parse(bytes).unwrap(), "fixture").unwrap();
        let (_, props) = super::props(&records, None);
        let placed = document_objects(&records);
        (records, props, placed)
    }

    fn saved(bytes: &[u8], props: &[Prop], placed: &[neutral::Object]) -> Vec<u8> {
        let mut out = Doc::parse(bytes).unwrap();
        write(&mut out, props, &[], placed, &[], None, "fixture").unwrap();
        out.encode()
    }

    fn texts(doc: &Doc, list: &str) -> Vec<String> {
        let gop = doc.root("m_GOPManager").unwrap();
        (doc.child(gop, list)
            .into_iter()
            .flat_map(|l| doc.children(l)))
        .map(|n| match doc.value(n) {
            Value::Text(t) => String::from_utf8_lossy(t).into_owned(),
            _ => String::new(),
        })
        .collect()
    }

    fn groups(doc: &Doc) -> Vec<Vec<u32>> {
        let gop = doc.root("m_GOPManager").unwrap();
        (doc.children(doc.child(gop, "ObjectGroups").unwrap()))
            .map(|g| match doc.value(g) {
                Value::Blob(b) => b
                    .chunks(4)
                    .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    // Untouched, the fixture writes back as it is. Moving handle 10 changes
    // its `m_Position` and both copies of its drawn position and nothing
    // else; a copy of handle 12 lands at the end of the handles as 31, one
    // past the 30 a name still links to, where it was put; moving the tree
    // changes its position alone.
    #[test]
    fn a_move_and_a_copy_write_only_their_records() {
        let bytes = linked();
        let (records, mut props, mut placed) = loaded(&bytes);
        assert_eq!(saved(&bytes, &props, &placed), bytes);

        placed[0].position = [4.5, 0.75, 11.25];
        let mut copy = placed[2].copied(99);
        copy.position = [20., 1., 20.];
        placed.push(copy);
        props[0].matrix[12] += 2.;
        let out = saved(&bytes, &props, &placed);

        let expected = {
            let mut doc = Doc::parse(&bytes).unwrap();
            let nodes = records::records(&doc);
            let a = nodes[0].1;
            let p = doc.child(a, "m_Position").unwrap();
            doc.set(p, &position([4.5, 0.75, 11.25])[10..34], true)
                .unwrap();
            let r = doc
                .child(doc.child(a, "PropertyMesh").unwrap(), "m_RenderParameters")
                .unwrap();
            let Value::Blob(b) = doc.value(r) else {
                unreachable!()
            };
            let mut b = b.to_vec();
            for at in [30, 82] {
                b[at..at + 12]
                    .copy_from_slice(&[4.5f32, 0.75, 11.25].map(f32::to_le_bytes).concat());
            }
            doc.set(r, &b, true).unwrap();
            let tree = nodes[3].1;
            let p = doc.child(tree, "m_Position").unwrap();
            doc.set(p, &position([3., 0.5, 2.])[10..34], true).unwrap();
            let handles = records::list(&doc, Section::Handle).unwrap();
            let c = doc.append_copies(handles, &[nodes[2].1]).unwrap()[0];
            doc.set(doc.child(c, "m_ID").unwrap(), b"31", false)
                .unwrap();
            let p = doc.child(c, "m_Position").unwrap();
            doc.set(p, &position([20., 1., 20.])[10..34], true).unwrap();
            let r = doc
                .child(doc.child(c, "PropertyMesh").unwrap(), "m_RenderParameters")
                .unwrap();
            let Value::Blob(b) = doc.value(r) else {
                unreachable!()
            };
            let mut b = b.to_vec();
            b[30..42].copy_from_slice(&[20f32, 1., 20.].map(f32::to_le_bytes).concat());
            doc.set(r, &b, true).unwrap();
            doc.encode()
        };
        assert!(out == expected, "{}", String::from_utf8_lossy(&out));

        let (back, _, again) = loaded(&out);
        assert_eq!(back.len(), records.len() + 1);
        let added = &again[3];
        assert_eq!(
            (added.id, added.guid, added.position),
            (31, 343, [20., 1., 20.])
        );
        assert_eq!(added.record(), Some(Record::Source(3)));
        assert_eq!(again[0].position, [4.5, 0.75, 11.25]);
        assert_eq!(again[0].rotation, [0., 0., 0., 1.]);
    }

    // Removing handle 10 drops its record and every link to it: the wall
    // that held it as its child holds none, the snap pairs and its name go,
    // its id leaves the first group, and the group of it alone goes.
    // Removing the tree drops its record.
    #[test]
    fn a_removed_record_takes_its_links_along() {
        let bytes = linked();
        let (_, props, mut placed) = loaded(&bytes);
        placed.remove(0);
        let out = saved(&bytes, &props[1..], &placed);
        let doc = Doc::parse(&out).unwrap();
        let back = records::read(&doc, "out").unwrap();
        assert_eq!(back.iter().map(|o| o.id).collect::<Vec<_>>(), [11, 12]);
        let wall = doc.child(records::records(&doc)[0].1, "Wall").unwrap();
        assert_eq!(
            text_number(&doc, doc.child(wall, "m_ChildHID").unwrap()),
            Some(0)
        );
        assert!(texts(&doc, "SnapPartner").is_empty());
        assert_eq!(texts(&doc, "ObjectNames"), ["30", "gone"]);
        assert_eq!(groups(&doc), [vec![11]]);
    }

    // A copy of a building whose `LinkedObjects` holds something (no
    // shipped one does) saves with it empty, while its original keeps it;
    // removing the first handle and copying the third in one save leaves
    // the links of the second alone.
    #[test]
    fn a_copy_claims_none_of_its_originals_linked_objects() {
        let mut doc = Doc::parse(&linked()).unwrap();
        let c = records::records(&doc)[2].1;
        let mesh = doc.child(c, "PropertyMesh");
        let l = doc.insert(c, mesh, "LinkedObjects").unwrap();
        doc.set(l, &10u32.to_le_bytes(), true).unwrap();
        let bytes = doc.encode();
        let (_, props, mut placed) = loaded(&bytes);
        placed.push(placed[2].copied(99));
        placed.remove(0);
        let out = Doc::parse(&saved(&bytes, &props, &placed)).unwrap();
        let handles: Vec<NodeId> = (records::records(&out).into_iter())
            .filter(|&(s, _)| s == Section::Handle)
            .map(|(_, o)| o)
            .collect();
        let ids: Vec<i64> = (handles.iter())
            .map(|&o| text_number(&out, out.child(o, "m_ID").unwrap()).unwrap())
            .collect();
        assert_eq!(ids, [11, 12, 31]);
        let linked = |o: NodeId| out.value(out.child(o, "LinkedObjects").unwrap());
        assert_eq!(linked(handles[1]), Value::Blob(&10u32.to_le_bytes()));
        assert_eq!(linked(handles[2]), Value::Empty);
        assert_eq!(texts(&out, "ObjectNames"), ["30", "gone"]);
    }

    // A turn rewrites the direction and both drawn rotations, a new
    // variation and a dropped owner their fields. A copy of the tree saves
    // as a copy of its record, numbered past every id; a prop drawing a
    // model no record on the island has refuses to save.
    #[test]
    fn turns_variations_owners_and_new_trees_save() {
        let bytes = linked();
        let (_, props, mut placed) = loaded(&bytes);
        let quarter = neutral::yaw_rotation(std::f32::consts::FRAC_PI_2);
        placed[0].rotation = quarter;
        placed[0].direction = std::f32::consts::FRAC_PI_2;
        placed[0].variation = 2;
        placed[0].owner = 0;
        let out = saved(&bytes, &props, &placed);
        let doc = Doc::parse(&out).unwrap();
        let a = records::records(&doc)[0].1;
        assert_eq!(
            text_number(&doc, doc.child(a, "m_Direction").unwrap()),
            Some(368_640)
        );
        assert_eq!(
            text_number(&doc, doc.child(a, "m_Variation").unwrap()),
            Some(2)
        );
        assert!(doc.child(a, "m_PlayerID").is_none());
        let back = document_objects(&records::read(&doc, "out").unwrap());
        assert_eq!(back[0].rotation, quarter);
        assert_eq!((back[0].variation, back[0].owner), (2, 0));
        let r = doc
            .child(doc.child(a, "PropertyMesh").unwrap(), "m_RenderParameters")
            .unwrap();
        let Value::Blob(r) = doc.value(r) else {
            unreachable!()
        };
        assert_eq!(floats::<4>(&r[42..]), quarter, "the second copy turns too");

        let mut more = props.clone();
        let mut copy = Prop {
            id: 2,
            ..props[0].clone()
        };
        copy.matrix[12] += 4.;
        more.push(copy);
        let out = saved(&bytes, &more, &placed);
        let back = records::read(&Doc::parse(&out).unwrap(), "out").unwrap();
        let trees: Vec<&Object> = back
            .iter()
            .filter(|o| o.section == Section::Nature)
            .collect();
        assert_eq!(trees.len(), 2);
        assert_eq!((trees[1].id, trees[1].guid), (31, 313));
        assert_eq!(trees[1].position, [5., 0.5, 2.]);
        assert_eq!(trees[1].rotation, trees[0].rotation);

        more[1].source.file = 1;
        let mut out = Doc::parse(&bytes).unwrap();
        let refused = write(&mut out, &more, &[], &placed, &[], None, "fixture").unwrap_err();
        assert!(refused.to_string().contains("never had"), "{refused}");
    }
}
