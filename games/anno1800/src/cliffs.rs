// The pieces auto cliffs dress steep ground with, per region, from the
// census of shipped islands' cliff and rock placements.
use anno_core::game::{CliffClass, CliffSet};

// The most-used substantial props in the installed 1800 regional pools.
// Pebbles are omitted. PRPs keep their bounds in the referenced RDM.
pub const MODERATE: CliffSet = CliffSet {
    pieces: &[
        (
            "data/graphics/props/terrain_props/rocks/rocks_moderate/rock_moderate_17.prp",
            CliffClass::Cliff,
            5324,
        ),
        (
            "data/graphics/props/terrain_props/rocks/rocks_moderate/rock_moderate_16.prp",
            CliffClass::Cliff,
            826,
        ),
        (
            "data/graphics/props/terrain_props/rocks/rocks_moderate/rock_moderate_19.prp",
            CliffClass::Boulder,
            2027,
        ),
        (
            "data/graphics/props/terrain_props/rocks/rocks_moderate/rock_moderate_20.prp",
            CliffClass::Boulder,
            1294,
        ),
    ],
    cliff_relief: 8.,
    spacing: 1.,
};

pub const COLONY: CliffSet = CliffSet {
    pieces: &[
        (
            "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_17.prp",
            CliffClass::Cliff,
            2373,
        ),
        (
            "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_23.prp",
            CliffClass::Cliff,
            1477,
        ),
        (
            "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_19.prp",
            CliffClass::Boulder,
            1371,
        ),
        (
            "data/graphics/props/terrain_props/rocks/rocks_jungle/rock_jungle_20.prp",
            CliffClass::Boulder,
            1247,
        ),
    ],
    cliff_relief: 8.,
    spacing: 1.,
};

pub const ARCTIC: CliffSet = CliffSet {
    pieces: &[
        (
            "data/dlc03/graphics/props/terrain_props/vegetation/rocks/rocks_snow_noskirt/rock_snow_noskirt_17.prp",
            CliffClass::Cliff,
            195,
        ),
        (
            "data/dlc03/graphics/props/terrain_props/vegetation/rocks/rocks_ice/rock_ice_17.prp",
            CliffClass::Cliff,
            65,
        ),
        (
            "data/dlc03/graphics/props/terrain_props/vegetation/rocks/rocks_snow_noskirt/rock_snow_noskirt_19.prp",
            CliffClass::Boulder,
            62,
        ),
        (
            "data/dlc03/graphics/props/terrain_props/vegetation/rocks/rocks_snow_noskirt/rock_snow_noskirt_20.prp",
            CliffClass::Boulder,
            30,
        ),
    ],
    cliff_relief: 8.,
    spacing: 1.,
};

pub const AFRICA: CliffSet = CliffSet {
    pieces: &[
        (
            "data/dlc06/graphics/props/terrain_props/rocks/rocks_colony02/rock_colony02_09.prp",
            CliffClass::Cliff,
            452,
        ),
        (
            "data/dlc06/graphics/props/terrain_props/rocks/rocks_colony02/rock_colony02_moderate_17.prp",
            CliffClass::Cliff,
            126,
        ),
        (
            "data/dlc06/graphics/props/terrain_props/rocks/rocks_colony02/rock_colony02_07.prp",
            CliffClass::Boulder,
            374,
        ),
        (
            "data/dlc06/graphics/props/terrain_props/rocks/rocks_colony02/rock_colony02_10.prp",
            CliffClass::Boulder,
            296,
        ),
    ],
    cliff_relief: 6.,
    spacing: 1.,
};
