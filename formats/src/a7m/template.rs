// Map template slots (`.a7tinfo`, ADF root `MapTemplate`): where the map
// generator puts islands. Reads the slot list and edits it in place: moves,
// adds and removes slots, and turns random slots into fixed ones that name an
// island file (the way the campaign and press-event templates do) and back.
// Every edit keeps the field order and the zero elision of shipped elements,
// and leaves what it doesn't name as it was, so an unedited template
// re-encodes byte-exactly.

use anyhow::{Context, Result, bail, ensure};

use crate::adf::{Adf, ItemId, utf16_bytes};

/// `TemplateElement/ElementType`. The engine writes no zero value: a fixed
/// island's element has none, a random slot of size class 0 no `Size` and
/// one of type 0 no `Config/Type/id`.
struct ElementType;

impl ElementType {
    const FIXED: u32 = 0;
    const RANDOM: u32 = 1;
    /// A bare position (shipped at the map centre).
    const MARKER: u32 = 2;
}

#[derive(Debug, Clone, PartialEq)]
pub enum SlotKind {
    /// Filled from the pool: placeholder size index (`Size`) and island type
    /// id (`Config/Type/id`, an index into the pool's `IslandType` list);
    /// absent is 0.
    Random { size: u16, island_type: u16 },
    /// Names its island; `island_type` (`RandomIslandConfig/value/Type/id`)
    /// is the pool type it plays as.
    Fixed { path: String, island_type: u16 },
    /// Other element types (2 is a bare position at the map centre).
    Other(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// Min corner of the island in metres.
    pub position: [u32; 2],
    pub kind: SlotKind,
    /// `FertilityGuids` (u32 list): the island's whole fertility set. The map
    /// generator takes a non-empty list as already distributed and gives the
    /// island nothing else; shipped fixed islands list 5–6, ascending.
    pub fertilities: Vec<u32>,
    /// `MineSlotActivation`: island slot object IDs forced on (true) or off.
    /// The generator keeps these and picks the rest of the island's slot
    /// count at random. Empty on every shipped template. Production writes
    /// it and never reads it; the shell's test-map test reads it back.
    pub mine_slots: Vec<(u64, bool)>,
    /// `Rotation90`: quarter turns of a fixed island; 0 elsewhere.
    pub turns: u8,
    /// `Difficulty/id` of the slot's config, where it states one. Carried,
    /// never interpreted: shipped random slots leave it out and fixed ones
    /// state 0.
    pub difficulty: Option<u16>,
    /// The `TemplateElement`.
    element: ItemId,
}

/// How [`MapTemplate::fix`] fills the fixed island's mine slots. Which
/// form a game's fixed slot takes is the game's; the random slot can't
/// tell.
#[derive(Clone, Copy)]
pub enum Mines<'a> {
    /// `MineSlotActivation`: these slot object IDs forced on; the generator
    /// picks the rest of the island's slot count at random (117).
    Activation(&'a [u64]),
    /// `MineSlotMapping`: the deposit asset each slot object takes (1800).
    Mapping(&'a [(u64, u32)]),
}

pub struct MapTemplate {
    adf: Adf,
    /// Map extent in metres.
    pub size: [u32; 2],
    /// `PlayableArea`, `x0 y0 x1 y1` in metres. Every shipped 117 template
    /// states it.
    pub playable: Option<[u32; 4]>,
    /// Entries under `RandomlyPlacedThirdParties`; what they mean isn't
    /// known, so they are carried and never written.
    pub third_parties: usize,
    pub slots: Vec<Slot>,
}

impl MapTemplate {
    pub fn parse(bytes: &[u8]) -> Result<MapTemplate> {
        let adf = Adf::parse(bytes)?;
        let root = adf
            .node(adf.root(), "MapTemplate")
            .context("a7tinfo has no MapTemplate")?;
        let size = adf
            .value(root, "Size")
            .map(|v| u32s(adf.payload(v)))
            .transpose()?
            .unwrap_or([0; 2]);
        let playable = adf
            .value(root, "PlayableArea")
            .map(|v| u32s(adf.payload(v)))
            .transpose()?;
        let third_parties = (adf.child_node(root, "RandomlyPlacedThirdParties"))
            .map_or(0, |n| adf.children(n).count());
        let mut slots = Vec::new();
        for element in adf.children_named(root, "TemplateElement", false) {
            let kind = adf
                .value(element, "ElementType")
                .map(|v| adf.read_u32(v))
                .transpose()?
                .unwrap_or(ElementType::FIXED);
            let e = adf
                .node(element, "Element")
                .context("TemplateElement without Element")?;
            let position = adf
                .value(e, "Position")
                .map(|v| u32s(adf.payload(v)))
                .transpose()?
                .unwrap_or([0; 2]);
            let u16_at = |path: &str| -> Result<Option<u16>> {
                adf.value(e, path).map(|v| adf.read_u16(v)).transpose()
            };
            let (kind, difficulty) = match kind {
                ElementType::FIXED => (
                    SlotKind::Fixed {
                        path: adf
                            .value(e, "MapFilePath")
                            .map(|v| adf.read_utf16(v))
                            .transpose()?
                            .unwrap_or_default(),
                        island_type: u16_at("RandomIslandConfig/value/Type/id")?
                            .unwrap_or_default(),
                    },
                    u16_at("RandomIslandConfig/value/Difficulty/id")?,
                ),
                ElementType::RANDOM => (
                    SlotKind::Random {
                        size: u16_at("Size")?.unwrap_or_default(),
                        island_type: u16_at("Config/Type/id")?.unwrap_or_default(),
                    },
                    u16_at("Config/Difficulty/id")?,
                ),
                other => (SlotKind::Other(other), None),
            };
            let turns = adf
                .value(e, "Rotation90")
                .map(|v| adf.read_u8(v))
                .transpose()?
                .unwrap_or(0);
            let fertilities = adf
                .value(e, "FertilityGuids")
                .map(|v| {
                    adf.payload(v)
                        .as_chunks()
                        .0
                        .iter()
                        .map(|c| u32::from_le_bytes(*c))
                        .collect()
                })
                .unwrap_or_default();
            // Alternating unnamed u64 object ID and unnamed u8 bool.
            let mut mine_slots = Vec::new();
            if let Some(m) = adf.node(e, "MineSlotActivation") {
                let items: Vec<ItemId> = adf.children(m).collect();
                for pair in items.as_chunks::<2>().0 {
                    mine_slots.push((adf.read_u64(pair[0])?, adf.read_u8(pair[1])? != 0));
                }
            }
            slots.push(Slot {
                position,
                kind,
                fertilities,
                mine_slots,
                turns,
                difficulty,
                element,
            });
        }
        Ok(MapTemplate {
            adf,
            size,
            playable,
            third_parties,
            slots,
        })
    }

    /// Turns random slot `index` into a fixed slot holding the island at
    /// `path`, keeping its position and pool type and difficulty, in the
    /// field order of shipped fixed slots. `fertilities` fills
    /// `FertilityGuids` (`None` or empty: left to the generator; see
    /// [`Slot`]); `mines` says how the island's mine slots are filled.
    pub fn fix(
        &mut self,
        index: usize,
        path: &str,
        fertilities: Option<&[u32]>,
        mines: Mines,
    ) -> Result<()> {
        let slot = &self.slots[index];
        ensure!(
            matches!(slot.kind, SlotKind::Random { .. }),
            "slot {index} is not a random slot"
        );
        let element = slot.element;
        let adf = &mut self.adf;
        if let Some(t) = adf.value(element, "ElementType") {
            adf.remove(element, t);
        }
        let e = adf.node(element, "Element").expect("parsed above");
        let ids = |name: &str| -> Result<Option<u16>> {
            adf.value(e, &format!("Config/{name}/id"))
                .map(|v| adf.read_u16(v))
                .transpose()
        };
        let (island_type, difficulty) = (ids("Type")?, ids("Difficulty")?);
        for name in ["Size", "Difficulty", "Config"] {
            for item in adf
                .children(e)
                .filter(|&c| adf.name(c) == Some(name))
                .collect::<Vec<_>>()
            {
                adf.remove(e, item);
            }
        }
        adf.add_value(e, Some("MapFilePath"), &utf16_bytes(path))?;
        adf.add_value(e, Some("Rotation90"), &[0])?;
        let mut sorted = fertilities.unwrap_or_default().to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let bytes: Vec<u8> = sorted.iter().flat_map(|g| g.to_le_bytes()).collect();
        adf.add_value(e, Some("FertilityGuids"), &bytes)?;
        // Mapped slots add and drop the activation list too: its name stays
        // in the file's dictionary (21 bytes), which the recorded 1800 test
        // maps hold.
        let activation = adf.add_node(e, Some("MineSlotActivation"))?;
        if let Mines::Activation(active) = mines {
            for id in active {
                adf.add_value(activation, None, &id.to_le_bytes())?;
                adf.add_value(activation, None, &[1])?;
            }
        }
        let config = adf.add_node(e, Some("RandomIslandConfig"))?;
        let value = adf.add_node(config, Some("value"))?;
        for (name, id) in [("Type", island_type), ("Difficulty", difficulty)] {
            let node = adf.add_node(value, Some(name))?;
            if let Some(id) = id {
                adf.add_value(node, Some("id"), &id.to_le_bytes())?;
            }
        }
        let mut mine_slots = Vec::new();
        match mines {
            Mines::Activation(active) => {
                mine_slots = active.iter().map(|&id| (id, true)).collect();
            }
            Mines::Mapping(objects) => {
                adf.remove(e, activation);
                if let Some(item) = adf.child_value(e, "RandomizeFertilities") {
                    adf.remove(e, item);
                }
                // A fixed list stands only with the generator's own
                // distribution switched off.
                if fertilities.is_some() {
                    adf.add_value(e, Some("RandomizeFertilities"), &[0])?;
                }
                let list = adf.add_node(e, Some("MineSlotMapping"))?;
                for (object, guid) in objects {
                    adf.add_value(list, None, &object.to_le_bytes())?;
                    adf.add_value(list, None, &guid.to_le_bytes())?;
                }
            }
        }
        let slot = &mut self.slots[index];
        slot.kind = SlotKind::Fixed {
            path: path.to_owned(),
            island_type: island_type.unwrap_or_default(),
        };
        slot.fertilities = sorted;
        slot.mine_slots = mine_slots;
        slot.turns = 0;
        Ok(())
    }

    /// Sets `PlayableArea`; fails on a template that states none.
    pub fn set_playable(&mut self, area: [u32; 4]) -> Result<()> {
        let root = self.root()?;
        let item = (self.adf.child_value(root, "PlayableArea"))
            .context("the template states no PlayableArea")?;
        self.adf.set_payload(item, &u32_bytes(&area));
        self.playable = Some(area);
        Ok(())
    }

    /// Moves slot `index`'s min corner to `at`, metres.
    pub fn set_position(&mut self, index: usize, at: [u32; 2]) -> Result<()> {
        if self.slots[index].position == at {
            return Ok(());
        }
        let e = self.element(index)?;
        match self.adf.child_value(e, "Position") {
            Some(v) => self.adf.set_payload(v, &u32_bytes(&at)),
            None => {
                let v = self.adf.add_value(e, Some("Position"), &u32_bytes(&at))?;
                move_behind(&mut self.adf, e, v, None)?;
            }
        }
        self.slots[index].position = at;
        Ok(())
    }

    /// Sets random slot `index`'s size class and pool type.
    pub fn set_random(&mut self, index: usize, size: u16, island_type: u16) -> Result<()> {
        let SlotKind::Random {
            size: was,
            island_type: was_type,
        } = self.slots[index].kind
        else {
            bail!("slot {index} is not a random slot");
        };
        let e = self.element(index)?;
        if size != was {
            let item = self.adf.child_value(e, "Size");
            match (item, size) {
                (Some(v), 0) => _ = self.adf.remove(e, v),
                (Some(v), _) => self.adf.set_payload(v, &size.to_le_bytes()),
                (None, _) => {
                    let v = self.adf.add_value(e, Some("Size"), &size.to_le_bytes())?;
                    let position = self.adf.child_value(e, "Position");
                    move_behind(&mut self.adf, e, v, position)?;
                }
            }
        }
        if island_type != was_type {
            let ty = self
                .adf
                .node(e, "Config/Type")
                .context("random slot without Config/Type")?;
            set_id(&mut self.adf, ty, island_type)?;
        }
        self.slots[index].kind = SlotKind::Random { size, island_type };
        Ok(())
    }

    /// Sets fixed slot `index`'s pool type, the role it plays.
    pub fn set_fixed_type(&mut self, index: usize, island_type: u16) -> Result<()> {
        let SlotKind::Fixed {
            island_type: was, ..
        } = &mut self.slots[index].kind
        else {
            bail!("slot {index} is not a fixed slot");
        };
        if *was == island_type {
            return Ok(());
        }
        *was = island_type;
        let e = self.element(index)?;
        let ty = (self.adf.node(e, "RandomIslandConfig/value/Type"))
            .context("fixed slot without RandomIslandConfig/value/Type")?;
        set_id(&mut self.adf, ty, island_type)
    }

    /// Turns fixed slot `index`'s island by `turns` quarter turns.
    pub fn set_turns(&mut self, index: usize, turns: u8) -> Result<()> {
        ensure!(turns < 4, "{turns} quarter turns");
        ensure!(
            matches!(self.slots[index].kind, SlotKind::Fixed { .. }),
            "slot {index} is not a fixed slot"
        );
        if self.slots[index].turns == turns {
            return Ok(());
        }
        let e = self.element(index)?;
        let item =
            (self.adf.child_value(e, "Rotation90")).context("fixed slot without Rotation90")?;
        self.adf.set_payload(item, &[turns]);
        self.slots[index].turns = turns;
        Ok(())
    }

    /// Puts the island at `path` into fixed slot `index`. A different
    /// island drops the fertilities and mine slots the old one was given:
    /// they name its slot objects.
    pub fn set_island(&mut self, index: usize, path: &str) -> Result<()> {
        let SlotKind::Fixed { path: was, .. } = &mut self.slots[index].kind else {
            bail!("slot {index} is not a fixed slot");
        };
        if was == path {
            return Ok(());
        }
        *was = path.to_owned();
        let e = self.element(index)?;
        let adf = &mut self.adf;
        let item = (adf.child_value(e, "MapFilePath")).context("fixed slot without MapFilePath")?;
        adf.set_payload(item, &utf16_bytes(path));
        if let Some(f) = adf.child_value(e, "FertilityGuids") {
            adf.set_payload(f, &[]);
        }
        for name in ["MineSlotActivation", "MineSlotMapping"] {
            if let Some(m) = adf.child_node(e, name) {
                adf.clear_children(m);
            }
        }
        let slot = &mut self.slots[index];
        slot.fertilities.clear();
        slot.mine_slots.clear();
        Ok(())
    }

    /// Turns slot `index`, fixed or not, into a random slot of `size` and
    /// `island_type` at the same position, shaped as shipped random slots
    /// are. What it held besides its position goes.
    pub fn unfix(&mut self, index: usize, size: u16, island_type: u16) -> Result<()> {
        let element = self.slots[index].element;
        let at = self.slots[index].position;
        self.adf.clear_children(element);
        random_element(&mut self.adf, element, at, size, island_type)?;
        let slot = &mut self.slots[index];
        slot.kind = SlotKind::Random { size, island_type };
        slot.fertilities.clear();
        slot.mine_slots.clear();
        slot.turns = 0;
        slot.difficulty = None;
        Ok(())
    }

    /// Removes slot `index`; later slots move down one.
    pub fn remove(&mut self, index: usize) -> Result<()> {
        let root = self.root()?;
        let slot = self.slots.remove(index);
        self.adf.remove(root, slot.element);
        self.count_elements()
    }

    /// Appends a random slot of `size` and `island_type` at `at`, shaped as
    /// shipped random slots are; its index.
    pub fn add_random(&mut self, at: [u32; 2], size: u16, island_type: u16) -> Result<usize> {
        let root = self.root()?;
        let element = self.adf.add_node(root, Some("TemplateElement"))?;
        random_element(&mut self.adf, element, at, size, island_type)?;
        self.slots.push(Slot {
            position: at,
            kind: SlotKind::Random { size, island_type },
            fertilities: Vec::new(),
            mine_slots: Vec::new(),
            turns: 0,
            difficulty: None,
            element,
        });
        self.count_elements()?;
        Ok(self.slots.len() - 1)
    }

    /// Appends a bare position, as the shipped map-centre element; its
    /// index.
    pub fn add_marker(&mut self, at: [u32; 2]) -> Result<usize> {
        let root = self.root()?;
        let element = self.adf.add_node(root, Some("TemplateElement"))?;
        let adf = &mut self.adf;
        adf.add_value(
            element,
            Some("ElementType"),
            &ElementType::MARKER.to_le_bytes(),
        )?;
        let e = adf.add_node(element, Some("Element"))?;
        adf.add_value(e, Some("Position"), &u32_bytes(&at))?;
        self.slots.push(Slot {
            position: at,
            kind: SlotKind::Other(ElementType::MARKER),
            fertilities: Vec::new(),
            mine_slots: Vec::new(),
            turns: 0,
            difficulty: None,
            element,
        });
        self.count_elements()?;
        Ok(self.slots.len() - 1)
    }

    fn root(&self) -> Result<ItemId> {
        (self.adf.node(self.adf.root(), "MapTemplate")).context("a7tinfo has no MapTemplate")
    }

    fn element(&self, index: usize) -> Result<ItemId> {
        (self.adf.node(self.slots[index].element, "Element"))
            .context("TemplateElement without Element")
    }

    // `ElementCount` states how many elements follow; every shipped
    // template has it right.
    fn count_elements(&mut self) -> Result<()> {
        let root = self.root()?;
        if let Some(v) = self.adf.child_value(root, "ElementCount") {
            let n = u32::try_from(self.slots.len())?;
            self.adf.set_payload(v, &n.to_le_bytes());
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        self.adf.encode()
    }
}

fn u32s<const N: usize>(bytes: &[u8]) -> Result<[u32; N]> {
    let (words, rest) = bytes.as_chunks::<4>();
    ensure!(
        words.len() == N && rest.is_empty(),
        "expected {N}×u32, got {} bytes",
        bytes.len()
    );
    Ok(std::array::from_fn(|i| u32::from_le_bytes(words[i])))
}

fn u32_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

// A random slot's fields into the empty `TemplateElement` `element`, as
// shipped: `Size` and `Config/Type/id` only when not 0, both difficulties
// empty.
fn random_element(
    adf: &mut Adf,
    element: ItemId,
    at: [u32; 2],
    size: u16,
    island_type: u16,
) -> Result<()> {
    adf.add_value(
        element,
        Some("ElementType"),
        &ElementType::RANDOM.to_le_bytes(),
    )?;
    let e = adf.add_node(element, Some("Element"))?;
    adf.add_value(e, Some("Position"), &u32_bytes(&at))?;
    if size != 0 {
        adf.add_value(e, Some("Size"), &size.to_le_bytes())?;
    }
    adf.add_node(e, Some("Difficulty"))?;
    let config = adf.add_node(e, Some("Config"))?;
    let ty = adf.add_node(config, Some("Type"))?;
    set_id(adf, ty, island_type)?;
    adf.add_node(config, Some("Difficulty"))?;
    Ok(())
}

// Sets a config node's `id`, leaving it out for 0 as the engine does.
fn set_id(adf: &mut Adf, node: ItemId, id: u16) -> Result<()> {
    match (adf.child_value(node, "id"), id) {
        (Some(v), 0) => _ = adf.remove(node, v),
        (Some(v), _) => adf.set_payload(v, &id.to_le_bytes()),
        (None, 0) => {}
        (None, _) => _ = adf.add_value(node, Some("id"), &id.to_le_bytes())?,
    }
    Ok(())
}

// Appends a copy of `src`'s subtree to `parent`.
fn copy_tree(adf: &mut Adf, src: ItemId, parent: ItemId) -> Result<ItemId> {
    let name = adf.name(src).map(str::to_owned);
    if !adf.is_node(src) {
        let payload = adf.payload(src).to_vec();
        return adf.add_value(parent, name.as_deref(), &payload);
    }
    let copy = adf.add_node(parent, name.as_deref())?;
    for child in adf.children(src).collect::<Vec<_>>() {
        copy_tree(adf, child, copy)?;
    }
    Ok(copy)
}

// The ADF tree only appends, so a field that belongs mid-element is added
// last and the siblings after `after` (the first ones when `None`) move
// behind it, by copy.
fn move_behind(adf: &mut Adf, parent: ItemId, item: ItemId, after: Option<ItemId>) -> Result<()> {
    let kids: Vec<ItemId> = adf.children(parent).collect();
    let from = after
        .and_then(|a| kids.iter().position(|&k| k == a))
        .map_or(0, |p| p + 1);
    for &k in &kids[from..] {
        if k != item {
            copy_tree(adf, k, parent)?;
            adf.remove(parent, k);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{MapTemplate, Mines, SlotKind};
    use crate::vfs::Install;

    fn install_of(var: &str) -> Option<Install> {
        let Some(dir) = crate::vfs::install_dir(var) else {
            eprintln!("skip: {var} unset");
            return None;
        };
        Some(Install::open(dir, crate::vfs::Install::DATA_FIRST).unwrap())
    }

    fn install() -> Option<Install> {
        install_of("ANNO117_DIR")
    }

    // Needs Anno 117 and 1800: every shipped template re-encodes
    // byte-exactly, and its element count is right.
    #[test]
    fn every_shipped_template_round_trips() {
        // 1800 ships 93 (2026-11-20).
        for (var, least) in [("ANNO117_DIR", 40), ("ANNO1800_DIR", 90)] {
            let Some(install) = install_of(var) else {
                continue;
            };
            let paths: Vec<String> = install
                .search(".a7tinfo", 500)
                .into_iter()
                .map(|id| install.file_path(id).to_owned())
                .collect();
            assert!(
                paths.len() >= least,
                "{var}: found {} templates",
                paths.len()
            );
            for path in paths {
                let bytes = install.read(&path).unwrap();
                let t = MapTemplate::parse(&bytes).unwrap();
                assert!(!t.slots.is_empty(), "{path}");
                if let Some(n) = element_count(&t) {
                    assert_eq!(n, t.slots.len(), "{path}");
                }
                assert_eq!(t.encode(), bytes, "{path} re-encodes byte-exactly");
            }
        }
    }

    #[test]
    fn a_fixed_slot_reads_back_like_a_shipped_one() {
        let Some(install) = install() else { return };
        let base = "data/base/provinces/roman/templates/pool/roman_province_default_01_easy/roman_province_default_01_easy.a7tinfo";
        let mut t = MapTemplate::parse(&install.read(base).unwrap()).unwrap();
        let starter = t
            .slots
            .iter()
            .position(|s| {
                matches!(
                    s.kind,
                    SlotKind::Random {
                        // 117's Starter type.
                        island_type: 1,
                        ..
                    }
                )
            })
            .expect("the easy map has starter slots");
        let at = t.slots[starter].position;
        let island = "data/base/provinces/roman/islands/custom/nova/nova.a7m";
        t.fix(starter, island, None, Mines::Activation(&[]))
            .unwrap();

        let back = MapTemplate::parse(&t.encode()).unwrap();
        assert_eq!(back.slots.len(), t.slots.len());
        assert_eq!(back.slots[starter].position, at);
        assert_eq!(
            back.slots[starter].kind,
            SlotKind::Fixed {
                path: island.into(),
                island_type: 1,
            }
        );

        // Same fields, in the same order, as a shipped fixed starter slot of
        // the press-event map (which has no IslandLabel either).
        let names = |t: &MapTemplate, i: usize| -> Vec<String> {
            let e = t.adf.node(t.slots[i].element, "Element").unwrap();
            t.adf
                .children(e)
                .filter_map(|c| t.adf.name(c).map(str::to_owned))
                .collect()
        };
        let press = MapTemplate::parse(
            &install
                .read(
                    install
                        .file_path(install.search("celtic_province_gamescom25_01.a7tinfo", 1)[0]),
                )
                .unwrap(),
        )
        .unwrap();
        let shipped = press
            .slots
            .iter()
            .position(
                |s| matches!(&s.kind, SlotKind::Fixed { path, .. } if path.contains("/pool/")),
            )
            .unwrap();
        assert_eq!(names(&back, starter), names(&press, shipped));
    }

    // Every field name under a slot's `TemplateElement`, with its depth,
    // in file order.
    fn shape(t: &MapTemplate, i: usize) -> Vec<(usize, String)> {
        fn walk(
            adf: &crate::adf::Adf,
            item: crate::adf::ItemId,
            depth: usize,
            out: &mut Vec<(usize, String)>,
        ) {
            for c in adf.children(item) {
                out.push((depth, adf.name(c).unwrap_or("").to_owned()));
                if adf.is_node(c) {
                    walk(adf, c, depth + 1, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&t.adf, t.slots[i].element, 0, &mut out);
        out
    }

    fn template(install: &Install, name: &str) -> MapTemplate {
        let path = install.file_path(install.search(name, 1)[0]).to_owned();
        MapTemplate::parse(&install.read(&path).unwrap()).unwrap()
    }

    fn element_count(t: &MapTemplate) -> Option<usize> {
        let root = t.root().unwrap();
        let v = t.adf.child_value(root, "ElementCount")?;
        Some(t.adf.read_u32(v).unwrap() as usize)
    }

    // Needs Anno 117: a moved slot rewrites its 8-byte position in place;
    // every other byte stays.
    #[test]
    fn moving_one_slot_changes_only_its_element() {
        let Some(install) = install() else { return };
        let bytes = install
            .read("data/base/provinces/roman/templates/pool/roman_province_default_01_easy/roman_province_default_01_easy.a7tinfo")
            .unwrap();
        let mut t = MapTemplate::parse(&bytes).unwrap();
        let n = t.slots.len() / 2;
        let at = t.slots[n].position.map(|v| v + 16);
        t.set_position(n, at).unwrap();
        let written = t.encode();
        assert_eq!(written.len(), bytes.len());
        let changed: Vec<usize> = (0..bytes.len())
            .filter(|&i| bytes[i] != written[i])
            .collect();
        assert!(
            !changed.is_empty() && changed.last().unwrap() - changed[0] < 8,
            "{changed:?}"
        );
        let back = MapTemplate::parse(&written).unwrap();
        for (i, (a, b)) in back.slots.iter().zip(&t.slots).enumerate() {
            assert_eq!((a.position, &a.kind), (b.position, &b.kind), "slot {i}");
        }
        assert_eq!(back.slots[n].position, at);
    }

    // Needs Anno 117: added, retyped, unfixed and removed slots read back as
    // set, with the field names and order of a shipped slot of their kind,
    // and `ElementCount` follows.
    #[test]
    fn edited_slots_keep_the_shipped_shape() {
        let Some(install) = install() else { return };
        let shipped: Vec<MapTemplate> = ["easy", "medium", "hard"]
            .map(|v| template(&install, &format!("roman_province_default_01_{v}.a7tinfo")))
            .into_iter()
            .collect();
        let press = template(&install, "celtic_province_gamescom25_01.a7tinfo");
        // A shipped slot of `want`'s kind: template and index.
        let like = |want: &SlotKind| -> (&MapTemplate, usize) {
            (shipped.iter())
                .find_map(|t| Some((t, t.slots.iter().position(|s| s.kind == *want)?)))
                .unwrap_or_else(|| panic!("no shipped {want:?}"))
        };
        let mut t = template(&install, "celtic_province_gamescom25_01.a7tinfo");
        let fixed = (t.slots.iter())
            .position(|s| matches!(s.kind, SlotKind::Fixed { .. }))
            .unwrap();
        let count = t.slots.len();
        let small = t.add_random([100, 200], 0, 0).unwrap();
        let large = t.add_random([600, 200], 2, 1).unwrap();
        let marker = t.add_marker([1024, 1024]).unwrap();
        let retyped = t.add_random([900, 900], 0, 0).unwrap();
        t.set_random(retyped, 1, 4).unwrap();
        t.unfix(fixed, 3, 1).unwrap();
        t.remove(0).unwrap();
        let (small, large, marker, retyped, fixed) =
            (small - 1, large - 1, marker - 1, retyped - 1, fixed - 1);
        assert_eq!(t.slots.len(), count + 3);
        assert_eq!(element_count(&t), Some(t.slots.len()));

        let back = MapTemplate::parse(&t.encode()).unwrap();
        assert_eq!(back.encode(), t.encode());
        for (i, (a, b)) in back.slots.iter().zip(&t.slots).enumerate() {
            assert_eq!(
                (a.position, &a.kind, a.turns),
                (b.position, &b.kind, b.turns),
                "slot {i}"
            );
        }
        assert_eq!(
            back.slots[retyped].kind,
            SlotKind::Random {
                size: 1,
                island_type: 4
            }
        );
        assert_eq!(
            back.slots[fixed].kind,
            SlotKind::Random {
                size: 3,
                island_type: 1
            }
        );
        for n in [small, large, marker, retyped, fixed] {
            let (shipped, want) = like(&back.slots[n].kind);
            assert_eq!(
                shape(&back, n),
                shape(shipped, want),
                "slot {n}: {:?}",
                back.slots[n].kind
            );
        }

        // A random slot made fixed, turned, retyped and given another
        // island has a shipped fixed slot's fields. Below them they differ
        // as `fix` always has: the shipped slots state difficulty 0, a
        // fixed one copies the random slot's, which states none.
        t.fix(small, "data/x.a7m", None, Mines::Activation(&[]))
            .unwrap();
        t.set_turns(small, 3).unwrap();
        t.set_fixed_type(small, 3).unwrap();
        t.set_island(small, "data/y.a7m").unwrap();
        let back = MapTemplate::parse(&t.encode()).unwrap();
        assert_eq!(
            back.slots[small].kind,
            SlotKind::Fixed {
                path: "data/y.a7m".into(),
                island_type: 3
            }
        );
        assert_eq!(back.slots[small].turns, 3);
        let shipped = (press.slots.iter())
            .position(|s| matches!(&s.kind, SlotKind::Fixed { path, island_type: 3 } if path.contains("3rdparty")))
            .unwrap();
        let names = |t: &MapTemplate, i| -> Vec<(usize, String)> {
            (shape(t, i).into_iter())
                .filter(|(depth, n)| {
                    *depth <= 2 && n != "IslandLabel" && n != "RandomizeFertilities"
                })
                .collect()
        };
        assert_eq!(names(&back, small), names(&press, shipped));
    }

    #[test]
    fn fixed_slot_fertilities_read_back_in_the_shipped_encoding() {
        let Some(install) = install() else { return };
        // The campaign's player islands carry their fertilities as shipped.
        let campaign = "data/base/provinces/celtic/templates/campaign/celtic_province_campaign_01/celtic_province_campaign_01.a7tinfo";
        let shipped = MapTemplate::parse(&install.read(campaign).unwrap()).unwrap();
        assert!(
            shipped
                .slots
                .iter()
                .any(|s| s.fertilities == [2212, 2217, 2218, 4049, 4063, 8487]),
            "campaign player island 01's list"
        );
        assert!(shipped.slots.iter().all(|s| s.mine_slots.is_empty()));

        let base = "data/base/provinces/celtic/templates/pool/celtic_province_default_01_medium/celtic_province_default_01_medium.a7tinfo";
        let mut t = MapTemplate::parse(&install.read(base).unwrap()).unwrap();
        let n = t
            .slots
            .iter()
            .position(|s| matches!(s.kind, SlotKind::Random { .. }))
            .unwrap();
        t.fix(
            n,
            "data/x.a7m",
            Some(&[4063, 2212, 4063]),
            Mines::Activation(&[0x2a_0000_0001]),
        )
        .unwrap();
        let back = MapTemplate::parse(&t.encode()).unwrap();
        assert_eq!(back.slots[n].fertilities, [2212, 4063], "sorted, once each");
        assert_eq!(back.slots[n].mine_slots, [(0x2a_0000_0001, true)]);
        assert_eq!(back.slots[n], t.slots[n]);
    }
}
