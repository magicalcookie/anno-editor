// Painting the island's gameplay grids (water, river, trees, environment,
// grit detail …) and the overlay colors that show one of them on the ground.
// Storage and save live in `anno_formats::grids`.
use anno_formats::{
    game::Game,
    grids::{Kind, Layer, MARSH},
};
use gpui_editor_kit::heightfield::Brush;

/// Layers the Grid tool offers, in picker order. Water infos follow the
/// heights on save (`anno_formats::water`); fog is undecoded.
pub const PAINTABLE: [Kind; 12] = [
    Kind::AreaIds,
    Kind::Water,
    Kind::River,
    Kind::Ford,
    Kind::ActiveMap,
    Kind::Trees,
    Kind::Herds,
    Kind::ForestChunks,
    Kind::Environment,
    Kind::Marsh,
    Kind::Grit,
    Kind::WaterFlow,
];

/// Layers the scene panel can show over the ground: the paintable ones,
/// then derived ones.
pub const OVERLAYS: [Kind; 12] = [
    Kind::Water,
    Kind::River,
    Kind::Ford,
    Kind::ActiveMap,
    Kind::AreaIds,
    Kind::Trees,
    Kind::Herds,
    Kind::ForestChunks,
    Kind::Environment,
    Kind::Marsh,
    Kind::Grit,
    Kind::WaterFlow,
];

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::ActiveMap => "Active area",
        Kind::Water => "Land / sea",
        Kind::River => "River",
        Kind::Ford => "Ford",
        Kind::Trees => "Tree cells",
        Kind::ForestChunks => "Forest chunks",
        Kind::Herds => "Herds",
        Kind::Environment => "Environment",
        // Construction area vs base area: where players may build.
        Kind::AreaIds => "Buildable",
        Kind::Grit => "Detail (grit)",
        Kind::GritAux => "Grit aux",
        Kind::WaterInfos => "Water infos",
        Kind::WaterFlow => "Water flow",
        Kind::Fog => "Fog",
        Kind::Marsh => "Marsh (irrigation)",
    }
}

/// Continuous layers blend toward the value with brush weight; the rest are
/// classes and set whole cells.
pub fn is_continuous(kind: Kind) -> bool {
    matches!(kind, Kind::Grit)
}

/// The value range a picker offers.
pub fn max_value(game: Game, kind: Kind) -> u32 {
    match kind {
        Kind::Environment => game.profile().environment_types.len() as u32 - 1,
        Kind::ForestChunks | Kind::Grit => 255,
        Kind::Marsh => MARSH,
        _ => 1,
    }
}

/// Paints `value` into `layer` under the brush; `erase` paints 0. Class
/// layers set every cell whose centre is inside the radius and ignore the
/// falloff, so edges stay predictable on 1 m and 8 m grids.
pub fn paint(layer: &mut Layer, center: [f32; 2], brush: &Brush, value: u32, erase: bool) -> bool {
    let target = if erase { 0 } else { value };
    let cell = layer.cell_metres;
    let span = |c: f32, n: usize| {
        let lo = ((c - brush.radius) / cell).floor().max(0.) as usize;
        let hi = (((c + brush.radius) / cell).ceil().max(0.) as usize).min(n);
        lo..hi
    };
    let mut changed = false;
    for y in span(center[1], layer.height) {
        for x in span(center[0], layer.width) {
            let (wx, wz) = ((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
            let w = brush.weight((wx - center[0]).hypot(wz - center[1]));
            if w <= 0. {
                continue;
            }
            let old = layer.get(x, y);
            let new = if is_continuous(layer.kind) {
                let t = (w * brush.strength).clamp(0., 1.);
                (old as f32 + (target as f32 - old as f32) * t).round() as u32
            } else {
                target
            };
            if new != old {
                layer.set(x, y, new);
                changed = true;
            }
        }
    }
    changed
}

/// `WaterFlow` cells: bytes 0/1 are the flow x/z biased by 127, so still
/// water is `0x7f7f`. Shipped sea currents run at 10 or 20 per axis.
const FLOW_BIAS: f32 = 127.;
/// What a river and the flow brush write: as fast as the strongest shipped
/// sea current.
pub const FLOW_SPEED: f32 = 20.;

/// Flow vector of a `WaterFlow` word, x/z in the word's units.
pub fn flow(word: u32) -> [f32; 2] {
    [
        (word & 255) as f32 - FLOW_BIAS,
        ((word >> 8) & 255) as f32 - FLOW_BIAS,
    ]
}

/// `word` with its flow replaced by `v` (x/z), keeping byte 2 (a coast
/// mask, 0 on land) and byte 3.
pub fn with_flow(word: u32, v: [f32; 2]) -> u32 {
    let byte = |f: f32| (f + FLOW_BIAS).round().clamp(0., 255.) as u32;
    word & 0xffff_0000 | byte(v[1]) << 8 | byte(v[0])
}

/// Combs `WaterFlow` under the brush toward `dir` (a unit vector, the drag
/// direction) at [`FLOW_SPEED`], weighted by falloff and strength; `None`
/// calms it back to still water.
pub fn paint_flow(
    layer: &mut Layer,
    center: [f32; 2],
    brush: &Brush,
    dir: Option<[f32; 2]>,
) -> bool {
    let target = dir.map_or([0.; 2], |d| [d[0] * FLOW_SPEED, d[1] * FLOW_SPEED]);
    let cell = layer.cell_metres;
    let span = |c: f32, n: usize| {
        let lo = ((c - brush.radius) / cell).floor().max(0.) as usize;
        let hi = (((c + brush.radius) / cell).ceil().max(0.) as usize).min(n);
        lo..hi
    };
    let mut changed = false;
    for y in span(center[1], layer.height) {
        for x in span(center[0], layer.width) {
            let (wx, wz) = ((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
            let w = brush.weight((wx - center[0]).hypot(wz - center[1]));
            if w <= 0. {
                continue;
            }
            let t = (w * brush.strength).clamp(0., 1.);
            let old = layer.get(x, y);
            let [fx, fz] = flow(old);
            let new = with_flow(old, [fx + (target[0] - fx) * t, fz + (target[1] - fz) * t]);
            if new != old {
                layer.set(x, y, new);
                changed = true;
            }
        }
    }
    changed
}

/// Overlay color and opacity of a cell value; `None` leaves the ground as is.
pub fn overlay(game: Game, kind: Kind, value: u32) -> Option<([f32; 3], f32)> {
    let rgb = |c: u32| [(c >> 16) & 255, (c >> 8) & 255, c & 255].map(|v| v as f32 / 255.);
    if kind == Kind::WaterFlow {
        // Hue is the direction (east red, south green-yellow, …), opacity
        // the speed; still water shows nothing.
        let [x, z] = flow(value);
        let speed = x.hypot(z);
        if speed < 1. {
            return None;
        }
        let hue = z.atan2(x).rem_euclid(std::f32::consts::TAU);
        let channel = |o: f32| ((hue - o).cos() * 0.5 + 0.5).clamp(0., 1.);
        let tau3 = std::f32::consts::TAU / 3.;
        return Some((
            [channel(0.), channel(tau3), channel(2. * tau3)],
            0.3 + 0.5 * (speed / FLOW_SPEED).min(1.),
        ));
    }
    if value == 0 {
        return None;
    }
    let bit = |c: u32| Some((rgb(c), 0.6));
    match kind {
        Kind::ActiveMap => bit(crate::colors::LAYER_ACTIVE_MAP),
        Kind::Water => bit(crate::colors::LAYER_WATER),
        Kind::River => bit(crate::colors::LAYER_RIVER),
        Kind::Ford => bit(crate::colors::LAYER_FORD),
        Kind::Trees => bit(crate::colors::LAYER_TREES),
        Kind::Herds => bit(crate::colors::LAYER_HERDS),
        Kind::Marsh => bit(crate::colors::LAYER_MARSH),
        Kind::Environment => {
            let (_, argb) = game.profile().environment_types.get(value as usize)?;
            // Types without a debug color in assets.xml still need to show.
            let color = if *argb == 0 {
                crate::colors::LAYER_UNCOLORED
            } else {
                *argb
            };
            Some((rgb(color), 0.65))
        }
        Kind::AreaIds => match value {
            0x2001 => Some((rgb(crate::colors::AREA_CONSTRUCTION), 0.65)),
            0x0001 => Some((rgb(crate::colors::AREA_BASE), 0.35)),
            _ => Some((rgb(crate::colors::AREA_OTHER), 0.5)),
        },
        Kind::ForestChunks => {
            // Neighbouring chunk ids get far-apart hues.
            let h = (value.wrapping_mul(2_654_435_761) >> 8) & 0xffffff;
            Some((rgb(h | 0x404040), 0.6))
        }
        _ => Some(([1., 1., 1.], value as f32 / 255. * 0.8)),
    }
}

/// The layer's value under island metres `(x, z)`, 0 outside.
pub fn value_at(layer: &Layer, x: f32, z: f32) -> u32 {
    layer.cell_at(x, z).map_or(0, |(cx, cy)| layer.get(cx, cy))
}
