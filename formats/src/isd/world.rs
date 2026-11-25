// A world's (`.www`) island list: which island files it places, where and
// how turned, read in place from an `isd::Doc`, and rewritten as a new list
// on the same world; also its size and ark slots (start points). Everything
// else the world holds (areas, light profiles, player setup) stays as it
// was.
//
// Every shipped world (all 57, 2026-11-19 census) lists its islands in
// `WorldConfig/Islands` as `<k>` key and `<v>` value pairs, each value
// `hasValue` 1 and one `IslandConfig`. Keys mostly run 1..n but may skip
// (addonscenario01 has gaps), so they are ids, not positions, and a rewrite
// keeps them.

use anyhow::{Context, Result, bail, ensure};

use super::{Doc, NodeId, Value};

/// One island a world places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// `Filename`: the island's virtual path as the world writes it. 818
    /// shipped entries use backslashes, 680 forward slashes, and case varies.
    pub file: String,
    /// `Name`. In all 1,515 shipped entries it is the file's stem.
    pub name: String,
    /// The island's corner in the world, in metres (`PositionX`,
    /// `PositionZ`). Shipped values are multiples of 16; one is negative.
    pub x: i32,
    pub z: i32,
    /// Quarter turns, 0-3.
    pub direction: u8,
    /// The island's side in metres (`Width` and `Height`), equal to the
    /// island file's own `Width` in every shipped entry.
    pub side: u32,
    pub kind: Kind,
}

/// What a placed island is for (`Type`). Read from the shipped worlds, not
/// the engine: every shipped entry is one of these, and any other value
/// fails the read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Regular = 0,
    Underwater = 1,
    Start = 2,
    ThirdPartyUnderwater = 3,
    ThirdPartySurface = 4,
    Decoration = 5,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Regular,
        Kind::Underwater,
        Kind::Start,
        Kind::ThirdPartyUnderwater,
        Kind::ThirdPartySurface,
        Kind::Decoration,
    ];

    fn of_type(value: i64) -> Option<Kind> {
        Kind::ALL.into_iter().find(|&k| k as i64 == value)
    }
}

/// Children of an `IslandConfig` that describe the island it places: its
/// objects, native slots and auto-built settlement. A swapped-in island
/// gets them empty.
const OWN_CHILDREN: [&str; 3] = ["ObjectConfigs", "NativeSlotConfigs", "AutoBuildConfig"];

/// The islands `doc`, a world, places, in its order.
pub fn read(doc: &Doc) -> Result<Vec<Placed>> {
    entries(doc)?
        .iter()
        .map(|&(_, config)| read_entry(doc, config))
        .collect()
}

/// Rewrites `doc`'s island list to place `islands`, in order. Entry n keeps
/// its key and every child the list doesn't name (resources, fertilities,
/// light profile), so a list that changes one island changes only that
/// entry. An entry whose file changes loses its `OWN_CHILDREN` and
/// `MissionID`, because they belong to the island it held. Entries past the
/// document's own copy its last one, keyed after the largest key; fails on
/// a world that places none.
pub fn write(doc: &mut Doc, islands: &[Placed]) -> Result<()> {
    islands.iter().try_for_each(check)?;
    let list = list(doc)?;
    let had = entries(doc)?;
    // Trim from the end first: removing later nodes leaves earlier ids be.
    for &(key, config) in had.iter().skip(islands.len()).rev() {
        doc.remove(parent_v(doc, list, config)?);
        doc.remove(key);
    }
    if islands.len() > had.len() {
        let Some(&(last_key, last_config)) = had.last() else {
            bail!("the world places no island to model new entries on");
        };
        let last_v = parent_v(doc, list, last_config)?;
        let mut next = (had.iter())
            .map(|&(k, _)| number(doc, k))
            .collect::<Result<Vec<i64>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        // Appending at the list's end moves no id before it, so the two
        // sources stay valid across the loop.
        for _ in had.len()..islands.len() {
            next += 1;
            let key = doc.insert_copy(last_key, list, None)?;
            doc.set(key, next.to_string().as_bytes(), false)?;
            doc.insert_copy(last_v, list, None)?;
        }
    }
    // Backwards, so emptying one entry's children leaves the ids of the
    // entries still to do unmoved.
    let now = entries(doc)?;
    for (p, &(_, config)) in islands.iter().zip(&now).rev() {
        let was = text(doc, config, "Filename")?;
        if !was.eq_ignore_ascii_case(&p.file) {
            for name in OWN_CHILDREN {
                if let Some(c) = doc.child(config, name) {
                    clear(doc, c);
                }
            }
            put(doc, config, "MissionID", "0")?;
        }
        put_entry(doc, config, p)?;
    }
    Ok(())
}

/// Rewrites `doc`'s island list to place `islands`, each with the entry it
/// came from: `Some(n)`, entry n of the list as read, keeps that entry's key
/// and everything the list doesn't name, losing its `OWN_CHILDREN` and
/// `MissionID` only where its file changes, as [`write`]; `None` is a new
/// entry, a copy of the document's last one keyed after the largest key,
/// with those cleared. Entries no one names go. Unlike
/// [`write`], which goes by position, a removal in the middle leaves every
/// later entry's carried children with its island. Kept entries come in
/// their order, before every new one.
pub fn write_by_source(doc: &mut Doc, islands: &[(Option<usize>, Placed)]) -> Result<()> {
    islands.iter().try_for_each(|(_, p)| check(p))?;
    let had = entries(doc)?;
    let mut kept = vec![0u64; had.len().div_ceil(64)];
    let (mut last, mut new) = (None, 0);
    for (source, p) in islands {
        let Some(i) = *source else {
            new += 1;
            continue;
        };
        ensure!(i < had.len(), "{}: the world has no entry {i}", p.file);
        ensure!(
            new == 0 && last.is_none_or(|l| l < i),
            "{}: kept entries must come in their order, before new ones",
            p.file
        );
        kept[i / 64] |= 1 << (i % 64);
        last = Some(i);
    }
    let list = list(doc)?;
    if new > 0 {
        let Some(&(last_key, last_config)) = had.last() else {
            bail!("the world places no island to model new entries on");
        };
        let last_v = parent_v(doc, list, last_config)?;
        let mut next = (had.iter())
            .map(|&(k, _)| number(doc, k))
            .collect::<Result<Vec<i64>>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        // Appending at the list's end moves no id before it.
        for _ in 0..new {
            next += 1;
            let key = doc.insert_copy(last_key, list, None)?;
            doc.set(key, next.to_string().as_bytes(), false)?;
            doc.insert_copy(last_v, list, None)?;
        }
    }
    // Backwards, so each removal leaves the ids still to remove unmoved.
    for (i, &(key, config)) in had.iter().enumerate().rev() {
        if kept[i / 64] >> (i % 64) & 1 == 0 {
            doc.remove(parent_v(doc, list, config)?);
            doc.remove(key);
        }
    }
    let now = entries(doc)?;
    ensure!(now.len() == islands.len(), "the island list lost an entry");
    for ((source, p), &(_, config)) in islands.iter().zip(&now).rev() {
        if source.is_none() || !text(doc, config, "Filename")?.eq_ignore_ascii_case(&p.file) {
            for name in OWN_CHILDREN {
                if let Some(c) = doc.child(config, name) {
                    clear(doc, c);
                }
            }
            put(doc, config, "MissionID", "0")?;
        }
        put_entry(doc, config, p)?;
    }
    Ok(())
}

/// The world's `Width` and `Height`, metres.
pub fn size(doc: &Doc) -> Result<[u32; 2]> {
    let config = doc
        .root("WorldConfig")
        .context("not a world: no <WorldConfig>")?;
    let side = |name: &str| -> Result<u32> {
        let id = doc
            .child(config, name)
            .with_context(|| format!("world without <{name}>"))?;
        Ok(number(doc, id)?.try_into()?)
    };
    Ok([side("Width")?, side("Height")?])
}

/// The world's ark slots (`ArkSlots`, `x` and `y` per `<i>`): where players
/// start, in metres.
pub fn arks(doc: &Doc) -> Result<Vec<[i32; 2]>> {
    let Some(slots) = ark_list(doc)? else {
        return Ok(Vec::new());
    };
    doc.children(slots)
        .map(|i| {
            let at = |name: &str| -> Result<i32> {
                let id = doc
                    .child(i, name)
                    .with_context(|| format!("ark slot without <{name}>"))?;
                Ok(number(doc, id)?.try_into()?)
            };
            Ok([at("x")?, at("y")?])
        })
        .collect()
}

/// Rewrites the world's ark slots to `arks`: slot n keeps its node, more
/// copy the last one, fewer drop the last ones. Fails on a world without
/// `ArkSlots`, or one with none to copy when `arks` wants more.
pub fn write_arks(doc: &mut Doc, arks: &[[i32; 2]]) -> Result<()> {
    let slots = ark_list(doc)?.context("world without <ArkSlots>")?;
    let had: Vec<NodeId> = doc.children(slots).collect();
    for &i in had.iter().skip(arks.len()).rev() {
        doc.remove(i);
    }
    if arks.len() > had.len() {
        let &last = had.last().context("the world has no ark slot to copy")?;
        for _ in had.len()..arks.len() {
            doc.insert_copy(last, slots, None)?;
        }
    }
    let now: Vec<NodeId> = doc.children(slots).collect();
    for (&[x, y], &i) in arks.iter().zip(&now).rev() {
        put(doc, i, "x", &x.to_string())?;
        put(doc, i, "y", &y.to_string())?;
    }
    Ok(())
}

fn ark_list(doc: &Doc) -> Result<Option<NodeId>> {
    let config = doc
        .root("WorldConfig")
        .context("not a world: no <WorldConfig>")?;
    Ok(doc.child(config, "ArkSlots"))
}

fn check(p: &Placed) -> Result<()> {
    ensure!(
        p.direction < 4,
        "{}: direction {} is not 0-3",
        p.file,
        p.direction
    );
    for text in [&p.file, &p.name] {
        ensure!(!text.contains('<'), "{text}: `<` in a world value");
    }
    Ok(())
}

// The fields the list names, into entry `config`.
fn put_entry(doc: &mut Doc, config: NodeId, p: &Placed) -> Result<()> {
    put(doc, config, "Name", &p.name)?;
    put(doc, config, "Filename", &p.file)?;
    put(doc, config, "Direction", &p.direction.to_string())?;
    put(doc, config, "PositionX", &p.x.to_string())?;
    put(doc, config, "PositionZ", &p.z.to_string())?;
    put(doc, config, "Width", &p.side.to_string())?;
    put(doc, config, "Height", &p.side.to_string())?;
    put(doc, config, "Type", &(p.kind as u8).to_string())
}

fn list(doc: &Doc) -> Result<NodeId> {
    let config = doc
        .root("WorldConfig")
        .context("not a world: no <WorldConfig>")?;
    doc.child(config, "Islands")
        .context("world without <Islands>")
}

// (key, IslandConfig) per entry, checking the k/v layout as it goes.
fn entries(doc: &Doc) -> Result<Vec<(NodeId, NodeId)>> {
    let list = list(doc)?;
    let mut out = Vec::new();
    let mut nodes = doc.children(list);
    while let Some(k) = nodes.next() {
        ensure!(
            doc.name(k) == b"k",
            "<Islands> entry {} has no <k>",
            out.len()
        );
        let key = number(doc, k)?;
        let v = nodes
            .next()
            .with_context(|| format!("island key {key} has no <v>"))?;
        ensure!(doc.name(v) == b"v", "island key {key} has no <v>");
        ensure!(
            text(doc, v, "hasValue")? == "1",
            "island key {key} holds no value"
        );
        let config = doc
            .child(v, "IslandConfig")
            .with_context(|| format!("island key {key} has no <IslandConfig>"))?;
        out.push((k, config));
    }
    Ok(out)
}

fn parent_v(doc: &Doc, list: NodeId, config: NodeId) -> Result<NodeId> {
    doc.children(list)
        .find(|&v| doc.child(v, "IslandConfig") == Some(config))
        .context("island entry lost its <v>")
}

fn read_entry(doc: &Doc, config: NodeId) -> Result<Placed> {
    let file = text(doc, config, "Filename")?;
    let named = |e: anyhow::Error| e.context(file.clone());
    let num = |name: &str| -> Result<i64> {
        let id = doc
            .child(config, name)
            .with_context(|| format!("no <{name}>"))?;
        number(doc, id)
    };
    let side = num("Width").map_err(named)?;
    ensure!(
        num("Height").map_err(named)? == side,
        "{file}: placed with a Width unlike its Height"
    );
    let direction = num("Direction").map_err(named)?;
    ensure!((0..4).contains(&direction), "{file}: direction {direction}");
    Ok(Placed {
        name: text(doc, config, "Name").map_err(named)?,
        x: num("PositionX").map_err(named)?.try_into()?,
        z: num("PositionZ").map_err(named)?.try_into()?,
        direction: direction as u8,
        side: side
            .try_into()
            .map_err(|_| anyhow::anyhow!("{file}: side {side}"))?,
        kind: {
            let value = num("Type").map_err(named)?;
            Kind::of_type(value).with_context(|| format!("{file}: Type {value}"))?
        },
        file,
    })
}

fn text(doc: &Doc, parent: NodeId, name: &str) -> Result<String> {
    let id = doc
        .child(parent, name)
        .with_context(|| format!("no <{name}>"))?;
    match doc.value(id) {
        Value::Text(t) => Ok(String::from_utf8_lossy(t).into_owned()),
        Value::Empty => Ok(String::new()),
        _ => bail!("<{name}> holds no text"),
    }
}

fn number(doc: &Doc, id: NodeId) -> Result<i64> {
    let name = String::from_utf8_lossy(doc.name(id)).into_owned();
    match doc.value(id) {
        Value::Text(t) => std::str::from_utf8(t)?
            .parse()
            .with_context(|| format!("<{name}> is not a number")),
        _ => bail!("<{name}> holds no number"),
    }
}

// Sets a text child, leaving it untouched when it already holds `value`, so
// unchanged entries add nothing to the document's value store.
fn put(doc: &mut Doc, parent: NodeId, name: &str, value: &str) -> Result<()> {
    let id = doc
        .child(parent, name)
        .with_context(|| format!("no <{name}>"))?;
    let same = match doc.value(id) {
        Value::Text(t) => t == value.as_bytes(),
        Value::Empty => value.is_empty(),
        _ => false,
    };
    if !same {
        doc.set(id, value.as_bytes(), false)?;
    }
    Ok(())
}

// Removes every child of `id`, which then writes as `<Name></Name>`.
fn clear(doc: &mut Doc, id: NodeId) {
    while let Some(c) = doc.children(id).next() {
        doc.remove(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One entry in the shipped layout. `extra` goes after `LightProfile`.
    fn entry(key: u32, p: &Placed, mission: u32, extra: &str) -> String {
        format!(
            "<k>{key}</k>\r\n<v><hasValue>1</hasValue>\r\n<IslandConfig><Name>{}</Name>\r\n<Filename>{}</Filename>\r\n<Direction>{}</Direction>\r\n<PositionX>{}</PositionX>\r\n<PositionZ>{}</PositionZ>\r\n<Width>{}</Width>\r\n<Height>{}</Height>\r\n<Type>{}</Type>\r\n<MissionID>{mission}</MissionID>\r\n<m_Resources><i><ResourceType><ResourceType>Coal</ResourceType>\r\n</ResourceType>\r\n<Amount>5372000</Amount>\r\n</i>\r\n</m_Resources>\r\n<m_Fertility></m_Fertility>\r\n<LightProfile></LightProfile>\r\n{extra}<AutoBuildConfig></AutoBuildConfig>\r\n</IslandConfig>\r\n</v>\r\n",
            p.name, p.file, p.direction, p.x, p.z, p.side, p.side, p.kind as u8
        )
    }

    fn world(entries: &str) -> String {
        format!(
            "<WorldConfig><Width>1792</Width>\r\n<Height>1792</Height>\r\n<Areas></Areas>\r\n<ArkSlots><i><x>1030</x>\r\n<y>920</y>\r\n</i>\r\n</ArkSlots>\r\n<WorldLightProfiles><North></North>\r\n<South></South>\r\n<UnderwaterNorth></UnderwaterNorth>\r\n<UnderwaterSouth></UnderwaterSouth>\r\n</WorldLightProfiles>\r\n<Islands>{entries}</Islands>\r\n</WorldConfig>\r\n"
        )
    }

    fn placed(file: &str, x: i32, z: i32, direction: u8, side: u32, kind: Kind) -> Placed {
        let stem = file
            .rsplit(['/', '\\'])
            .next()
            .unwrap()
            .trim_end_matches(".isd");
        Placed {
            file: file.into(),
            name: stem.into(),
            x,
            z,
            direction,
            side,
            kind,
        }
    }

    fn shipped() -> [Placed; 2] {
        [
            placed(
                r"data\levels\Islands\normal\N_L07.isd",
                832,
                1376,
                0,
                256,
                Kind::Start,
            ),
            placed(
                "data/levels/Islands/underwater/U_S06.isd",
                -112,
                560,
                3,
                80,
                Kind::Underwater,
            ),
        ]
    }

    // Keys 1 and 4, as addonscenario01's gaps have them; the second entry
    // holds an object and a mission.
    fn sample() -> String {
        let [a, b] = shipped();
        let objects = "<ObjectConfigs><i><ID>7</ID>\r\n</i>\r\n</ObjectConfigs>\r\n";
        world(&(entry(1, &a, 0, "") + &entry(4, &b, 12, objects)))
    }

    #[test]
    fn reads_the_islands_a_world_places() {
        let doc = Doc::parse(sample().as_bytes()).unwrap();
        assert_eq!(read(&doc).unwrap(), shipped());
    }

    #[test]
    fn a_written_world_reads_back_the_islands_it_was_given() {
        let mut doc = Doc::parse(sample().as_bytes()).unwrap();
        let three = vec![
            placed(
                "data/levels/islands/normal/mine.isd",
                96,
                96,
                1,
                160,
                Kind::Regular,
            ),
            shipped()[0].clone(),
            placed(
                "addondata/levels/islands/normal/ours.isd",
                1200,
                400,
                2,
                208,
                Kind::Start,
            ),
        ];
        write(&mut doc, &three).unwrap();
        let bytes = doc.encode();
        let back = Doc::parse(&bytes).unwrap();
        assert_eq!(back.encode(), bytes);
        assert_eq!(read(&back).unwrap(), three);
        // The third entry copies the last one and takes the next key.
        let keys: Vec<i64> = (back.children(list(&back).unwrap()))
            .filter(|&n| back.name(n) == b"k")
            .map(|k| number(&back, k).unwrap())
            .collect();
        assert_eq!(keys, [1, 4, 5]);

        // Down to one: the others go, key and value.
        write(&mut doc, &three[..1]).unwrap();
        let back = Doc::parse(&doc.encode()).unwrap();
        assert_eq!(read(&back).unwrap(), three[..1]);
        assert_eq!(back.children(list(&back).unwrap()).count(), 2);
    }

    // Spelled out by hand: the swapped entry is written as expected and
    // everything around it as it was.
    #[test]
    fn swapping_one_island_changes_only_its_entry() {
        let [a, b] = shipped();
        let mut doc = Doc::parse(sample().as_bytes()).unwrap();
        let mine = placed(
            "data/levels/islands/normal/mine.isd",
            -96,
            576,
            1,
            64,
            Kind::Underwater,
        );
        write(&mut doc, &[a.clone(), mine.clone()]).unwrap();
        let want = world(
            &(entry(1, &a, 0, "") + &entry(4, &mine, 0, "<ObjectConfigs></ObjectConfigs>\r\n")),
        );
        assert_eq!(String::from_utf8(doc.encode()).unwrap(), want);

        // Placing the same list again changes nothing; the same file moved
        // keeps its objects and mission.
        let mut doc = Doc::parse(sample().as_bytes()).unwrap();
        write(&mut doc, &shipped()).unwrap();
        assert_eq!(String::from_utf8(doc.encode()).unwrap(), sample());
        let moved = Placed {
            x: 0,
            file: b.file.to_uppercase(),
            ..b.clone()
        };
        write(&mut doc, &[a.clone(), moved.clone()]).unwrap();
        let objects = "<ObjectConfigs><i><ID>7</ID>\r\n</i>\r\n</ObjectConfigs>\r\n";
        assert_eq!(
            String::from_utf8(doc.encode()).unwrap(),
            world(&(entry(1, &a, 0, "") + &entry(4, &moved, 12, objects)))
        );
    }

    // Removing the first of three entries by source keeps the third's
    // objects and mission with it; going by position would have handed it
    // the second's.
    #[test]
    fn deleting_a_middle_world_entry_keeps_the_others_carried_children() {
        let [a, b] = shipped();
        let c = placed(
            "data/levels/islands/normal/n_m02.isd",
            400,
            400,
            2,
            208,
            Kind::Regular,
        );
        let objects =
            |id: u32| format!("<ObjectConfigs><i><ID>{id}</ID>\r\n</i>\r\n</ObjectConfigs>\r\n");
        let three = world(
            &(entry(1, &a, 3, &objects(5)) + &entry(2, &b, 0, "") + &entry(4, &c, 12, &objects(7))),
        );
        let mut doc = Doc::parse(three.as_bytes()).unwrap();
        let moved = Placed {
            x: 416,
            ..c.clone()
        };
        write_by_source(&mut doc, &[(Some(0), a.clone()), (Some(2), moved.clone())]).unwrap();
        assert_eq!(
            String::from_utf8(doc.encode()).unwrap(),
            world(&(entry(1, &a, 3, &objects(5)) + &entry(4, &moved, 12, &objects(7))))
        );

        // A kept entry given another island loses the old one's objects
        // and mission, as `write` drops them.
        let mut doc = Doc::parse(three.as_bytes()).unwrap();
        let swapped = Placed {
            file: "data/levels/islands/normal/n_s02.isd".into(),
            name: "n_s02".into(),
            ..c.clone()
        };
        write_by_source(
            &mut doc,
            &[(Some(0), a.clone()), (Some(2), swapped.clone())],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(doc.encode()).unwrap(),
            world(
                &(entry(1, &a, 3, &objects(5))
                    + &entry(4, &swapped, 0, "<ObjectConfigs></ObjectConfigs>\r\n"))
            )
        );

        // Kept as they are, nothing changes; a new entry copies the last,
        // takes the next key and starts without objects or mission.
        let mut doc = Doc::parse(three.as_bytes()).unwrap();
        let all = [
            (Some(0), a.clone()),
            (Some(1), b.clone()),
            (Some(2), c.clone()),
        ];
        write_by_source(&mut doc, &all).unwrap();
        assert_eq!(String::from_utf8(doc.encode()).unwrap(), three);
        write_by_source(&mut doc, &[(Some(1), b.clone()), (None, a.clone())]).unwrap();
        assert_eq!(
            String::from_utf8(doc.encode()).unwrap(),
            world(&(entry(2, &b, 0, "") + &entry(5, &a, 0, "<ObjectConfigs></ObjectConfigs>\r\n")))
        );

        let mut doc = Doc::parse(three.as_bytes()).unwrap();
        for bad in [
            vec![(Some(1), b.clone()), (Some(0), a.clone())],
            vec![(None, a.clone()), (Some(0), a.clone())],
            vec![(Some(3), a.clone())],
        ] {
            assert!(write_by_source(&mut doc, &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ark_slots_and_size_read_back_as_written() {
        let mut doc = Doc::parse(sample().as_bytes()).unwrap();
        assert_eq!(size(&doc).unwrap(), [1792, 1792]);
        assert_eq!(arks(&doc).unwrap(), [[1030, 920]]);
        write_arks(&mut doc, &[[1030, 920]]).unwrap();
        assert_eq!(String::from_utf8(doc.encode()).unwrap(), sample());
        let three = [[100, 200], [1030, 920], [-16, 1600]];
        write_arks(&mut doc, &three).unwrap();
        let back = Doc::parse(&doc.encode()).unwrap();
        assert_eq!(arks(&back).unwrap(), three);
        assert_eq!(read(&back).unwrap(), shipped(), "the islands stay");
        write_arks(&mut doc, &[]).unwrap();
        assert!(arks(&doc).unwrap().is_empty());
        assert!(
            write_arks(&mut doc, &three).is_err(),
            "nothing left to copy"
        );
    }

    #[test]
    fn worlds_off_the_shipped_layout_fail() {
        let [a, _] = shipped();
        let one = entry(1, &a, 0, "");
        for bad in [
            world(&one.replace("<k>1</k>\r\n", "")),
            world(&one.replace("<hasValue>1<", "<hasValue>0<")),
            world(&one.replace("<Height>256<", "<Height>128<")),
            world(&one.replace("<Direction>0<", "<Direction>4<")),
            "<Width>32</Width>\r\n".to_owned(),
        ] {
            let doc = Doc::parse(bad.as_bytes()).unwrap();
            assert!(read(&doc).is_err(), "{bad}");
        }
        let mut empty = Doc::parse(world("").as_bytes()).unwrap();
        assert!(write(&mut empty, std::slice::from_ref(&a)).is_err());
        let mut doc = Doc::parse(sample().as_bytes()).unwrap();
        let turned = Placed { direction: 4, ..a };
        assert!(write(&mut doc, &[turned]).is_err());
    }
}
