// Which assets play Anno 117's harbour, slot and water pieces on its
// shipped islands, and how its work areas stand by their slots.
use anno_core::harbour::{Piece, RoadShape};

/// How a work area playing `kind` (None when unknown) stands by its slot.
///
/// Measured on all 93 117 islands with linked work areas, 2026-11-22
/// (yaws as medians, spread as the 25th to 75th percentile around them):
///
/// | kind     | n   | yaw off the slot's direction | side off the slot's yaw | span   |
/// |----------|-----|------------------------------|-------------------------|--------|
/// | mountain | 777 | -6.6° (-15 .. +2)            | 9.1° (+4 .. +16)        | 2.55 m |
/// | river    | 213 | 171.7° (±1)                  | -81.7° (±1)             | 2.63 m |
///
/// Mountain work areas (Roman, Celtic, Egyptian) stand in front of their
/// slot, which faces up the slope, so they sit uphill of it, not down.
/// River ones turn 90° from their slot and stand on its river side.
pub fn road_shape(kind: Option<Piece>) -> RoadShape {
    // Two-point paths from the work area to the slot, with handles a
    // sixth of the span.
    let link = RoadShape {
        span: 0.,
        side: 0.,
        facing: 0.,
        start: [0.; 3],
        spacing: 0.,
        clearance: 0.,
        handle: 1. / 6.,
    };
    match kind {
        Some(Piece::RiverWorkArea) => RoadShape {
            span: 2.63,
            side: -81.7f32.to_radians(),
            facing: 171.7f32.to_radians(),
            ..link
        },
        _ => RoadShape {
            span: 2.55,
            side: 9.1f32.to_radians(),
            facing: -6.6f32.to_radians(),
            ..link
        },
    }
}

/// Which piece an asset plays on 117's islands, by its template and
/// internal name (`Islands::piece_of`). The names are the ones its assets
/// ship (`Slot Roman Mountain`, `Fixed Slot Celtic Mountain`, `Workareaslot
/// Roman River`, `Position Marker Latium Player Islands`).
pub fn piece_of(template: &str, name: &str) -> Option<Piece> {
    Piece::ALL.into_iter().find(|&p| plays(p, template, name))
}

fn plays(piece: Piece, template: &str, name: &str) -> bool {
    let fixed = name.contains("Fixed");
    match piece {
        Piece::Coast => template == "Coastline" && name == "Coastline",
        Piece::StartCoast => template == "Coastline" && name.contains("StartCoast"),
        Piece::Marker => template == "PositionMarker" && name.contains("Player Islands"),
        Piece::MineSlot => template == "Slot" && name.contains("Mountain") && !fixed,
        Piece::FixedMineSlot => template == "Slot" && name.contains("Mountain") && fixed,
        Piece::MineWorkArea => {
            template == "WorkAreaSlot" && name.to_ascii_lowercase().contains("mountain")
        }
        Piece::RiverSlot => template == "Slot" && name.contains("River") && !fixed,
        Piece::FixedRiverSlot => template == "Slot" && name.contains("River") && fixed,
        Piece::RiverWorkArea => template == "WorkAreaSlot" && name.contains("River"),
        Piece::MarshSlot => template == "Slot_Marsh",
        Piece::River => template == "River",
        Piece::Lake => template == "Lake",
        // 1800's alone.
        Piece::CaveWorkArea
        | Piece::ClaySlot
        | Piece::OilSlot
        | Piece::GasSlot
        | Piece::NewWorldMineSlot
        | Piece::NewWorldMineWorkArea => false,
    }
}
