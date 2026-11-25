// The pieces auto cliffs dress steep ground with, per region, from the
// census of shipped islands' cliff and rock placements.
use anno_core::game::{CliffClass, CliffSet};

pub const ROMAN: CliffSet = CliffSet {
    pieces: &[
        (
            "data/base/graphics/roman/environment/props/rocks/roman_cliff_01/roman_cliff_01_b_noskirt.cfg",
            CliffClass::Cliff,
            221,
        ),
        (
            "data/base/graphics/roman/environment/props/rocks/roman_cliff_01/roman_cliff_01_h_noskirt.cfg",
            CliffClass::Cliff,
            81,
        ),
        (
            "data/base/graphics/roman/environment/props/rocks/roman_cliff_01/roman_cliff_01_e_noskirt.cfg",
            CliffClass::Cliff,
            72,
        ),
        (
            "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_b.cfg",
            CliffClass::Boulder,
            1270,
        ),
        (
            "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_a.cfg",
            CliffClass::Boulder,
            271,
        ),
        (
            "data/base/graphics/roman/environment/props/rocks/roman_rock_02/roman_rock_02_c.cfg",
            CliffClass::Boulder,
            197,
        ),
        (
            "data/base/graphics/roman/environment/props/rocks/roman_rock_05/roman_rock_05_g.cfg",
            CliffClass::Boulder,
            265,
        ),
    ],
    cliff_relief: 8.,
    spacing: 1.,
};

pub const CELTIC: CliffSet = CliffSet {
    pieces: &[
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_cliff_02/celtic_cliff_02_b_noskirt.cfg",
            CliffClass::Cliff,
            54,
        ),
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_cliff_02/celtic_cliff_02_h_noskirt.cfg",
            CliffClass::Cliff,
            13,
        ),
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_cliff_02/celtic_cliff_02_e_noskirt.cfg",
            CliffClass::Cliff,
            12,
        ),
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_mountain_element_01/celtic_mountain_element_01.cfg",
            CliffClass::Boulder,
            618,
        ),
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_rock_01/celtic_rock_01_b.cfg",
            CliffClass::Boulder,
            601,
        ),
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_rock_01/celtic_rock_01_a.cfg",
            CliffClass::Boulder,
            528,
        ),
        (
            "data/base/graphics/celtic/environment/props/rocks/celtic_rock_01/celtic_rock_01_d.cfg",
            CliffClass::Boulder,
            269,
        ),
    ],
    cliff_relief: 12.,
    spacing: 0.75,
};

// Census of the 43 Egypt pool islands: walls are `egypt_cliff_04`
// (4.5-5.7 m radius), rocks the smaller `egypt_cliff_03` (3.5-4 m).
// `egypt_rock_01`, by far the most placed, is under 1.3 m: scatter, not a
// piece auto cliffs stand on a face.
// TODO: cliff_relief and spacing are Latium's; Egypt's steep ground isn't
// measured yet.
pub const EGYPT: CliffSet = CliffSet {
    pieces: &[
        (
            "data/dlc03/graphics/egypt/environment/props/rocks/egypt_cliff_04/egypt_cliff_04_c.cfg",
            CliffClass::Cliff,
            1792,
        ),
        (
            "data/dlc03/graphics/egypt/environment/props/rocks/egypt_cliff_04/egypt_cliff_04_b.cfg",
            CliffClass::Cliff,
            1066,
        ),
        (
            "data/dlc03/graphics/egypt/environment/props/rocks/egypt_cliff_04/egypt_cliff_04_a.cfg",
            CliffClass::Cliff,
            522,
        ),
        (
            "data/dlc03/graphics/egypt/environment/props/rocks/egypt_cliff_03/egypt_cliff_03_b.cfg",
            CliffClass::Boulder,
            2807,
        ),
        (
            "data/dlc03/graphics/egypt/environment/props/rocks/egypt_cliff_03/egypt_cliff_03_c.cfg",
            CliffClass::Boulder,
            1171,
        ),
        (
            "data/dlc03/graphics/egypt/environment/props/rocks/egypt_cliff_03/egypt_cliff_03_a.cfg",
            CliffClass::Boulder,
            1072,
        ),
    ],
    cliff_relief: 8.,
    spacing: 1.,
};
