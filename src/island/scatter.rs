// The island's scatter brush: which props the set draws from, turning the
// kit's placements into rd3d prop-grid instances on the terrain, and marking
// the RegrowManager tree cells under trees.
use super::doc::{self, Doc, Prop};
use anno_formats::{grids::Kind, island};
use gpui_editor_kit::{heightfield::Brush, scatter};
use std::sync::Arc;

#[derive(Default)]
pub struct Scatter {
    /// Prop file indices drawn from at random.
    pub files: Vec<u32>,
    /// Density and scale variation.
    pub spread: scatter::Scatter,
    /// Set `Trees` grid cells under new props (and clear them on erase).
    pub mark_trees: bool,
}

impl Scatter {
    /// Adds props under the brush toward the density. Returns how many.
    pub fn fill(
        &mut self,
        doc: &mut Doc,
        center: [f32; 2],
        brush: &Brush,
        next_id: &mut u64,
    ) -> usize {
        let set = doc
            .props
            .iter()
            .filter(|p| self.files.contains(&p.source.file))
            .map(|p| {
                let t = gpui_editor_kit::math::translation(&p.matrix);
                [t[0], t[2]]
            });
        // Props below sea level would sit in the water.
        let field = &doc.field;
        let placed = self
            .spread
            .fill(center, brush, set, self.files.len(), |at| {
                field.height(at) >= 0.1
            });
        for p in &placed {
            let mut source = template(doc, self.files[p.choice]);
            source.position = [p.at[0], doc.field.height(p.at), p.at[1]];
            source.rotation = [0., (p.yaw / 2.).sin(), 0., (p.yaw / 2.).cos()];
            source.scale = source.scale.map(|v| v * p.scale);
            Arc::make_mut(&mut doc.props).push(Prop {
                id: *next_id,
                matrix: doc::trs(source.position, source.rotation, source.scale),
                source,
                auto_cliff: false,
            });
            *next_id += 1;
            if self.mark_trees {
                set_tree(doc, p.at, 1);
            }
        }
        placed.len()
    }

    /// Removes props of the set (any prop when the set is empty) under the
    /// brush, each with chance weight × strength. Returns how many.
    pub fn thin(&mut self, doc: &mut Doc, center: [f32; 2], brush: &Brush) -> usize {
        let mut removed = Vec::new();
        Arc::make_mut(&mut doc.props).retain(|p| {
            if !self.files.is_empty() && !self.files.contains(&p.source.file) {
                return true;
            }
            let t = gpui_editor_kit::math::translation(&p.matrix);
            let at = [t[0], t[2]];
            let remove = self.spread.thin(center, brush, at);
            if remove {
                removed.push(at);
            }
            !remove
        });
        if self.mark_trees {
            for at in &removed {
                set_tree(doc, *at, 0);
            }
        }
        removed.len()
    }
}

/// Settings for a new prop of `file`: copied from an existing instance (tint
/// color, terrain adaption), else plain defaults.
pub(super) fn template(doc: &Doc, file: u32) -> island::Prop {
    doc.props
        .iter()
        .find(|p| p.source.file == file)
        .map(|p| p.source.clone())
        .unwrap_or(island::Prop {
            file,
            position: [0.; 3],
            rotation: [0., 0., 0., 1.],
            scale: [1.; 3],
            color: [1.; 4],
            adapt_terrain_height: Some(1),
        })
}

fn set_tree(doc: &mut Doc, at: [f32; 2], value: u32) {
    let Some(grids) = &mut doc.grids else { return };
    let grids = Arc::make_mut(grids);
    if let Some(layer) = grids.layer_mut(Kind::Trees)
        && let Some((x, y)) = layer.cell_at(at[0], at[1])
    {
        layer.set(x, y, value);
    }
}
