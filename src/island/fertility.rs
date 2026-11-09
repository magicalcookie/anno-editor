// Owns which resources (deposits) a slot can be given and the fertility list
// a test-map island carries. The game has no per-slot resource: a mine needs
// the island to own the deposit (`Fertility`, `NeededFertility` on the
// building) and a free slot of the deposit's `NeedsSlot` kind. The map
// generator hands fertilities out per island from `FertilitySet`s; a fixed
// template slot may name the island's whole list in `FertilityGuids`
// instead.
use anno_formats::assets::Assets;
use std::collections::BTreeSet;

/// A deposit a slot kind can serve.
#[derive(Clone, Debug, PartialEq)]
pub struct Deposit {
    pub guid: u32,
    pub label: String,
}

/// Every fertility the region's shipped sets can hand out, pools expanded.
/// Sets are matched by name ("Fertility Set Celtic 2nd Island Medium"); the
/// `ResourceSetCondition/AllowedRegion` they carry says the same.
fn region_fertilities(assets: &Assets, region: &str) -> BTreeSet<u32> {
    let prefix = format!("Fertility Set {region} ");
    let mut out = BTreeSet::new();
    for set in assets.iter().filter(|a| {
        a.template == "FertilitySet"
            && a.allowed_regions.as_deref().map_or_else(
                || a.name.starts_with(&prefix),
                |r| r.split(';').any(|r| r == region),
            )
    }) {
        for &g in &set.fertilities {
            match assets.get(g) {
                Some(pool) if pool.template == "FertilityPool" => out.extend(&pool.fertilities),
                _ => {
                    out.insert(g);
                }
            }
        }
    }
    out
}

/// Crop and water fertilities offered by the region's shipped sets.
pub fn choices(assets: &Assets, region: &str) -> Vec<Deposit> {
    let mut out: Vec<_> = region_fertilities(assets, region)
        .into_iter()
        .filter(|g| assets.get(*g).is_some_and(|a| a.template == "Fertility"))
        .map(|guid| Deposit {
            guid,
            label: label(assets, guid),
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// Deposits the region offers that need a `slot_type` slot (`Mountain`,
/// `River`, `Marsh`), by display name.
pub fn deposits(assets: &Assets, region: &str, slot_type: &str) -> Vec<Deposit> {
    let mut out: Vec<Deposit> = region_fertilities(assets, region)
        .into_iter()
        .filter_map(|g| assets.get(g))
        .filter(|a| a.template == "Fertility" && a.needs_slot.as_deref() == Some(slot_type))
        .map(|a| Deposit {
            guid: a.guid,
            label: a.text.clone().unwrap_or_else(|| {
                a.name
                    .trim_start_matches("Deposit ")
                    .trim_start_matches("Fertility ")
                    .to_owned()
            }),
        })
        .collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// Short name of a deposit for labels.
pub fn label(assets: &Assets, guid: u32) -> String {
    if guid == 0 {
        return "Empty".to_owned();
    }
    assets.get(guid).map_or(guid.to_string(), |a| {
        a.text.clone().unwrap_or_else(|| a.name.clone())
    })
}

/// Slot kind of a slot asset, from its name ("Fixed Slot Celtic Mountain",
/// "Slot Celtic Marsh"); `Slot/SlotType` is not inherited by the fixed
/// variants and marsh slots have none.
pub fn slot_type_of(assets: &Assets, guid: u32) -> Option<&'static str> {
    let name = &assets.get(guid)?.name;
    ["Mountain", "River", "Marsh"]
        .into_iter()
        .find(|t| name.contains(t))
}

/// `FertilityGuids` for a fixed test-map island with `assigned` deposits:
/// the region's Medium-resources set for its role (1st island for starts,
/// 2nd otherwise), each pool resolved to its first member the island can
/// use and does not have yet, plus the assigned ones. A non-empty list is
/// all the island gets, so it must carry crops and fish as well; shipped
/// fixed islands list 5–6 entries. Entries needing a slot kind the island
/// lacks (`slot_types`) are skipped. Empty when nothing is assigned, which
/// leaves the island to the generator.
pub fn island_list(
    assets: &Assets,
    region: &str,
    start: bool,
    slot_types: &[&str],
    assigned: &[u32],
) -> Vec<u32> {
    if assigned.is_empty() {
        return Vec::new();
    }
    let mut chosen: BTreeSet<u32> = assigned.iter().copied().collect();
    let usable = |g: u32| {
        assets.get(g).is_some_and(|a| {
            a.template == "Fertility"
                && a.needs_slot
                    .as_deref()
                    .is_none_or(|t| slot_types.contains(&t))
        })
    };
    let name = format!(
        "Fertility Set {region} {} Island Medium",
        if start { "1st" } else { "2nd" }
    );
    if let Some(set) = assets.iter().find(|a| a.name == name) {
        for &g in &set.fertilities {
            let pick = match assets.get(g) {
                Some(pool) if pool.template == "FertilityPool" => pool
                    .fertilities
                    .iter()
                    .copied()
                    .find(|&f| usable(f) && !chosen.contains(&f)),
                _ => Some(g).filter(|&g| usable(g)),
            };
            chosen.extend(pick);
        }
    }
    chosen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anno_formats::vfs::Install;

    fn assets() -> Option<Assets> {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return None;
        };
        Some(Assets::load(&Install::open(dir).unwrap()).unwrap())
    }

    #[test]
    fn slot_kinds_offer_the_regions_deposits() {
        let Some(assets) = assets() else { return };
        let guids = |region, kind| -> Vec<u32> {
            let mut g: Vec<u32> = deposits(&assets, region, kind)
                .iter()
                .map(|d| d.guid)
                .collect();
            g.sort();
            g
        };
        // Iron, copper, tin, granite, silver; birds and beavers.
        assert_eq!(guids("Celtic", "Mountain"), [4049, 4063, 4064, 4066, 8487]);
        assert_eq!(guids("Celtic", "Marsh"), [2219, 4082]);
        assert!(guids("Celtic", "River").is_empty());
        // Iron, marble, minerals; sturgeon and gold.
        assert_eq!(guids("Roman", "Mountain"), [4049, 4053, 4062]);
        assert_eq!(guids("Roman", "River"), [8577, 32027]);
        assert!(
            deposits(&assets, "Celtic", "Mountain")
                .iter()
                .all(|d| !d.label.is_empty() && !d.label.starts_with("Deposit"))
        );
        assert_eq!(slot_type_of(&assets, 38473), Some("Mountain"));
        assert_eq!(slot_type_of(&assets, 5282), Some("Marsh"));
    }

    #[test]
    fn an_assigned_deposit_joins_a_full_island_list() {
        let Some(assets) = assets() else { return };
        assert!(island_list(&assets, "Celtic", false, &["Mountain"], &[]).is_empty());
        let list = island_list(&assets, "Celtic", false, &["Mountain"], &[4066]);
        assert!(list.contains(&4066), "{list:?}");
        assert!(list.windows(2).all(|w| w[0] < w[1]), "ascending: {list:?}");
        assert!(list.len() >= 4, "crops and fish too: {list:?}");
        // No marsh slots on the island: no birds or beavers.
        assert!(!list.contains(&2219) && !list.contains(&4082), "{list:?}");
        let marsh = island_list(&assets, "Celtic", false, &["Mountain", "Marsh"], &[2219]);
        assert!(marsh.contains(&2219));
    }
}
