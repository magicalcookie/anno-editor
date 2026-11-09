// Owns simulated input on the canvas: in Interact mode, the widget under
// the pointer is hovered and a held button presses it. Buttons show their
// states through timeline clips with conventional names (Enter 137 and
// Leave 145 in the install, Press 65, Release 58, Select, Disabled…), so
// a state is the matching clip held at its end on that button's template
// instance (`layout::Sim`). The search walks outward from the innermost
// template, so a plain image inside a button template still hovers it.
use super::{UiEditor, timeline::ClipRef};
use anno_formats::phx::{Guid, layout::Sim};
use gpui_editor_kit::gpui::{Pixels, Point};

/// Clip names per state, in preference order (shipped spellings).
const HOVER: [&str; 7] = [
    "Enter",
    "MouseEnter",
    "OnMouseEnter",
    "Anim_Enter",
    "OverState",
    "Hover",
    "SelectEnter",
];
const PRESS: [&str; 6] = [
    "Press",
    "OnMouseDown",
    "Pressed",
    "Anim_Press",
    "SelectedPress",
    "MouseDown",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Hover,
    Press,
}

fn find(clips: &[ClipRef], file: Option<Guid>, names: &[&str]) -> Option<usize> {
    names.iter().find_map(|want| {
        clips
            .iter()
            // An empty clip (player cards keep a trackless Enter) leaves
            // the state to the next template out.
            .find(|c| c.file == file && c.name.eq_ignore_ascii_case(want) && !c.lanes.is_empty())
            .map(|c| c.index)
    })
}

impl UiEditor {
    /// The state `state` for the widget at `at` (window pixels): its
    /// innermost template instance that has a matching clip, else the open
    /// file's own clip.
    pub(super) fn sim_at(&self, at: Point<Pixels>, state: State) -> Option<Sim> {
        let names: &[&str] = match state {
            State::Hover => &HOVER,
            State::Press => &PRESS,
        };
        let p = self.canvas.view.to_design(at - self.canvas_bounds.origin);
        let clips = &self.player.clips;
        // Only what reacts takes the pointer, as in game: decoration over a
        // button (the minimap's "N", a glow) lets it through to the button.
        self.hits(p)
            .find_map(|(_, node)| {
                node.owners.iter().find_map(|&(g, inst)| {
                    find(clips, Some(g), names).map(|clip| Sim {
                        file: Some(g),
                        instance: Some(inst),
                        clip,
                    })
                })
            })
            .or_else(|| {
                find(clips, None, names).map(|clip| Sim {
                    file: None,
                    instance: None,
                    clip,
                })
            })
    }

    /// The nodes that draw something at design point `p`, topmost first: an
    /// image where it is not transparent, a label in its box. Bounding
    /// boxes are not enough: a turned minimap wedge or a round stroke
    /// covers far more than it draws, and containers draw nothing.
    pub(super) fn hits(&self, p: [f32; 2]) -> impl Iterator<Item = (usize, &super::canvas::Node)> {
        self.nodes
            .iter()
            .enumerate()
            .rev()
            .filter_map(move |(i, n)| {
                if !n.visible || n.opacity < 0.01 {
                    return None;
                }
                if let Some([x, y, w, h]) = n.clip
                    && !(p[0] >= x && p[1] >= y && p[0] < x + w && p[1] < y + h)
                {
                    return None;
                }
                let [x, y, w, h] = n.rect;
                let inside =
                    w > 0. && h > 0. && p[0] >= x && p[1] >= y && p[0] < x + w && p[1] < y + h;
                let hit = match (&n.sprite, &n.image, &n.text) {
                    (Some(s), ..) => s.alpha_at(p) > 0.1,
                    // Composited on the CPU into exactly its rect: its own
                    // pixels say where it shows.
                    (None, Some(image), _) if inside => {
                        let size = image.size(0);
                        let (iw, ih) = (size.width.0 as usize, size.height.0 as usize);
                        let ix = (((p[0] - x) / w * iw as f32) as usize).min(iw.saturating_sub(1));
                        let iy = (((p[1] - y) / h * ih as f32) as usize).min(ih.saturating_sub(1));
                        image
                            .as_bytes(0)
                            .and_then(|b| b.get((iy * iw + ix) * 4 + 3).copied())
                            .is_some_and(|a| a > 25)
                    }
                    (None, None, Some(_)) => inside,
                    _ => false,
                };
                hit.then_some((i, n))
            })
    }

    /// Puts the widget at `at` into `state` (None: nothing hovered).
    /// Returns whether the canvas changed.
    pub fn interact_at(&mut self, at: Option<Point<Pixels>>, state: State) -> bool {
        let sims: Vec<Sim> = at.and_then(|a| self.sim_at(a, state)).into_iter().collect();
        if sims == self.sims {
            return false;
        }
        // Say which clip now holds, so the state is visible even when it
        // only changes something subtle.
        if let Some(s) = sims.first()
            && let Some(c) = self
                .player
                .clips
                .iter()
                .find(|c| c.file == s.file && c.index == s.clip)
        {
            self.message = Some((format!("{} · {}", c.name, c.owner), false));
        }
        self.sims = sims;
        self.relayout();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::State;
    use crate::ui::UiEditor;
    use anno_formats::vfs::Install;
    use std::sync::Arc;

    // Needs the game: hovering a main-title menu button holds its Enter
    // clip on that one instance, which changes how the button draws.
    #[test]
    fn hovering_a_button_applies_its_enter_clip() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Arc::new(Install::open(dir).unwrap());
        let mut ui = UiEditor::load(install, "data/ui/studio/generated/maintitlescene").unwrap();
        let looks = |ui: &UiEditor| {
            ui.nodes
                .iter()
                .map(|n| {
                    (
                        n.visible,
                        (n.opacity * 100.) as i32,
                        n.color,
                        n.rect.map(|v| v as i32),
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = looks(&ui);
        // A visible widget inside a template that has an Enter clip, and
        // the top one where it is (another button may cover it).
        let candidates: Vec<usize> = (0..ui.nodes.len())
            .filter(|&i| {
                let n = &ui.nodes[i];
                n.visible
                    && n.rect[2] > 4.
                    && n.rect[3] > 4.
                    && n.owners.iter().any(|&(g, _)| {
                        ui.player.clips.iter().any(|c| {
                            c.file == Some(g)
                                && c.name.eq_ignore_ascii_case("Enter")
                                && !c.lanes.is_empty()
                        })
                    })
            })
            .collect();
        assert!(
            !candidates.is_empty(),
            "no hoverable button on the main title"
        );
        let hovered = candidates.into_iter().any(|node| {
            let [x, y, w, h] = ui.nodes[node].rect;
            let at = ui.canvas.view.to_screen(x + w / 2., y + h / 2.);
            ui.interact_at(Some(at), State::Hover)
        });
        assert!(hovered, "nothing hovered");
        assert_eq!(ui.sims.len(), 1);
        assert_ne!(looks(&ui), before, "the Enter clip changed nothing");
        assert!(ui.interact_at(None, State::Hover));
        assert_eq!(looks(&ui), before);
    }

    // Needs the game: on the minimap ring, the pointer over a button's icon
    // hovers that button's own sector. The ring's turned wedges and round
    // strokes overlap far beyond what they draw; picking by bounding box
    // hovered whichever was on top, sectors away from the pointer.
    #[test]
    fn hovering_a_minimap_button_picks_its_own_sector() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skipping: ANNO117_DIR unset");
            return;
        };
        let install = Arc::new(Install::open(dir).unwrap());
        let ui = UiEditor::load(install, "data/ui/studio/generated/minimapscene").unwrap();
        // A ring entry is an instance of the sector template.
        let entry = |owners: &[(anno_formats::phx::Guid, u64)]| {
            owners
                .iter()
                .find(|o| o.0.to_string().starts_with("620a61e2"))
                .map(|o| o.1)
        };
        let mut buttons = 0;
        for icon in ui
            .nodes
            .iter()
            .filter(|n| *n.name == *"Image_" && n.visible)
        {
            let [x, y, w, h] = icon.rect;
            let at = ui.canvas.view.to_screen(x + w / 2., y + h / 2.);
            let Some(sim) = ui.sim_at(at, State::Hover) else {
                // Fertilities are information, not buttons.
                continue;
            };
            let hovered = ui
                .nodes
                .iter()
                .find(|n| n.owners.iter().any(|o| Some(o.1) == sim.instance))
                .and_then(|n| entry(&n.owners));
            assert_eq!(hovered, entry(&icon.owners), "icon at {:?}", icon.rect);
            buttons += 1;
        }
        assert_eq!(buttons, 10);
    }
}
