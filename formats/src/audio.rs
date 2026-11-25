// Sound: Wwise banks and streamed `.wem` media, and the events a model's
// sequences play, for 117 and 1800. Layouts and counts: docs/audio-format.md.

use anyhow::{Context, Result, anyhow, ensure};

use crate::vfs::Install;

/// Banks, and in 117 its `media/` folder of loose `.wem`s.
const SOUND_DIR: &str = "data/sound/generatedsoundbanks/windows";

/// Where a game keeps its sound and how its banks lay out the nodes read
/// here. The node offsets are measured on each game's bank version; a bank
/// of another version is left out rather than misread.
#[derive(Debug)]
struct Game {
    /// The `Audio` assets: GUID -> Wwise event. 117 keeps them apart from
    /// assets.xml, 1800 in it.
    assets: &'static str,
    /// Loose `.wem`s under [`SOUND_DIR`]: `media/` in 117, the bank folder
    /// itself in 1800. Voice is in a folder of the language's name below.
    media: &'static str,
    /// BKHD version. 1800 also ships three stale banks (versions 132 and
    /// 134) that its soundbanksinfo does not list.
    version: u32,
    /// Where an event's 7-bit varint action count starts: 117's events
    /// have 7 bytes before it that 1800's lack.
    event_actions: usize,
    /// A codec sound's source, before NodeBaseParams: 117's carries a u32
    /// cache id that 1800's lacks. A source plugin adds a u32-sized
    /// parameter block.
    source: usize,
    /// One NodeBaseParams FX entry.
    fx: usize,
    /// Bytes between the FX list and the output bus: 117's metadata
    /// override and count, 1800's attachment override.
    after_fx: usize,
}

static GAMES: [Game; 2] = [
    // Anno 117: Wwise 2025.1.6.
    Game {
        assets: "data/base/config/export/audio_generated.xml",
        media: "media/",
        version: 172,
        event_actions: 11,
        source: 22,
        fx: 6,
        after_fx: 2,
    },
    // Anno 1800: Wwise 2019.2.
    Game {
        assets: "data/config/export/main/asset/assets.xml",
        media: "",
        version: 135,
        event_actions: 4,
        source: 18,
        fx: 7,
        after_fx: 1,
    },
];

// HIRC object types.
const SOUND: u8 = 2;
const ACTION: u8 = 3;
const EVENT: u8 = 4;
const RANDOM_SEQUENCE: u8 = 5;
const SWITCH: u8 = 6;
const ACTOR_MIXER: u8 = 7;
const LAYER: u8 = 9;
/// Action type Play. The others (stop, set state, set parameter) start nothing.
const PLAY: u16 = 0x0403;
const NONE: u32 = u32::MAX;

/// A node of [`Library`]'s tree of what events play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// An encoded clip and where its bytes are. [`Library::read`] fetches it,
/// [`decode`] turns it into samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Media {
    /// Wwise media id, the `.wem`'s file name.
    pub id: u32,
    at: At,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum At {
    /// `media/<id>.wem`, or `media/<language>/<id>.wem` for voice.
    Loose { language: bool },
    /// `size` bytes at `offset` in bank file `bank` (inside its DATA chunk).
    Bank { bank: u16, offset: u32, size: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Node<'a> {
    Media(Media),
    /// A sound whose media neither a bank nor a loose file has, by media id.
    Missing(u32),
    /// A source plugin that makes its own signal, by plugin id
    /// (0x00650002 is Wwise Silence): nothing to decode.
    Generated(u32),
    /// Plays one child, picked at random. Wwise weights the picks; the
    /// weights are not kept.
    Random(&'a [NodeId]),
    /// Plays its children in playlist order: the next one each time it
    /// plays, or all of them back to back when `continuous`.
    Sequence {
        children: &'a [NodeId],
        continuous: bool,
    },
    /// Plays the child the current value of a switch or state group picks.
    /// Which group, and its default, are not read.
    Switch(&'a [NodeId]),
    /// Plays all children at once.
    Layer(&'a [NodeId]),
}

/// An `Audio` asset and what its event plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event<'a> {
    pub guid: u32,
    /// `WwiseStandard/WwiseID`: FNV-1 of the event's lowercased name.
    pub wwise_id: u32,
    /// `Audio/MaxAttenuation`: the distance past which the event is not
    /// heard, in the attenuation curve's units (10 for a resident's broom).
    /// 0 for a sound without position.
    pub max_attenuation: f32,
    /// `Audio/IsLooping`: the event plays until something stops it (its
    /// `StopEvent`, or the sequence that started it).
    pub looping: bool,
    /// What the event's Play actions start, together. Empty when no loaded
    /// bank has the event or it plays nothing (a stop or state event).
    pub roots: &'a [NodeId],
}

/// Every `Audio` asset of an install resolved down to media, for one
/// language. Flat: nodes, their children and media are arrays, a node's
/// children always come before it.
#[derive(Debug)]
pub struct Library {
    game: &'static Game,
    language: String,
    /// Bank paths, for [`At::Bank`].
    banks: Vec<String>,
    /// Sorted by GUID.
    assets: Vec<AssetRow>,
    /// Every asset's `Name`, end to end; a row holds where its own is.
    names: String,
    roots: Vec<NodeId>,
    nodes: Vec<Rec>,
    children: Vec<NodeId>,
    media: Vec<Media>,
}

#[derive(Debug, Clone, Copy)]
struct AssetRow {
    guid: u32,
    wwise_id: u32,
    /// Its `Name` in [`Library::names`].
    name: u32,
    name_len: u16,
    max_attenuation: f32,
    looping: bool,
    roots: u32,
    root_count: u16,
    /// A loaded bank has the event.
    found: bool,
}

/// A node: `at` is the first child in `children`, the index in `media`,
/// the missing media id or the plugin id, by `kind`.
#[derive(Debug, Clone, Copy)]
struct Rec {
    kind: Kind,
    len: u16,
    at: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Media,
    Missing,
    Generated,
    Random,
    Sequence,
    Continuous,
    Switch,
    Layer,
}

impl Library {
    /// Reads the `Audio` assets and every bank (the root folder's, then
    /// `language`'s, which wins for a bank both ship) and resolves each
    /// asset, for 117 or 1800, whichever `install` is. 0.2 s for 117, 0.5 s
    /// for 1800 in a release build; build it once per install, off the UI
    /// thread. `language` is a folder
    /// name: `en_us`, `de_de`, `fr_fr`, and `zh_cn` in 117, `ru_ru` in 1800.
    pub fn load(install: &Install, language: &str) -> Result<Library> {
        let game = (GAMES.iter())
            .find(|g| install.exists(g.assets))
            .context("neither 117's nor 1800's sound assets are in the install")?;
        let xml = install.read(game.assets)?;
        let mut names = String::new();
        let mut assets = audio_assets(&xml, &mut names).context(game.assets)?;
        drop(xml); // 1800's assets.xml is 289 MB

        let is_bank = |p: &String| p.ends_with(".bnk");
        let local: Vec<String> = (install.list(&format!("{SOUND_DIR}/{language}")).into_iter())
            .filter(is_bank)
            .collect();
        let name = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
        let mut paths: Vec<String> = (install.list(SOUND_DIR).into_iter())
            .filter(|p| is_bank(p) && !local.iter().any(|l| name(l) == name(p)))
            .collect();
        paths.extend(local);
        ensure!(paths.len() < u16::MAX as usize, "too many banks");

        let mut read = Vec::new();
        let mut banks = Vec::new();
        for path in paths {
            if let Some(bank) =
                read_bank(install, &path, game.version).with_context(|| path.clone())?
            {
                read.push(bank);
                banks.push(path);
            }
        }
        ensure!(
            !banks.is_empty(),
            "no bank of version {} in {SOUND_DIR}",
            game.version
        );
        let mut objs = Vec::new();
        let mut didx = Vec::new();
        for (b, (bank, path)) in read.iter().zip(&banks).enumerate() {
            hirc_objects(&bank.hirc, &mut objs).with_context(|| path.clone())?;
            didx.extend(bank.media.iter().map(|&(id, offset, size)| Embedded {
                id,
                bank: b as u16,
                offset,
                size,
            }));
        }
        // One copy per id, the last bank's: a language bank after the root ones.
        objs.sort_by_key(|o: &Obj| o.id);
        let mut w = 0;
        for r in 0..objs.len() {
            if w > 0 && objs[w - 1].id == objs[r].id {
                objs[w - 1] = objs[r];
            } else {
                objs[w] = objs[r];
                w += 1;
            }
        }
        objs.truncate(w);
        didx.sort_by_key(|d: &Embedded| d.id);

        let mut lib = Library {
            game,
            language: language.to_owned(),
            banks,
            assets: Vec::new(),
            names,
            roots: Vec::new(),
            nodes: Vec::new(),
            children: Vec::new(),
            media: Vec::new(),
        };
        let mut build = Build::new(&objs, &didx, install, &mut lib);
        for a in &mut assets {
            let first = build.lib.roots.len();
            a.roots = first as u32;
            let Some(event) = build.find(a.wwise_id, EVENT) else {
                continue;
            };
            a.found = true;
            for action in event_actions(game, objs[event].body).unwrap_or_default() {
                let Some(i) = build.find(action, ACTION) else {
                    continue;
                };
                let body = objs[i].body;
                if u16_at(body, 4) != Some(PLAY) {
                    continue;
                }
                let target = u32_at(body, 6).and_then(|t| build.index(t));
                if let Some(node) = target.and_then(|t| build.intern(t, 0)) {
                    build.lib.roots.push(node);
                }
            }
            a.root_count = (build.lib.roots.len() - first) as u16;
        }
        lib.assets = assets;
        Ok(lib)
    }

    /// The `Audio` asset `guid`, or `None` when it is not one.
    pub fn event(&self, guid: u32) -> Option<Event<'_>> {
        let a = self.asset(guid)?;
        Some(Event {
            guid,
            wwise_id: a.wwise_id,
            max_attenuation: a.max_attenuation,
            looping: a.looping,
            roots: &self.roots[a.roots as usize..][..a.root_count as usize],
        })
    }

    pub fn node(&self, id: NodeId) -> Node<'_> {
        let r = self.nodes[id.0 as usize];
        let kids = || &self.children[r.at as usize..][..r.len as usize];
        match r.kind {
            Kind::Media => Node::Media(self.media[r.at as usize]),
            Kind::Missing => Node::Missing(r.at),
            Kind::Generated => Node::Generated(r.at),
            Kind::Random => Node::Random(kids()),
            Kind::Sequence | Kind::Continuous => Node::Sequence {
                children: kids(),
                continuous: r.kind == Kind::Continuous,
            },
            Kind::Switch => Node::Switch(kids()),
            Kind::Layer => Node::Layer(kids()),
        }
    }

    /// One playthrough of `guid`: the media that start together, appended to
    /// `out`. `roll(n)` picks for random and switch containers, an index
    /// below `n`. A sequence plays its first child: the step a fresh
    /// emitter takes; continuous sequences are cut to it too.
    pub fn pick(&self, guid: u32, mut roll: impl FnMut(usize) -> usize, out: &mut Vec<Media>) {
        let Some(e) = self.event(guid) else { return };
        for &root in e.roots {
            self.walk(root, &mut roll, out);
        }
    }

    // Children precede their parent in `nodes`, so this ends.
    fn walk(&self, id: NodeId, roll: &mut impl FnMut(usize) -> usize, out: &mut Vec<Media>) {
        match self.node(id) {
            Node::Media(m) => out.push(m),
            Node::Missing(_) | Node::Generated(_) => {}
            Node::Random(c) | Node::Switch(c) => {
                if !c.is_empty() {
                    self.walk(c[roll(c.len()).min(c.len() - 1)], roll, out);
                }
            }
            Node::Sequence { children, .. } => {
                if let Some(&first) = children.first() {
                    self.walk(first, roll, out);
                }
            }
            Node::Layer(c) => {
                for &c in c {
                    self.walk(c, roll, out);
                }
            }
        }
    }

    /// The `.wem` bytes of `media`.
    pub fn read(&self, install: &Install, media: Media) -> Result<Vec<u8>> {
        match media.at {
            At::Loose { language } => install.read(self.loose_path(language, media.id)),
            At::Bank { bank, offset, size } => {
                let path = &self.banks[bank as usize];
                let bytes = install.read_range(path, offset.into(), size.into())?;
                ensure!(
                    bytes.len() == size as usize,
                    "{path}: media {} cut short",
                    media.id
                );
                Ok(bytes)
            }
        }
    }

    fn loose_path(&self, language: bool, id: u32) -> String {
        let media = self.game.media;
        match language {
            true => format!("{SOUND_DIR}/{media}{}/{id}.wem", self.language),
            false => format!("{SOUND_DIR}/{media}{id}.wem"),
        }
    }

    fn asset(&self, guid: u32) -> Option<&AssetRow> {
        let i = self.assets.binary_search_by_key(&guid, |a| a.guid).ok()?;
        Some(&self.assets[i])
    }

    fn name_of(&self, a: &AssetRow) -> &str {
        &self.names[a.name as usize..][..a.name_len as usize]
    }

    /// The `Audio` asset's `Name`: `evt_fba_resident_sweeping` for the broom.
    pub fn name(&self, guid: u32) -> Option<&str> {
        self.asset(guid).map(|a| self.name_of(a))
    }

    /// The asset named `name`. Names are unique: 117's 16,448, 1800's 22,600.
    pub fn guid_of(&self, name: &str) -> Option<u32> {
        (self.assets.iter())
            .find(|a| self.name_of(a) == name)
            .map(|a| a.guid)
    }

    /// The assets whose event starts something, by GUID with their names:
    /// what a sound clip can play. A stop or state event plays nothing.
    pub fn sounds(&self) -> impl Iterator<Item = (u32, &str)> {
        (self.assets.iter())
            .filter(|a| a.root_count > 0)
            .map(|a| (a.guid, self.name_of(a)))
    }
}

/// `Audio` assets by GUID, their names appended to `names`. Each is a flat
/// `<Asset>` with its own `<Template>` (no `BaseAssetGUID`), in 117's
/// audio_generated.xml and in 1800's assets.xml alike, so a text scan is
/// enough.
fn audio_assets(xml: &[u8], names: &mut String) -> Result<Vec<AssetRow>> {
    let text = std::str::from_utf8(xml).context("not UTF-8")?;
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("<Asset>") {
        let body = &rest[at + "<Asset>".len()..];
        let end = body.find("</Asset>").context("unclosed <Asset>")?;
        let asset = &body[..end];
        rest = &body[end..];
        if crate::xml::tag(asset, "Template") != Some("Audio") {
            continue;
        }
        let number = |block: &str, name: &str| {
            crate::xml::block(asset, block)
                .and_then(|b| crate::xml::tag(b, name))
                .and_then(|v| v.parse::<u32>().ok())
        };
        let (Some(guid), Some(wwise_id)) = (
            number("Standard", "GUID"),
            number("WwiseStandard", "WwiseID"),
        ) else {
            continue;
        };
        let max_attenuation = (crate::xml::tag(asset, "MaxAttenuation"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.);
        let looping = crate::xml::tag(asset, "IsLooping") == Some("1");
        let name = (crate::xml::block(asset, "Standard"))
            .and_then(|b| crate::xml::tag(b, "Name"))
            .unwrap_or("");
        // Longest shipped: 108 bytes.
        let name = &name[..name.floor_char_boundary(u16::MAX as usize)];
        let at = names.len() as u32;
        names.push_str(name);
        out.push(AssetRow {
            guid,
            wwise_id,
            name: at,
            name_len: name.len() as u16,
            max_attenuation,
            looping,
            roots: 0,
            root_count: 0,
            found: false,
        });
    }
    out.sort_by_key(|a| a.guid);
    out.dedup_by_key(|a| a.guid);
    Ok(out)
}

/// A HIRC object of a loaded bank: `body` starts at its id.
#[derive(Debug, Clone, Copy)]
struct Obj<'a> {
    id: u32,
    kind: u8,
    body: &'a [u8],
}

/// A DIDX entry: media `id` is `size` bytes at `offset` in bank `bank`.
#[derive(Debug, Clone, Copy)]
struct Embedded {
    id: u32,
    bank: u16,
    offset: u32,
    size: u32,
}

/// What [`Library::load`] keeps of a bank file: its HIRC chunk, and per
/// embedded media its id, file offset and size.
struct Bank {
    hirc: Vec<u8>,
    media: Vec<(u32, u32, u32)>,
}

/// Reads a bank's chunks but DATA, which stays in the file and is read per
/// media: in 1800 it is nearly all of the 535 MB of banks. Chunks: BKHD
/// (version, bank id), DIDX (12-byte entries), DATA, HIRC, and in the Init
/// bank INIT, STMG, ENVS, PLAT, which are skipped. `None` for a bank of
/// another version than `version`.
fn read_bank(install: &Install, path: &str, version: u32) -> Result<Option<Bank>> {
    let (mut at, mut hirc, mut didx, mut data) = (0u64, Vec::new(), None, None);
    loop {
        let head = install.read_range(path, at, 8)?;
        if head.len() < 8 {
            break;
        }
        let len = u64::from(u32_at(&head, 4).unwrap());
        let tag: [u8; 4] = head[..4].try_into().unwrap();
        if matches!(&tag, b"BKHD" | b"DIDX" | b"HIRC") {
            let body = install.read_range(path, at + 8, len)?;
            ensure!(body.len() as u64 == len, "chunk past the end");
            match &tag {
                b"BKHD" => {
                    if u32_at(&body, 0).context("short BKHD")? != version {
                        return Ok(None);
                    }
                }
                b"DIDX" => didx = Some(body),
                _ => hirc = body,
            }
        } else if &tag == b"DATA" {
            data = Some(at + 8);
        }
        at += 8 + len;
    }
    let mut media = Vec::new();
    if let Some(didx) = didx {
        let data = data.context("DIDX without DATA")?;
        for e in didx.as_chunks::<12>().0 {
            let (id, offset, size) = (u32_at(e, 0), u32_at(e, 4), u32_at(e, 8));
            media.push((
                id.unwrap(),
                (data + u64::from(offset.unwrap())) as u32,
                size.unwrap(),
            ));
        }
    }
    Ok(Some(Bank { hirc, media }))
}

/// A HIRC chunk's objects: u32 count, then per object u8 type, u32 size,
/// body.
fn hirc_objects<'a>(hirc: &'a [u8], objs: &mut Vec<Obj<'a>>) -> Result<()> {
    let Some(count) = u32_at(hirc, 0) else {
        return Ok(()); // a bank without objects
    };
    let mut o = 4;
    for _ in 0..count {
        let kind = *hirc.get(o).context("HIRC cut short")?;
        let size = u32_at(hirc, o + 1).context("HIRC cut short")? as usize;
        let obj = hirc
            .get(o + 5..o + 5 + size)
            .context("HIRC object past the end")?;
        let id = u32_at(obj, 0).context("HIRC object without an id")?;
        objs.push(Obj {
            id,
            kind,
            body: obj,
        });
        o += 5 + size;
    }
    Ok(())
}

/// The action ids of an event: u32 id, in 117 7 bytes (zero in all but 21
/// of the 16,446 events; not decoded), a 7-bit varint count, the ids.
fn event_actions(game: &Game, body: &[u8]) -> Option<Vec<u32>> {
    let mut o = game.event_actions;
    let mut count = 0usize;
    loop {
        let b = *body.get(o)?;
        o += 1;
        count = count << 7 | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    if body.len() != o + 4 * count {
        return None;
    }
    Some(
        (0..count)
            .map(|i| u32_at(body, o + 4 * i).unwrap())
            .collect(),
    )
}

/// Where a sound's or container's NodeBaseParams starts: after the id for
/// containers; for a sound after its source too (u32 plugin, u8 stream
/// type, u32 media id, in 117 a u32 cache id, u32 in-memory size, u8
/// source bits, and a source plugin's u32-sized parameter block).
fn node_params(game: &Game, kind: u8, body: &[u8]) -> Option<usize> {
    match kind {
        SOUND => {
            let plugin = u32_at(body, 4)?;
            if plugin & 0xF == 2 {
                Some(game.source + 4 + u32_at(body, game.source)? as usize)
            } else {
                Some(game.source)
            }
        }
        RANDOM_SEQUENCE | SWITCH | ACTOR_MIXER | LAYER => Some(4),
        _ => None,
    }
}

/// DirectParentID of a sound or container. NodeBaseParams opens with the
/// FX list (u8 override, u8 count, then a u8 bypass mask and an entry per
/// effect), [`Game::after_fx`] bytes, u32 output bus and u32 parent. In
/// 117 those bytes are zero in every node (wwiser: metadata override and
/// count).
fn parent(game: &Game, kind: u8, body: &[u8]) -> Option<u32> {
    let mut o = node_params(game, kind, body)?;
    let fx = *body.get(o + 1)? as usize;
    o += 2;
    if fx > 0 {
        o += 1 + game.fx * fx;
    }
    if game.after_fx == 2 && *body.get(o + 1)? != 0 {
        return None; // metadata entries: their size is unknown
    }
    u32_at(body, o + game.after_fx + 4)
}

/// Turns HIRC objects into [`Library`]'s node arrays, one asset at a time.
struct Build<'a, 'b> {
    objs: &'b [Obj<'a>],
    didx: &'b [Embedded],
    install: &'b Install,
    lib: &'b mut Library,
    /// Child objects of object `i`: `kids[first[i]..first[i + 1]]`, from the
    /// parent links (a container's own list is read only for order).
    first: Vec<u32>,
    kids: Vec<u32>,
    /// Node of each object once interned, else NONE.
    memo: Vec<u32>,
}

impl<'a, 'b> Build<'a, 'b> {
    fn new(
        objs: &'b [Obj<'a>],
        didx: &'b [Embedded],
        install: &'b Install,
        lib: &'b mut Library,
    ) -> Self {
        let index = |id: u32| objs.binary_search_by_key(&id, |o| o.id).ok();
        let parents: Vec<u32> = (objs.iter())
            .map(|o| {
                (parent(lib.game, o.kind, o.body).filter(|&p| p != 0))
                    .and_then(index)
                    .map_or(NONE, |p| p as u32)
            })
            .collect();
        let mut first = vec![0u32; objs.len() + 1];
        for &p in parents.iter().filter(|&&p| p != NONE) {
            first[p as usize + 1] += 1;
        }
        for i in 1..first.len() {
            first[i] += first[i - 1];
        }
        let mut fill = first.clone();
        let mut kids = vec![0u32; *first.last().unwrap() as usize];
        for (i, &p) in parents.iter().enumerate().filter(|(_, p)| **p != NONE) {
            kids[fill[p as usize] as usize] = i as u32;
            fill[p as usize] += 1;
        }
        Build {
            objs,
            didx,
            install,
            lib,
            first,
            kids,
            memo: vec![NONE; objs.len()],
        }
    }

    fn index(&self, id: u32) -> Option<usize> {
        self.objs.binary_search_by_key(&id, |o| o.id).ok()
    }

    fn find(&self, id: u32, kind: u8) -> Option<usize> {
        self.index(id).filter(|&i| self.objs[i].kind == kind)
    }

    /// The node for object `i`, its subtree first. `None` for what plays
    /// nothing here: music, actor-mixers, unreadable sounds.
    fn intern(&mut self, i: usize, depth: u32) -> Option<NodeId> {
        if self.memo[i] != NONE {
            return Some(NodeId(self.memo[i]));
        }
        // Wwise trees are a few levels deep; this only stops a cycle.
        if depth > 32 {
            return None;
        }
        let o = self.objs[i];
        let rec = match o.kind {
            SOUND => self.sound(o.body)?,
            RANDOM_SEQUENCE | SWITCH | LAYER => {
                let own = &self.kids[self.first[i] as usize..self.first[i + 1] as usize];
                let (kind, order) = match o.kind {
                    RANDOM_SEQUENCE => self.playlist(o.body, own),
                    SWITCH => (Kind::Switch, own.to_vec()),
                    _ => (Kind::Layer, own.to_vec()),
                };
                let mut ids = Vec::with_capacity(order.len());
                for c in order {
                    if let Some(id) = self.intern(c as usize, depth + 1) {
                        ids.push(id);
                    }
                }
                let at = self.lib.children.len() as u32;
                self.lib.children.extend_from_slice(&ids);
                Rec {
                    kind,
                    len: ids.len().try_into().ok()?,
                    at,
                }
            }
            _ => return None,
        };
        let id = self.lib.nodes.len() as u32;
        self.lib.nodes.push(rec);
        self.memo[i] = id;
        Some(NodeId(id))
    }

    /// A sound's source. Stream type 0 is in memory: a bank's DATA, or a
    /// loose file the prepared-events bank loads. 1 (prefetch: the bank has
    /// only the head) and 2 (streamed) are whole only as loose files.
    fn sound(&mut self, body: &[u8]) -> Option<Rec> {
        let plugin = u32_at(body, 4)?;
        let stream = *body.get(8)?;
        let id = u32_at(body, 9)?;
        let language = body.get(self.lib.game.source - 1)? & 1 != 0;
        match plugin & 0xF {
            1 => {}
            2 => {
                return Some(Rec {
                    kind: Kind::Generated,
                    len: 0,
                    at: plugin,
                });
            }
            _ => return None,
        }
        let embedded = (stream == 0)
            .then(|| self.didx.binary_search_by_key(&id, |d| d.id).ok())
            .flatten()
            .map(|d| self.didx[d]);
        let at = match embedded {
            Some(e) => At::Bank {
                bank: e.bank,
                offset: e.offset,
                size: e.size,
            },
            None if self.install.exists(&self.lib.loose_path(language, id)) => {
                At::Loose { language }
            }
            None => {
                return Some(Rec {
                    kind: Kind::Missing,
                    len: 0,
                    at: id,
                });
            }
        };
        let at_media = self.lib.media.len() as u32;
        self.lib.media.push(Media { id, at });
        Some(Rec {
            kind: Kind::Media,
            len: 0,
            at: at_media,
        })
    }

    /// Kind and play order of a random/sequence container, read from the
    /// object's end, past NodeBaseParams' variable-length parts: ...,
    /// u8 transition mode, u8 random mode, u8 mode (0 random, 1 sequence),
    /// u8 flags (0x08 continuous), u32 child count and ids, u16 playlist
    /// count and (u32 id, i32 weight) entries. The child list must match
    /// the parent links, which fixes where it starts; on a mismatch (1 of
    /// 8,970) the container counts as random over its linked children.
    fn playlist(&self, body: &[u8], own: &[u32]) -> (Kind, Vec<u32>) {
        let ids = |list: &[u32]| {
            let mut v: Vec<u32> = list.iter().map(|&k| self.objs[k as usize].id).collect();
            v.sort_unstable();
            v
        };
        let linked = ids(own);
        let m = own.len();
        for n in 0..=(body.len().saturating_sub(4) / 8) {
            let Some(pl) = body.len().checked_sub(2 + 8 * n) else {
                break;
            };
            if u16_at(body, pl) != Some(n as u16) {
                continue;
            }
            let Some(list) = pl.checked_sub(4 + 4 * m).filter(|&l| l >= 6) else {
                continue;
            };
            if u32_at(body, list) != Some(m as u32) {
                continue;
            }
            let mut listed: Vec<u32> = (0..m)
                .map(|k| u32_at(body, list + 4 + 4 * k).unwrap())
                .collect();
            listed.sort_unstable();
            if listed != linked {
                continue;
            }
            let (mode, flags) = (body[list - 2], body[list - 1]);
            let kind = match (mode, flags & 0x08 != 0) {
                (1, false) => Kind::Sequence,
                (1, true) => Kind::Continuous,
                _ => Kind::Random,
            };
            let order = (0..n)
                .filter_map(|k| self.index(u32_at(body, pl + 2 + 8 * k).unwrap()))
                .map(|k| k as u32)
                .filter(|k| own.contains(k))
                .collect::<Vec<_>>();
            return (
                kind,
                if order.is_empty() {
                    own.to_vec()
                } else {
                    order
                },
            );
        }
        (Kind::Random, own.to_vec())
    }
}

/// What [`decode`] appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
    /// First-order ambisonics rather than speakers (117's 4-channel
    /// ambiences). Channel 0 is W, the omni part: alone it is a fair mono mix.
    pub ambisonic: bool,
    /// Samples per channel appended.
    pub frames: usize,
}

/// Decodes a `.wem` to interleaved f32 in [-1, 1], appended to `out`.
/// Wwise Vorbis is the only codec 117 ships; 1800 also ships 16-bit PCM.
/// Anything else is an error.
pub fn decode(wem: &[u8], out: &mut Vec<f32>) -> Result<Format> {
    ensure!(
        wem.get(..4) == Some(b"RIFF") && wem.get(8..12) == Some(b"WAVE"),
        "not a RIFF WAVE"
    );
    // Chunks end at the RIFF size: one shipped file (111716566) has 34
    // stray bytes after it.
    let end = (u32_at(wem, 4).unwrap() as usize + 8).min(wem.len());
    let (mut fmt, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= end {
        let len = u32_at(wem, at + 4).unwrap() as usize;
        let body = wem
            .get(at + 8..at + 8 + len)
            .context("RIFF chunk past the end")?;
        match &wem[at..at + 4] {
            b"fmt " => fmt = Some(body),
            b"data" => data = Some(body),
            _ => {}
        }
        at += 8 + len;
    }
    let (fmt, data) = (fmt.context("no fmt chunk")?, data.context("no data chunk")?);
    let tag = u16_at(fmt, 0).context("short fmt")?;
    match tag {
        0xFFFF => decode_vorbis(fmt, data, out),
        0xFFFE | 0x0001 => decode_pcm(fmt, data, out),
        _ => anyhow::bail!("codec {tag:#06x}: only Wwise Vorbis (0xffff) and PCM are decoded"),
    }
}

/// Wwise PCM: a 24-byte fmt (tag 0xFFFE, then the WAVE fields, then the
/// Wwise channel config at 0x14) and interleaved 16-bit samples. 1800's
/// 167 embedded and 1,824 streamed voice sounds of codec 0x00010001.
fn decode_pcm(fmt: &[u8], data: &[u8], out: &mut Vec<f32>) -> Result<Format> {
    let channels = u16_at(fmt, 2).context("short fmt")?;
    let sample_rate = u32_at(fmt, 4).context("short fmt")?;
    let bits = u16_at(fmt, 14).context("short fmt")?;
    ensure!(bits == 16, "{bits}-bit PCM, only 16-bit is decoded");
    ensure!((1..=8).contains(&channels), "{channels} channels");
    let ambisonic = u32_at(fmt, 0x14).is_some_and(|c| (c >> 8) & 0xF == 2);
    let frames = data.len() / (2 * channels as usize);
    let samples = &data[..frames * 2 * channels as usize];
    out.extend(
        (samples.as_chunks::<2>().0.iter()).map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.),
    );
    Ok(Format {
        sample_rate,
        channels,
        ambisonic,
        frames,
    })
}

/// Wwise Vorbis: Vorbis packets with the Ogg framing, the identification
/// and comment headers dropped and the setup header packed (codebooks by
/// index into ww2ogg's aoTuV table, fixed fields left out). This rebuilds
/// standard header and audio packets, after ww2ogg, and decodes them with
/// lewton. Only the 0x42-byte fmt (vorb fields inside fmt) is read: every
/// 117 `.wem` has it.
fn decode_vorbis(fmt: &[u8], data: &[u8], out: &mut Vec<f32>) -> Result<Format> {
    use lewton::audio::{PreviousWindowRight, read_audio_packet_generic};
    use lewton::samples::InterleavedSamples;

    ensure!(
        fmt.len() == 0x42,
        "Wwise Vorbis fmt of {:#x} bytes, only 0x42 is read",
        fmt.len()
    );
    let u32f = |o| u32_at(fmt, o).unwrap();
    let channels = u16_at(fmt, 2).unwrap();
    let sample_rate = u32f(4);
    let avg_bytes = u32f(8);
    // Wwise channel config: u8 count, 4-bit type (2 = ambisonic), 20-bit mask.
    let ambisonic = (u32f(0x14) >> 8) & 0xF == 2;
    let samples = u32f(0x18) as usize;
    // ww2ogg: these four mean plain packets; any other (0xBC in 117)
    // means each packet's type bit and window flags were cut.
    let mod_packets = !matches!(u32f(0x1C), 0x4A | 0x4B | 0x69 | 0x70);
    let setup_at = u32f(0x28) as usize;
    let audio_at = u32f(0x2C) as usize;
    let (bs0, bs1) = (fmt[0x40], fmt[0x41]);
    ensure!((1..=8).contains(&channels), "{channels} channels");

    let mut ident = vec![1];
    ident.extend_from_slice(b"vorbis");
    ident.extend_from_slice(&0u32.to_le_bytes());
    ident.push(channels as u8);
    ident.extend_from_slice(&sample_rate.to_le_bytes());
    ident.extend_from_slice(&0u32.to_le_bytes());
    ident.extend_from_slice(&avg_bytes.wrapping_mul(8).to_le_bytes());
    ident.extend_from_slice(&0u32.to_le_bytes());
    ident.push(bs0 | bs1 << 4);
    ident.push(1);
    let ident = lewton::header::read_header_ident(&ident).map_err(|e| anyhow!("ident: {e}"))?;
    let (setup, long) = rebuild_setup(packet_at(data, setup_at)?, channels)?;
    let setup = lewton::header::read_header_setup(&setup, channels as u8, (bs0, bs1))
        .map_err(|e| anyhow!("setup: {e}"))?;
    let mode_bits = ilog(long.len() as u32 - 1);

    let start = out.len();
    let mut pwr = PreviousWindowRight::new();
    let mut prev_long = false;
    let mut rebuilt = Vec::new();
    let mut at = audio_at;
    while at < data.len() {
        let payload = packet_at(data, at).with_context(|| format!("packet at {at}"))?;
        let next = at + 2 + payload.len();
        at = next;
        if payload.is_empty() {
            continue;
        }
        let packet = if mod_packets {
            let next_first = packet_at(data, next).ok().and_then(|p| p.first().copied());
            rebuild_packet(
                payload,
                next_first,
                mode_bits,
                &long,
                &mut prev_long,
                &mut rebuilt,
            )?;
            &rebuilt[..]
        } else {
            payload
        };
        let s: InterleavedSamples<f32> =
            read_audio_packet_generic(&ident, &setup, packet, &mut pwr)
                .map_err(|e| anyhow!("audio packet at {}: {e}", next - 2 - payload.len()))?;
        out.extend_from_slice(&s.samples);
    }
    // The last packet decodes a block past the end; the header has the length.
    let channels_n = channels as usize;
    let frames = ((out.len() - start) / channels_n).min(samples);
    out.truncate(start + frames * channels_n);
    Ok(Format {
        sample_rate,
        channels,
        ambisonic,
        frames,
    })
}

/// A packet in `data`: u16 size, then the payload (117's files carry no
/// granule).
fn packet_at(data: &[u8], at: usize) -> Result<&[u8]> {
    let size = u16_at(data, at).context("packet header past the data")? as usize;
    data.get(at + 2..at + 2 + size)
        .context("packet past the data")
}

/// Puts back what Wwise cut from an audio packet: before the mode number a
/// 0 type bit, after it, for a long block, the previous and next blocks'
/// window flags. The payload's other bits follow shifted by 1 or 3.
fn rebuild_packet(
    payload: &[u8],
    next_first: Option<u8>,
    mode_bits: u32,
    long: &[bool],
    prev_long: &mut bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let mask = (1u32 << mode_bits) - 1;
    let mode = payload[0] as u32 & mask;
    let is_long = *long.get(mode as usize).context("mode out of range")?;
    let mut head = mode << 1;
    let mut head_bits = 1 + mode_bits;
    if is_long {
        let next_long = next_first
            .and_then(|b| long.get((b as u32 & mask) as usize).copied())
            .unwrap_or(false);
        head |= (*prev_long as u32) << head_bits | (next_long as u32) << (head_bits + 1);
        head_bits += 2;
    }
    *prev_long = is_long;
    let shift = head_bits - mode_bits;
    out.clear();
    let mut carry = 0u8;
    for &b in payload {
        out.push(b << shift | carry);
        carry = b >> (8 - shift);
    }
    out.push(carry);
    // The low `head_bits` (up to 9) now hold the shifted mode: replace them.
    let keep = !((1u16 << head_bits) - 1);
    let low = (u16::from_le_bytes([out[0], out[1]]) & keep) | head as u16;
    out[..2].copy_from_slice(&low.to_le_bytes());
    Ok(())
}

/// ww2ogg's packed codebooks: bodies, then u32 offsets, the last u32 the
/// offset of that table. 598 books.
static CODEBOOKS: &[u8] = include_bytes!("../data/audio/packed_codebooks_aoTuV_603.bin");

fn codebook(id: u32) -> Result<&'static [u8]> {
    let n = CODEBOOKS.len();
    let table = u32_at(CODEBOOKS, n - 4).unwrap() as usize;
    let count = (n - table) / 4;
    ensure!(
        (id as usize) < count - 1,
        "codebook {id} is not in the aoTuV table"
    );
    let at = |i: usize| u32_at(CODEBOOKS, table + 4 * i).unwrap() as usize;
    Ok(&CODEBOOKS[at(id as usize)..at(id as usize + 1)])
}

/// The standard Vorbis setup header for a Wwise one, and each mode's
/// block flag (true for long blocks), which audio packets need.
fn rebuild_setup(src: &[u8], channels: u16) -> Result<(Vec<u8>, Vec<bool>)> {
    let mut r = Bits::new(src);
    let mut w = BitWriter::default();
    w.put(5, 8);
    for &c in b"vorbis" {
        w.put(c as u32, 8);
    }
    let books = w.copy(&mut r, 8)? + 1;
    for _ in 0..books {
        let id = r.read(10)?;
        let mut cb = Bits::new(codebook(id)?);
        rebuild_codebook(&mut cb, &mut w).with_context(|| format!("codebook {id}"))?;
        ensure!(
            cb.at / 8 + 1 == cb.data.len(),
            "codebook {id}: size mismatch"
        );
    }
    // Time domain transforms: one placeholder, as the spec requires.
    w.put(0, 6);
    w.put(0, 16);

    let floors = w.copy(&mut r, 6)? + 1;
    for _ in 0..floors {
        w.put(1, 16); // floor type 1, the only one Wwise writes
        let partitions = w.copy(&mut r, 5)?;
        let mut classes = Vec::with_capacity(partitions as usize);
        for _ in 0..partitions {
            classes.push(w.copy(&mut r, 4)?);
        }
        let max_class = classes.iter().copied().max().map_or(0, |m| m + 1);
        let mut dims = Vec::with_capacity(max_class as usize);
        for _ in 0..max_class {
            dims.push(w.copy(&mut r, 3)? + 1);
            let subclasses = w.copy(&mut r, 2)?;
            if subclasses != 0 {
                ensure!(w.copy(&mut r, 8)? < books, "floor masterbook out of range");
            }
            for _ in 0..1 << subclasses {
                ensure!(
                    w.copy(&mut r, 8)? <= books,
                    "floor subclass book out of range"
                );
            }
        }
        w.copy(&mut r, 2)?; // multiplier
        let rangebits = w.copy(&mut r, 4)?;
        for &c in &classes {
            for _ in 0..dims[c as usize] {
                w.copy(&mut r, rangebits)?;
            }
        }
    }

    let residues = w.copy(&mut r, 6)? + 1;
    for _ in 0..residues {
        let kind = r.read(2)?;
        ensure!(kind <= 2, "residue type {kind}");
        w.put(kind, 16);
        w.copy(&mut r, 24)?; // begin
        w.copy(&mut r, 24)?; // end
        w.copy(&mut r, 24)?; // partition size
        let classifications = w.copy(&mut r, 6)? + 1;
        ensure!(w.copy(&mut r, 8)? < books, "residue classbook out of range");
        let mut cascade = Vec::with_capacity(classifications as usize);
        for _ in 0..classifications {
            let low = w.copy(&mut r, 3)?;
            let high = if w.copy(&mut r, 1)? == 1 {
                w.copy(&mut r, 5)?
            } else {
                0
            };
            cascade.push(high << 3 | low);
        }
        for c in cascade {
            for k in 0..8 {
                if c & 1 << k != 0 {
                    ensure!(w.copy(&mut r, 8)? < books, "residue book out of range");
                }
            }
        }
    }

    let mappings = w.copy(&mut r, 6)? + 1;
    let channel_bits = ilog(channels as u32 - 1);
    for _ in 0..mappings {
        w.put(0, 16); // mapping type 0
        let submaps = if w.copy(&mut r, 1)? == 1 {
            w.copy(&mut r, 4)? + 1
        } else {
            1
        };
        if w.copy(&mut r, 1)? == 1 {
            let steps = w.copy(&mut r, 8)? + 1;
            for _ in 0..steps {
                let magnitude = w.copy(&mut r, channel_bits)?;
                let angle = w.copy(&mut r, channel_bits)?;
                ensure!(
                    magnitude != angle && magnitude < channels as u32 && angle < channels as u32,
                    "bad channel coupling"
                );
            }
        }
        ensure!(w.copy(&mut r, 2)? == 0, "mapping reserved bits set");
        if submaps > 1 {
            for _ in 0..channels {
                ensure!(w.copy(&mut r, 4)? < submaps, "mapping mux out of range");
            }
        }
        for _ in 0..submaps {
            w.copy(&mut r, 8)?; // time config
            ensure!(w.copy(&mut r, 8)? < floors, "mapping floor out of range");
            ensure!(
                w.copy(&mut r, 8)? < residues,
                "mapping residue out of range"
            );
        }
    }

    let modes = w.copy(&mut r, 6)? + 1;
    let mut long = Vec::with_capacity(modes as usize);
    for _ in 0..modes {
        long.push(w.copy(&mut r, 1)? == 1);
        w.put(0, 16); // window type
        w.put(0, 16); // transform type
        ensure!(w.copy(&mut r, 8)? < mappings, "mode mapping out of range");
    }
    w.put(1, 1); // framing
    ensure!(
        r.at.div_ceil(8) == src.len(),
        "setup packet: read {} bits of {} bytes",
        r.at,
        src.len()
    );
    Ok((w.bytes, long))
}

/// One codebook from ww2ogg's packed form (4-bit dimensions, 14-bit
/// entries, 3-bit length width, 1-bit lookup type) to the spec's.
fn rebuild_codebook(r: &mut Bits, w: &mut BitWriter) -> Result<()> {
    let dims = r.read(4)?;
    let entries = r.read(14)?;
    w.put(0x564342, 24); // "BCV"
    w.put(dims, 16);
    w.put(entries, 24);
    if w.copy(r, 1)? == 1 {
        // Ordered: runs of entries per length.
        w.copy(r, 5)?;
        let mut current = 0;
        while current < entries {
            current += w.copy(r, ilog(entries - current))?;
        }
        ensure!(current == entries, "ordered lengths overrun");
    } else {
        let width = r.read(3)?;
        ensure!((1..=5).contains(&width), "codeword length width {width}");
        let sparse = w.copy(r, 1)? == 1;
        for _ in 0..entries {
            if !sparse || w.copy(r, 1)? == 1 {
                w.put(r.read(width)?, 5);
            }
        }
    }
    let lookup = r.read(1)?;
    w.put(lookup, 4);
    if lookup == 1 {
        w.copy(r, 32)?; // minimum
        w.copy(r, 32)?; // delta
        let value_bits = w.copy(r, 4)? + 1;
        w.copy(r, 1)?; // sequence flag
        for _ in 0..quantvals(entries, dims)? {
            w.copy(r, value_bits)?;
        }
    }
    Ok(())
}

/// The largest v with v^dims <= entries: a lookup-1 codebook's value count.
fn quantvals(entries: u32, dims: u32) -> Result<u32> {
    ensure!(
        entries > 0 && dims > 0,
        "empty codebook with a lookup table"
    );
    let pow = |v: u64| (0..dims).fold(1u64, |a, _| a.saturating_mul(v));
    let mut v = (entries as f64).powf(1. / dims as f64).floor() as u64;
    while pow(v) > entries as u64 {
        v -= 1;
    }
    while pow(v + 1) <= entries as u64 {
        v += 1;
    }
    Ok(v as u32)
}

fn ilog(v: u32) -> u32 {
    32 - v.leading_zeros()
}

/// Vorbis bit order: least significant bit of each byte first.
struct Bits<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, at: 0 }
    }

    fn read(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self.data.get(self.at / 8).context("read past the packet")?;
            v |= ((byte >> (self.at % 8)) as u32 & 1) << i;
            self.at += 1;
        }
        Ok(v)
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        for i in 0..n {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= ((v >> i & 1) as u8) << (self.bits % 8);
            self.bits += 1;
        }
    }

    /// Reads `n` bits and writes them unchanged.
    fn copy(&mut self, r: &mut Bits, n: u32) -> Result<u32> {
        let v = r.read(n)?;
        self.put(v, n);
        Ok(v)
    }
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Model, Play};
    use crate::vfs::FileId;
    use rayon::prelude::*;
    use std::collections::BTreeMap;

    fn install_of(var: &str) -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir(var) else {
            eprintln!("skip: {var} unset");
            return None;
        };
        Some(Install::open(dir, Install::DATA_FIRST).unwrap())
    }

    fn install() -> Option<Install> {
        install_of("ANNO117_DIR")
    }

    /// Every GUID a `<Sequences>` sound element names, with how many elements name it.
    fn sequence_guids(install: &Install) -> BTreeMap<u32, usize> {
        let cfgs: Vec<String> = (0..install.file_count())
            .map(|i| install.file_path(FileId(i as u32)))
            .filter(|p| p.ends_with(".cfg"))
            .collect();
        let found: Vec<Vec<u32>> = cfgs
            .par_iter()
            .map(|p| {
                let Ok(bytes) = install.read(p) else {
                    return Vec::new();
                };
                if memchr::memmem::find(&bytes, b"<GUID>").is_none() {
                    return Vec::new();
                }
                let Ok(m) = Model::parse(&bytes) else {
                    return Vec::new();
                };
                (m.sequences.iter().flat_map(|s| &s.clips))
                    .filter_map(|c| match c.what {
                        Play::Sound(s) => Some(s.guid),
                        _ => None,
                    })
                    .collect()
            })
            .collect();
        let mut out = BTreeMap::new();
        for g in found.into_iter().flatten() {
            *out.entry(g).or_default() += 1;
        }
        out
    }

    fn media_under(lib: &Library, id: NodeId, out: &mut Vec<Media>) {
        match lib.node(id) {
            Node::Media(m) => out.push(m),
            Node::Missing(_) | Node::Generated(_) => {}
            Node::Random(c) | Node::Switch(c) | Node::Layer(c) => {
                c.iter().for_each(|&c| media_under(lib, c, out))
            }
            Node::Sequence { children, .. } => {
                children.iter().for_each(|&c| media_under(lib, c, out))
            }
        }
    }

    fn peak(pcm: &[f32]) -> f32 {
        pcm.iter().fold(0f32, |m, s| m.max(s.abs()))
    }

    // A wem built by hand would need a Vorbis encoder; the codebook table's
    // own framing is checked instead: every book rebuilds to its exact size.
    #[test]
    fn codebook_table_rebuilds() {
        let n = CODEBOOKS.len();
        let count = (n - u32_at(CODEBOOKS, n - 4).unwrap() as usize) / 4 - 1;
        assert_eq!(count, 598);
        for id in 0..count as u32 {
            let mut r = Bits::new(codebook(id).unwrap());
            rebuild_codebook(&mut r, &mut BitWriter::default()).unwrap();
            assert_eq!(r.at / 8 + 1, r.data.len(), "codebook {id}");
        }
        assert!(codebook(598).is_err());
    }

    #[test]
    fn quantvals_match_libvorbis() {
        assert_eq!(quantvals(81, 4).unwrap(), 3);
        assert_eq!(quantvals(80, 4).unwrap(), 2);
        assert_eq!(quantvals(625, 4).unwrap(), 5);
        assert_eq!(quantvals(1, 2).unwrap(), 1);
        assert_eq!(quantvals(16383, 1).unwrap(), 16383);
    }

    #[test]
    fn rebuilt_packet_gets_type_bit_and_windows() {
        // Two modes, short and long: mode_bits 1. A long packet between a
        // short and a long one gets prev 0, next 1.
        let long = [false, true];
        let mut prev = false;
        let mut out = Vec::new();
        rebuild_packet(&[0b1010_1011], Some(0x01), 1, &long, &mut prev, &mut out).unwrap();
        // Out bits, LSB first: 0 type, 1 mode, 0 prev, 1 next, then the
        // input's bits 1..8 (1,0,1,0,1,0,1).
        let bits: Vec<u8> = (0..11).map(|i| out[i / 8] >> (i % 8) & 1).collect();
        assert_eq!(bits, [0, 1, 0, 1, 1, 0, 1, 0, 1, 0, 1]);
        assert!(prev);
        // A short packet: only the type bit is added.
        rebuild_packet(&[0b0000_0110], None, 1, &long, &mut prev, &mut out).unwrap();
        assert_eq!(out, [0b0000_1100, 0]);
        assert!(!prev);
    }

    // The resident's broom (51235, `evt_fba_resident_sweeping`): a random
    // pick of five mono 48 kHz clips of 256 to 353 ms, as the asset's
    // DurationMinimum/Maximum say.
    #[test]
    fn broom_resolves_and_decodes() {
        let Some(install) = install() else { return };
        let lib = Library::load(&install, "en_us").unwrap();
        let e = lib.event(51235).unwrap();
        assert_eq!(e.wwise_id, 3877899962);
        assert_eq!(lib.name(51235), Some("evt_fba_resident_sweeping"));
        assert_eq!(lib.guid_of("evt_fba_resident_sweeping"), Some(51235));
        assert_eq!((e.max_attenuation, e.looping), (10., false));
        assert_eq!(e.roots.len(), 1);
        let Node::Random(choices) = lib.node(e.roots[0]) else {
            panic!("{:?}", lib.node(e.roots[0]));
        };
        let mut ids: Vec<u32> = (choices.iter())
            .map(|&c| match lib.node(c) {
                Node::Media(m) => m.id,
                n => panic!("{n:?}"),
            })
            .collect();
        ids.sort();
        assert_eq!(ids, [5273307, 301521506, 486949864, 691922059, 1052406773]);
        let mut picked = Vec::new();
        lib.pick(51235, |n| n - 1, &mut picked);
        assert_eq!(picked.len(), 1);
        for &c in choices {
            let Node::Media(m) = lib.node(c) else {
                unreachable!()
            };
            let mut pcm = Vec::new();
            let f = decode(&lib.read(&install, m).unwrap(), &mut pcm).unwrap();
            assert_eq!((f.sample_rate, f.channels, f.ambisonic), (48000, 1, false));
            let ms = f.frames * 1000 / 48000;
            assert!((250..=360).contains(&ms), "{} lasts {ms} ms", m.id);
            assert!(
                peak(&pcm) > 0.05 && peak(&pcm) <= 1.0,
                "{} peak {}",
                m.id,
                peak(&pcm)
            );
        }
    }

    // 1800's train: a looping event heard to 30, an Audio asset of
    // assets.xml, and what it starts decodes.
    #[test]
    fn anno1800_train_loops() {
        let Some(install) = install_of("ANNO1800_DIR") else {
            return;
        };
        let lib = Library::load(&install, "en_us").unwrap();
        assert_eq!(lib.name(9818702), Some("wwe_sfx_veh_default_train"));
        let e = lib.event(9818702).unwrap();
        assert_eq!(
            (e.wwise_id, e.max_attenuation, e.looping),
            (984654681, 30., true)
        );
        let mut picked = Vec::new();
        lib.pick(9818702, |_| 0, &mut picked);
        assert!(!picked.is_empty());
        for m in picked {
            let mut pcm = Vec::new();
            let f = decode(&lib.read(&install, m).unwrap(), &mut pcm).unwrap();
            assert!(f.frames > 0 && peak(&pcm) > 0.01, "{} {f:?}", m.id);
        }
    }

    // Wwise PCM as 1800 ships it: a 24-byte fmt and 16-bit samples.
    #[test]
    fn pcm_decodes() {
        let mut fmt = Vec::new();
        for v in [0xFFFEu16, 2] {
            fmt.extend_from_slice(&v.to_le_bytes());
        }
        for v in [48000u32, 192_000] {
            fmt.extend_from_slice(&v.to_le_bytes());
        }
        for v in [4u16, 16, 6, 0] {
            fmt.extend_from_slice(&v.to_le_bytes());
        }
        fmt.extend_from_slice(&0x3102u32.to_le_bytes());
        let data: Vec<u8> = [16384i16, -32768, 0, 32767]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut body = b"WAVE".to_vec();
        for (tag, chunk) in [(b"fmt ", &fmt), (b"data", &data)] {
            body.extend_from_slice(tag);
            body.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            body.extend_from_slice(chunk);
        }
        let mut wem = b"RIFF".to_vec();
        wem.extend_from_slice(&(body.len() as u32).to_le_bytes());
        wem.extend_from_slice(&body);
        let mut pcm = Vec::new();
        let f = decode(&wem, &mut pcm).unwrap();
        assert_eq!(
            (f.sample_rate, f.channels, f.frames, f.ambisonic),
            (48000, 2, 2, false)
        );
        assert_eq!(pcm, [0.5, -1., 0., 32767. / 32768.]);
    }

    // What a sound clip can play: the fire's start event plays, its stop
    // event (36703) plays nothing and is left out.
    #[test]
    fn only_events_that_play_are_sounds() {
        let Some(install) = install() else { return };
        let lib = Library::load(&install, "en_us").unwrap();
        let sounds: Vec<(u32, &str)> = lib.sounds().collect();
        assert!(sounds.contains(&(36702, "evt_sfx_inc_fire_cl_play")));
        assert_eq!(lib.name(36703), Some("evt_sfx_inc_fire_cl_stop"));
        assert!(!sounds.iter().any(|&(g, _)| g == 36703));
        assert!(sounds.len() > 1000, "{} sounds", sounds.len());
    }

    // Every bank parses, every node reads, and of the GUIDs sequences name,
    // count how many reach media; then decode all their media.
    #[test]
    fn sequence_sounds_resolve_and_decode() {
        let Some(install) = install() else { return };
        resolve_and_decode(&install);
    }

    // 1800's banks are an older Wwise's: the same counts on them.
    #[test]
    fn anno1800_sequence_sounds_resolve_and_decode() {
        let Some(install) = install_of("ANNO1800_DIR") else {
            return;
        };
        resolve_and_decode(&install);
    }

    fn resolve_and_decode(install: &Install) {
        let t = std::time::Instant::now();
        let lib = Library::load(install, "en_us").unwrap();
        eprintln!(
            "library: {} banks, {} assets, {} nodes, {} media, {:.2?}",
            lib.banks.len(),
            lib.assets.len(),
            lib.nodes.len(),
            lib.media.len(),
            t.elapsed()
        );
        let found = lib.assets.iter().filter(|a| a.found).count();
        let playing = lib.assets.iter().filter(|a| a.root_count > 0).count();
        eprintln!("assets: {found} events in the banks, {playing} with a Play that resolves");
        assert!(lib.banks.len() >= 6);
        assert!(found > lib.assets.len() * 99 / 100);

        let guids = sequence_guids(install);
        let elements: usize = guids.values().sum();
        // Per outcome: GUIDs, elements, the first few GUIDs.
        let mut tally = BTreeMap::<&str, (usize, usize, Vec<u32>)>::new();
        let mut media = Vec::new();
        for (&g, &n) in &guids {
            let kind = match lib.event(g) {
                None => "not an Audio asset",
                Some(_) if !lib.asset(g).unwrap().found => "event in no bank",
                Some(e) if e.roots.is_empty() => "plays nothing",
                Some(e) => {
                    let before = media.len();
                    e.roots
                        .iter()
                        .for_each(|&r| media_under(&lib, r, &mut media));
                    if media.len() > before {
                        "reaches media"
                    } else {
                        "no media"
                    }
                }
            };
            let t = tally.entry(kind).or_default();
            t.0 += 1;
            t.1 += n;
            if t.2.len() < 10 {
                t.2.push(g);
            }
        }
        eprintln!(
            "sequence sounds: {} GUIDs in {elements} elements",
            guids.len()
        );
        for (k, (g, n, some)) in &tally {
            eprintln!("  {k}: {g} GUIDs, {n} elements, e.g. {some:?}");
        }
        assert!(guids.len() > 50 && elements > 2500);
        assert!(tally["reaches media"].0 * 2 > guids.len());

        media.sort_by_key(|m| m.id);
        media.dedup();
        let results: Vec<Result<(Format, f32)>> = media
            .par_iter()
            .map(|&m| {
                let mut pcm = Vec::new();
                let f = decode(&lib.read(install, m)?, &mut pcm)
                    .with_context(|| format!("media {}", m.id))?;
                Ok((f, peak(&pcm)))
            })
            .collect();
        let failed: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        let mut seconds = 0.;
        let mut shapes = BTreeMap::<(u32, u16, bool), usize>::new();
        for (f, _) in results.iter().flatten() {
            seconds += f.frames as f64 / f.sample_rate as f64;
            *shapes
                .entry((f.sample_rate, f.channels, f.ambisonic))
                .or_default() += 1;
        }
        let silent = results.iter().flatten().filter(|(_, p)| *p < 1e-4).count();
        eprintln!(
            "decoded {} of {} media, {seconds:.0} s, {silent} silent; (rate, channels, ambisonic): {shapes:?}",
            media.len() - failed.len(),
            media.len()
        );
        for e in failed.iter().take(5) {
            eprintln!("  {e:#}");
        }
        assert!(failed.is_empty());
    }

    // ANNO117_SWEEP=1: every `.wem` of the install and every bank-embedded
    // media, by fmt tag, decoded.
    #[test]
    fn sweep_all_media() {
        if std::env::var_os("ANNO117_SWEEP").is_none() {
            return;
        }
        let Some(install) = install() else { return };
        let lib = Library::load(&install, "en_us").unwrap();
        let loose: Vec<String> = (0..install.file_count())
            .map(|i| install.file_path(FileId(i as u32)))
            .filter(|p| p.starts_with(SOUND_DIR) && p.ends_with(".wem"))
            .collect();
        let files = lib
            .banks
            .iter()
            .map(|p| install.read(p).unwrap())
            .collect::<Vec<_>>();
        let mut embedded = Vec::new();
        for (b, path) in lib.banks.iter().enumerate() {
            let bank = read_bank(&install, path, lib.game.version)
                .unwrap()
                .unwrap();
            embedded.extend(bank.media.iter().map(|&(id, offset, size)| Embedded {
                id,
                bank: b as u16,
                offset,
                size,
            }));
        }
        let t = std::time::Instant::now();
        type Outcome = (u16, Result<f64, String>);
        let decode_one = |wem: &[u8]| -> Outcome {
            let tag = (wem.get(..4) == Some(b"RIFF"))
                .then(|| {
                    let at = memchr::memmem::find(wem, b"fmt ")?;
                    u16_at(wem, at + 8)
                })
                .flatten()
                .unwrap_or(0);
            let mut pcm = Vec::new();
            let r = decode(wem, &mut pcm)
                .map(|f| f.frames as f64 / f.sample_rate as f64)
                .map_err(|e| format!("{e:#}"));
            (tag, r)
        };
        let mut outcomes: Vec<(String, Outcome)> = loose
            .par_iter()
            .map(|p| (p.clone(), decode_one(&install.read(p).unwrap())))
            .collect();
        // A prefetched sound's bank holds only the head of the file that
        // streams in full from media/; those heads are not decodable alone.
        let loose_ids: std::collections::HashSet<u32> = (loose.iter())
            .filter_map(|p| p.rsplit('/').next()?.strip_suffix(".wem")?.parse().ok())
            .collect();
        let (heads, whole): (Vec<&Embedded>, Vec<&Embedded>) =
            embedded.iter().partition(|e| loose_ids.contains(&e.id));
        outcomes.par_extend(whole.par_iter().map(|e| {
            let f = &files[e.bank as usize];
            let wem = &f[e.offset as usize..][..e.size as usize];
            (
                format!("{} in {}", e.id, lib.banks[e.bank as usize]),
                decode_one(wem),
            )
        }));
        let mut by_tag = BTreeMap::<u16, (usize, usize)>::new();
        let mut seconds = 0.;
        for (_, (tag, r)) in &outcomes {
            let e = by_tag.entry(*tag).or_default();
            e.0 += 1;
            if let Ok(s) = r {
                e.1 += 1;
                seconds += s;
            }
        }
        eprintln!(
            "{} loose + {} embedded media ({} more are prefetch heads of loose files) in {:.1?}; {:.1} h decoded",
            loose.len(),
            whole.len(),
            heads.len(),
            t.elapsed(),
            seconds / 3600.
        );
        for (tag, (n, ok)) in &by_tag {
            eprintln!("  fmt {tag:#06x}: {n} files, {ok} decoded");
        }
        let failed: Vec<_> = outcomes.iter().filter(|(_, (_, r))| r.is_err()).collect();
        for (p, (_, r)) in failed.iter().take(10) {
            eprintln!("  {p}: {}", r.as_ref().unwrap_err());
        }
        // Undecodable: the convolution reverb's impulse responses (`PLUG`
        // containers, not RIFF), and one sword parry shipped broken: its
        // fourth packet claims 43,010 bytes of a 7,967-byte data chunk.
        let vorbis_failures: Vec<&str> = (failed.iter())
            .filter(|(_, (tag, _))| *tag == 0xFFFF)
            .map(|(p, _)| p.as_str())
            .collect();
        assert_eq!(
            vorbis_failures,
            [format!("{SOUND_DIR}/media/111716566.wem")]
        );
    }
}
