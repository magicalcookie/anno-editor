// Map template slots (`.a7tinfo`, ADF root `MapTemplate`): where the map
// generator puts islands. Reads the slot list and turns random slots into
// fixed ones that name an island file, the way the campaign and press-event
// templates do.

use anyhow::{Context, Result, ensure};

use crate::adf::{Adf, ItemId, utf16_bytes};

/// `TemplateElement/ElementType`. Absent means a fixed island.
const ELEMENT_RANDOM: u32 = 1;

/// `Config/Type/id` of random slots, an index into the pool's `IslandType`
/// list (properties.xml `MapGenerator`); absent is 0.
pub const TYPE_NORMAL: u16 = 0;
pub const TYPE_STARTER: u16 = 1;

/// Slot `Size` ids, the pool's `IslandPlaceholderSize` order; absent is 0.
pub const SIZE_NAMES: [&str; 4] = ["Small", "Medium", "Large", "XL"];

#[derive(Debug, Clone, PartialEq)]
pub enum SlotKind {
    /// Filled from the pool: placeholder size index and island type id.
    Random { size: u16, island_type: u16 },
    /// Names its island.
    Fixed { path: String },
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
    /// count at random. Empty on every shipped template.
    pub mine_slots: Vec<(u64, bool)>,
    element: ItemId,
}

pub struct MapTemplate {
    adf: Adf,
    /// Map extent in metres.
    pub size: [u32; 2],
    pub slots: Vec<Slot>,
}

impl MapTemplate {
    pub fn parse(bytes: &[u8]) -> Result<MapTemplate> {
        let adf = Adf::decode(bytes)?;
        let root = adf
            .node(adf.root(), "MapTemplate")
            .context("a7tinfo has no MapTemplate")?;
        let size = adf
            .value(root, "Size")
            .map(|v| u32s(adf.payload(v)))
            .transpose()?
            .unwrap_or([0; 2]);
        let mut slots = Vec::new();
        for element in adf.children_named(root, "TemplateElement", false) {
            let kind = adf
                .value(element, "ElementType")
                .map(|v| adf.read_u32(v))
                .transpose()?
                .unwrap_or(0);
            let e = adf
                .node(element, "Element")
                .context("TemplateElement without Element")?;
            let position = adf
                .value(e, "Position")
                .map(|v| u32s(adf.payload(v)))
                .transpose()?
                .unwrap_or([0; 2]);
            let u16_at = |path: &str| -> Result<u16> {
                adf.value(e, path)
                    .map(|v| adf.read_u16(v))
                    .transpose()
                    .map(Option::unwrap_or_default)
            };
            let kind = match kind {
                0 => SlotKind::Fixed {
                    path: adf
                        .value(e, "MapFilePath")
                        .map(|v| adf.read_utf16(v))
                        .transpose()?
                        .unwrap_or_default(),
                },
                ELEMENT_RANDOM => SlotKind::Random {
                    size: u16_at("Size")?,
                    island_type: u16_at("Config/Type/id")?,
                },
                other => SlotKind::Other(other),
            };
            let fertilities = adf
                .value(e, "FertilityGuids")
                .map(|v| {
                    adf.payload(v)
                        .as_chunks::<4>()
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
                element,
            });
        }
        Ok(MapTemplate { adf, size, slots })
    }

    /// Turns random slot `index` into a fixed slot holding the island at
    /// `path`, keeping its position and pool type and difficulty. Mirrors the
    /// field order of shipped fixed slots. `fertilities` (empty: left to the
    /// generator) and `active_slots` (slot object IDs forced on) fill
    /// `FertilityGuids` and `MineSlotActivation`; see [`Slot`].
    pub fn fix(
        &mut self,
        index: usize,
        path: &str,
        fertilities: &[u32],
        active_slots: &[u64],
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
        let mut fertilities = fertilities.to_vec();
        fertilities.sort_unstable();
        fertilities.dedup();
        let bytes: Vec<u8> = fertilities.iter().flat_map(|g| g.to_le_bytes()).collect();
        adf.add_value(e, Some("FertilityGuids"), &bytes)?;
        let mines = adf.add_node(e, Some("MineSlotActivation"))?;
        for id in active_slots {
            adf.add_value(mines, None, &id.to_le_bytes())?;
            adf.add_value(mines, None, &[1])?;
        }
        let config = adf.add_node(e, Some("RandomIslandConfig"))?;
        let value = adf.add_node(config, Some("value"))?;
        for (name, id) in [("Type", island_type), ("Difficulty", difficulty)] {
            let node = adf.add_node(value, Some(name))?;
            if let Some(id) = id {
                adf.add_value(node, Some("id"), &id.to_le_bytes())?;
            }
        }
        let slot = &mut self.slots[index];
        slot.kind = SlotKind::Fixed {
            path: path.to_owned(),
        };
        slot.fertilities = fertilities;
        slot.mine_slots = active_slots.iter().map(|&id| (id, true)).collect();
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        self.adf.encode()
    }

    /// 1800 replaces random slot assets per object, rather than activating
    /// 117 slots against an island-wide deposit list.
    pub fn fix_1800(
        &mut self,
        index: usize,
        path: &str,
        slots: &[(u64, u32)],
        fertilities: Option<&[u32]>,
    ) -> Result<()> {
        self.fix(index, path, fertilities.unwrap_or_default(), &[])?;
        let element = self.slots[index].element;
        let adf = &mut self.adf;
        let e = adf.node(element, "Element").expect("parsed element");
        if let Some(item) = adf.child_node(e, "MineSlotActivation") {
            adf.remove(e, item);
        }
        if let Some(item) = adf.child_value(e, "RandomizeFertilities") {
            adf.remove(e, item);
        }
        if fertilities.is_some() {
            adf.add_value(e, Some("RandomizeFertilities"), &[0])?;
        }
        let list = adf.add_node(e, Some("MineSlotMapping"))?;
        for (object, guid) in slots {
            adf.add_value(list, None, &object.to_le_bytes())?;
            adf.add_value(list, None, &guid.to_le_bytes())?;
        }
        Ok(())
    }
}

fn u32s(bytes: &[u8]) -> Result<[u32; 2]> {
    ensure!(
        bytes.len() == 8,
        "expected 2×u32, got {} bytes",
        bytes.len()
    );
    Ok([
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
    ])
}

#[cfg(test)]
mod tests {
    use super::{MapTemplate, SlotKind, TYPE_STARTER};
    use crate::vfs::Install;

    fn install() -> Option<Install> {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Install::open(dir).unwrap())
    }

    #[test]
    fn every_shipped_template_round_trips() {
        let Some(install) = install() else { return };
        let paths: Vec<String> = install
            .search(".a7tinfo", 500)
            .into_iter()
            .map(|id| install.file_path(id).to_owned())
            .collect();
        assert!(paths.len() > 40, "found {} templates", paths.len());
        for path in paths {
            let bytes = install.read(&path).unwrap();
            let t = MapTemplate::parse(&bytes).unwrap();
            assert!(!t.slots.is_empty(), "{path}");
            assert_eq!(t.encode(), bytes, "{path} re-encodes byte-exactly");
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
                        island_type: TYPE_STARTER,
                        ..
                    }
                )
            })
            .expect("the easy map has starter slots");
        let at = t.slots[starter].position;
        let island = "data/base/provinces/roman/islands/custom/nova/nova.a7m";
        t.fix(starter, island, &[], &[]).unwrap();

        let back = MapTemplate::parse(&t.encode()).unwrap();
        assert_eq!(back.slots.len(), t.slots.len());
        assert_eq!(back.slots[starter].position, at);
        assert_eq!(
            back.slots[starter].kind,
            SlotKind::Fixed {
                path: island.into()
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
            .position(|s| matches!(&s.kind, SlotKind::Fixed { path } if path.contains("/pool/")))
            .unwrap();
        assert_eq!(names(&back, starter), names(&press, shipped));
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
        t.fix(n, "data/x.a7m", &[4063, 2212, 4063], &[0x2a_0000_0001])
            .unwrap();
        let back = MapTemplate::parse(&t.encode()).unwrap();
        assert_eq!(back.slots[n].fertilities, [2212, 4063], "sorted, once each");
        assert_eq!(back.slots[n].mine_slots, [(0x2a_0000_0001, true)]);
        assert_eq!(back.slots[n], t.slots[n]);
    }
}
