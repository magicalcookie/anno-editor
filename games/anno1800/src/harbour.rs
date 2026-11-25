// Anno 1800's harbour pieces per region, which assets play them, and its
// mine rails, measured on its shipped islands.
use anno_core::harbour::{Piece, RoadShape};

/// The pieces a region's library needs before it stops reading islands.
pub fn required_pieces(region: &str) -> &'static [Piece] {
    match region {
        "Moderate" => &[
            Piece::Coast,
            Piece::MineSlot,
            Piece::MineWorkArea,
            Piece::CaveWorkArea,
            Piece::ClaySlot,
            Piece::OilSlot,
            Piece::River,
            Piece::Lake,
        ],
        "Colony01" => &[
            Piece::Coast,
            Piece::MineSlot,
            Piece::MineWorkArea,
            Piece::ClaySlot,
            Piece::OilSlot,
            Piece::NewWorldMineSlot,
            Piece::NewWorldMineWorkArea,
            Piece::River,
            Piece::Lake,
        ],
        "Arctic" => &[
            Piece::Coast,
            Piece::MineSlot,
            Piece::MineWorkArea,
            Piece::GasSlot,
        ],
        "Africa" => &[
            Piece::Coast,
            Piece::RiverSlot,
            Piece::RiverWorkArea,
            Piece::River,
            Piece::Lake,
        ],
        _ => &[Piece::Coast],
    }
}

/// The rail for a work area playing `kind` (None when unknown).
///
/// Measured on all 145 1800 islands with linked work areas, 2026-11-22
/// (yaws as medians, spread as the 25th to 75th percentile around them):
///
/// | kind     | n   | yaw off the slot's direction | side off the slot's yaw | span   |
/// |----------|-----|------------------------------|-------------------------|--------|
/// | mountain | 995 | -5.5° (-14 .. +5)            | 2.3° (-16 .. +17)       | 11.4 m |
/// | cave     | 977 | 180° (169 .. 195)            | 2.9° (-4 .. +10)        | 5.3 m  |
/// | river    | 38  | -97.1° (±1)                  | -82.9° (±1)             | 8.06 m |
///
/// The New World's mountain work area (121) ships like the Old World's
/// (-2.8°, -2.4°, 10.9 m). Against the slope the yaws spread as much or
/// more, and the slot's frame holds the side far tighter than the slope
/// does (mountain ±27°, cave ±30°). The cave model's opening is on its -x
/// side, so it turns its back on the slot's direction to face it.
pub fn road_shape(kind: Option<Piece>) -> RoadShape {
    // 1800 rails: points 1.6 m apart (median), nearly straight (1.02
    // to 1.06 times the chord). The first point sits on the model's
    // `mining_mountain` dummy, the last at the slot's height, and inner
    // ones 0.1–0.3 m off the straight ramp between them at the median
    // (0.6 m off the ground), rarely under ground + 0.2. The lowest
    // point of the track between them is a median 0.14 m over the
    // ground, and 32 of 2,113 dip under it. The records store
    // `TangentScale` 0.3.
    let rail = RoadShape {
        span: 0.,
        side: 0.,
        facing: 0.,
        start: [0.; 3],
        spacing: 1.6,
        clearance: 0.2,
        handle: 0.3,
    };
    match kind {
        // "slot mining workarea cave" (mining_01_slot_01): about level
        // with the slot, 4–6 points.
        Some(Piece::CaveWorkArea) => RoadShape {
            span: 5.3,
            side: 2.9f32.to_radians(),
            facing: 180f32.to_radians(),
            start: [1.30, 1.54, -0.36],
            ..rail
        },
        // "Riverslot_workarea" (DLC06): its channel leaves from the side.
        Some(Piece::RiverWorkArea) => RoadShape {
            span: 8.06,
            side: -82.9f32.to_radians(),
            facing: -97.1f32.to_radians(),
            start: [-0.15, 0.48, 2.64],
            ..rail
        },
        // "slot mining workarea mountain" (mining_02_slot_01) and the
        // rest: 7–21 m up the hill, 4.6 m higher, 6–10 points. The rail
        // starts behind the model's centre and runs through it.
        _ => RoadShape {
            span: 11.4,
            side: 2.3f32.to_radians(),
            facing: -5.5f32.to_radians(),
            start: [-0.62, 0.18, 0.],
            ..rail
        },
    }
}

/// Which piece an asset plays on 1800's islands, by its template and
/// internal name (`Islands::piece_of`). The names are the ones its assets
/// ship; it has no marker, start coast, fixed river or marsh slot.
pub fn piece_of(template: &str, name: &str) -> Option<Piece> {
    Piece::ALL.into_iter().find(|&p| plays(p, template, name))
}

fn plays(piece: Piece, template: &str, name: &str) -> bool {
    match piece {
        Piece::Coast => template == "Coastline" && name == "Coastline",
        Piece::MineSlot => {
            template == "Slot" && matches!(name, "random slot mining" | "random slot mining arctic")
        }
        Piece::FixedMineSlot => template == "Slot" && name.contains("(Coal Mine Slot)"),
        // `slot mining workarea mountain`; the New World's has its own piece.
        Piece::MineWorkArea => {
            template == "WorkAreaSlot"
                && name.to_ascii_lowercase().contains("mountain")
                && !name.contains("season4")
        }
        Piece::CaveWorkArea => template == "WorkAreaSlot" && name == "slot mining workarea cave",
        Piece::ClaySlot => template == "Slot" && name == "random slot clay",
        Piece::OilSlot => template == "Slot" && name == "random slot oil pump",
        Piece::GasSlot => template == "Slot" && name == "random slot gas arctic",
        Piece::NewWorldMineSlot => {
            template == "Slot" && name == "season4 random mining slot colony01"
        }
        Piece::NewWorldMineWorkArea => {
            template == "WorkAreaSlot" && name == "season4 slot mining workarea mountain"
        }
        // `Riverslot`, `Oasis_Riverslot`.
        Piece::RiverSlot => template == "Slot" && name.contains("River"),
        Piece::RiverWorkArea => template == "WorkAreaRiverSlot",
        Piece::River => template == "River",
        Piece::Lake => template == "Lake",
        Piece::StartCoast | Piece::Marker | Piece::FixedRiverSlot | Piece::MarshSlot => false,
    }
}
