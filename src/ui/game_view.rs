// Owns the game view: other screens drawn around the open one, as the game
// stacks them, and an in-game reference screenshot under the canvas to
// compare against.
//
// How the game places a screen: the window is split into one zone per
// player; each zone has its whole rectangle, the largest centred 16:9
// rectangle, the largest centred 64:27 (21:9) one, and that one grown by
// a sixth of its height above and below. Every screen picks one of them by
// a viewport type compiled into its C++ class (not in the asset), and its
// root is always sized 1920×1080 inside it. At a 1920×1080 window all of
// those rectangles are the screen, so screens stack at identity, which is
// what this view does.
// TODO(alex): other window sizes need the per-screen viewport types.
use super::{GENERATED, UiEditor, canvas::Node, scene, states};
use crate::editor::Editor;
use anno_formats::phx::Asset;
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    texture::Texture,
    theme,
    widgets::{Tooltip, button, section},
};
use std::sync::Arc;

/// The in-game shot the view compares against: 1920×1080, HUD only.
const REFERENCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../docs/reference/ingame-hud-1920x1080.png"
);

/// Screens on top of the island in the reference shot. Their contents that
/// game code spawns (resource and construction bar items, notifications)
/// are not in the files and stay empty. QuickNavigation (the province
/// overlay) and QuickTools (the gamepad radial) are not open there.
pub const HUD: [&str; 7] = [
    "hudgovernorsbarscene",
    "resourcebarscene",
    "questtrackerscene",
    // The left button column (the archives' button panel).
    "hudarchivesscene",
    "minimapscene",
    "constructionbarscene",
    // The bottom bar: category tabs, building buttons and tools.
    "constructionmenuscene",
];

/// Opacities the reference button steps through.
const OPACITIES: [f32; 4] = [0.25, 0.5, 0.75, 1.];

pub struct Screen {
    pub path: String,
    pub name: String,
    pub nodes: Vec<Node>,
}

#[derive(Default)]
pub struct GameView {
    /// Other screens, bottom to top; drawn under the open file.
    pub screens: Vec<Screen>,
    reference: Option<Arc<RenderImage>>,
    pub show_reference: bool,
    pub opacity: f32,
}

/// The shot lives in the source tree's docs/, not in shipped builds (it is
/// game art), so the button only shows where the file exists. Checked once.
fn reference_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| std::path::Path::new(REFERENCE).is_file())
}

fn load_reference() -> Result<Arc<RenderImage>> {
    let image = image::open(REFERENCE)
        .with_context(|| format!("reading {REFERENCE}"))?
        .to_rgba8();
    let (w, h) = image.dimensions();
    Ok(Texture::from_rgba(w, h, image.into_raw())?.render_image())
}

impl UiEditor {
    /// Lays out another screen for the game view.
    fn layout_screen(&mut self, path: &str) -> Result<Screen> {
        let asset = Asset::parse(self.install.read(path)?)?;
        let (mut nodes, _) = scene::layout(
            &asset,
            &self.install,
            &mut self.images,
            &self.texts,
            scene::View {
                design: self.canvas.design_view,
                bindings: self.bindings,
                play: None,
                preview: self.preview.values.clone(),
                shown: self.shown,
                sims: Vec::new(),
                sample: self.sample,
                edit: None,
                stale: Vec::new(),
            },
            |c| format!("{c:08x}"),
        );
        // Placeholders for game text help in the file being edited; around
        // it they are noise.
        for n in &mut nodes {
            if n.text.as_deref() == Some(scene::BOUND_TEXT) {
                n.visible = false;
            }
        }
        let groups = if self.bindings {
            Vec::new()
        } else {
            states::groups(&nodes)
        };
        states::apply(&mut nodes, &groups, &Default::default(), self.show_helpers);
        Ok(Screen {
            path: path.to_owned(),
            name: nodes
                .first()
                .map_or_else(|| path.to_owned(), |n| n.name.to_string()),
            nodes,
        })
    }

    /// Adds `path` to the game view; adding it again removes it.
    pub fn toggle_screen(&mut self, path: &str) -> Result<String> {
        if path == self.path {
            return Ok("That is the open file".into());
        }
        if let Some(i) = self.game.screens.iter().position(|s| s.path == path) {
            let s = self.game.screens.remove(i);
            return Ok(format!("Game view: removed {}", s.name));
        }
        let screen = self.layout_screen(path)?;
        let msg = format!("Game view: added {}", screen.name);
        self.game.screens.push(screen);
        Ok(msg)
    }

    /// Stacks the screens of the in-game HUD around the open file.
    pub fn add_hud(&mut self) -> Result<String> {
        let mut added = 0;
        for name in HUD {
            let path = format!("{GENERATED}{name}");
            if path == self.path || self.game.screens.iter().any(|s| s.path == path) {
                continue;
            }
            if let Ok(s) = self.layout_screen(&path) {
                self.game.screens.push(s);
                added += 1;
            }
        }
        Ok(format!("Game view: {added} HUD screens added"))
    }

    /// Lays the stacked screens out again (design view or helpers changed).
    pub(super) fn relayout_screens(&mut self) {
        let paths: Vec<String> = self.game.screens.iter().map(|s| s.path.clone()).collect();
        self.game.screens = paths
            .iter()
            .filter_map(|p| self.layout_screen(p).ok())
            .collect();
    }

    pub fn toggle_reference(&mut self) -> Result<String> {
        if self.game.reference.is_none() {
            self.game.reference = Some(load_reference()?);
        }
        if self.game.opacity == 0. {
            self.game.opacity = 0.5;
        }
        self.game.show_reference = !self.game.show_reference;
        Ok(String::new())
    }

    /// The reference screenshot over the page, then the other screens.
    pub(super) fn game_view_elements(&self) -> Vec<AnyElement> {
        let view = &self.canvas.view;
        let mut out = Vec::new();
        if self.game.show_reference
            && let Some(image) = &self.game.reference
        {
            let p = view.to_screen(0., 0.);
            out.push(
                img(image.clone())
                    .absolute()
                    .left(p.x)
                    .top(p.y)
                    .w(px(1920. * view.zoom))
                    .h(px(1080. * view.zoom))
                    .opacity(self.game.opacity)
                    .object_fit(ObjectFit::Fill)
                    .into_any_element(),
            );
        }
        // The game draws nothing outside the window: other screens are cut
        // to the page.
        let p = view.to_screen(0., 0.);
        let mut page = div()
            .absolute()
            .left(p.x)
            .top(p.y)
            .w(px(view.size[0] * view.zoom))
            .h(px(view.size[1] * view.zoom))
            .overflow_hidden();
        let mut inner = div().absolute().left(-p.x).top(-p.y).size_full();
        let region = self.visible_region();
        for s in &self.game.screens {
            inner = inner.children(
                super::canvas::render(
                    &self.canvas,
                    &s.nodes,
                    &super::Bits::default(),
                    None,
                    region,
                    &self.gpu,
                    None::<fn(usize, _, _, &mut Window, &mut App)>,
                )
                .0,
            );
        }
        page = page.child(inner);
        out.push(page.into_any_element());
        out
    }

    /// The canvas's game view bar: HUD, reference shot and its opacity.
    pub(super) fn game_view_bar(&self, cx: &mut Context<Editor>) -> Stateful<Div> {
        let act = |f: fn(&mut UiEditor) -> Result<String>| {
            cx.listener(move |this: &mut Editor, _: &ClickEvent, _, cx| {
                if let Some(ui) = &mut this.ui {
                    let r = f(ui);
                    match r {
                        Ok(m) if m.is_empty() => {}
                        Ok(m) => this.message(m, false),
                        Err(e) => this.message(format!("{e:#}"), true),
                    }
                }
                cx.notify();
            })
        };
        let hud_on = !self.game.screens.is_empty();
        div()
            .id("game-view-bar")
            .absolute()
            .top_1()
            .left_2()
            .flex()
            .gap_1()
            .text_size(px(11.))
            // Clicks on the bar must not select the widgets under it. It
            // occludes the pane's own press capture, so it takes the keys
            // for the canvas itself (round 11: Show hidden left Position X
            // typing, and 123 became 1234).
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.keys_to(super::Keys::Canvas);
                    cx.stop_propagation();
                }),
            )
            .child(
                gpui_editor_kit::widgets::overlay_toggle("gv-hud", "Game HUD", hud_on)
                    .tooltip(Tooltip::text(
                        "Game view",
                        "",
                        "Stack the in-game HUD screens around this one at 1920×1080, as the game does. Shift-click in UI files adds any screen.",
                    ))
                    .on_click(act(|ui| {
                        if ui.game.screens.is_empty() {
                            ui.add_hud()
                        } else {
                            ui.game.screens.clear();
                            Ok(String::new())
                        }
                    })),
            )
            .when(reference_available(), |d| {
                d.child(
                    gpui_editor_kit::widgets::overlay_toggle(
                        "gv-reference",
                        "Game screenshot",
                        self.game.show_reference,
                    )
                    .tooltip(Tooltip::text(
                        "In-game reference",
                        "",
                        "An in-game screenshot at 1920×1080 under the page, to compare positions",
                    ))
                    .on_click(act(UiEditor::toggle_reference)),
                )
            })
            .child(
                gpui_editor_kit::widgets::overlay_toggle(
                    "gv-hidden",
                    "Show hidden",
                    self.canvas.show_hidden,
                )
                    .tooltip(Tooltip::text(
                        "Hidden widgets",
                        "",
                        "Outline widgets that do not draw (hidden, transparent or masked away), so they can be found",
                    ))
                    .on_click(act(|ui| {
                        ui.canvas.show_hidden = !ui.canvas.show_hidden;
                        Ok(String::new())
                    })),
            )
            .child(
                gpui_editor_kit::widgets::overlay_toggle("gv-interact", "Interact", self.interact)
                    .tooltip(Tooltip::text(
                        "Interact",
                        "",
                        "Hover and press widgets as a player would: buttons play their Enter and Press clips. Editing is paused.",
                    ))
                    .on_click(act(|ui| {
                        ui.interact = !ui.interact;
                        if !ui.interact {
                            ui.interact_at(None, super::interact::State::Hover);
                        }
                        Ok(String::new())
                    })),
            )
            .when(self.game.show_reference, |d| {
                d.child(
                    button(format!("{:.0}%", self.game.opacity * 100.), false)
                        .bg(theme::get().overlay)
                        .on_click(act(|ui| {
                            let i = OPACITIES
                                .iter()
                                .position(|&o| o >= ui.game.opacity - 1e-3)
                                .map_or(0, |i| (i + 1) % OPACITIES.len());
                            ui.game.opacity = OPACITIES[i];
                            Ok(String::new())
                        })),
                )
            })
    }
}

impl Editor {
    /// The inspector's list of stacked screens.
    pub(super) fn game_view_section(&self, cx: &mut Context<Self>) -> Option<Div> {
        let ui = self.ui.as_ref()?;
        if ui.game.screens.is_empty() {
            return None;
        }
        let mut d = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section(&format!("Game view ({})", ui.game.screens.len())));
        for s in &ui.game.screens {
            let path = s.path.clone();
            let visible = s
                .nodes
                .iter()
                .filter(|n| n.visible && n.image.is_some())
                .count();
            d = d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_size(px(11.)).child(s.name.clone()))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme::get().muted)
                            .child(if visible == 0 {
                                "filled at runtime".to_owned()
                            } else {
                                format!("{visible} images")
                            }),
                    )
                    .child(button("Open", false).on_click(cx.listener({
                        let path = path.clone();
                        move |this, _, _, cx| {
                            this.open_ui_asset(&path, cx);
                            cx.notify();
                        }
                    })))
                    .child(
                        button("×", false).on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(ui) = &mut this.ui {
                                let _ = ui.toggle_screen(&path);
                            }
                            cx.notify();
                        })),
                    ),
            );
        }
        Some(d)
    }
}
