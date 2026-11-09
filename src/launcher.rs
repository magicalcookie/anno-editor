// Owns game selection and the editor's lifetime. Switching drops the old
// editor entity, so its asynchronous loads cannot land in another game.
use crate::editor::{Editor, SwitchGame};
use anno_formats::game::Game;
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    platform, widgets,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Settings {
    last_game: Option<Game>,
    directories: BTreeMap<String, PathBuf>,
}

impl Settings {
    fn load() -> Self {
        std::fs::read(platform::config_dir("anno-editor").join("games.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> anyhow::Result<()> {
        let config = platform::config_dir("anno-editor");
        std::fs::create_dir_all(&config)?;
        std::fs::write(config.join("games.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    fn directory(&self, game: Game, configured: Option<PathBuf>) -> Option<PathBuf> {
        configured
            .filter(|dir| Game::detect(dir) == Some(game))
            .or_else(|| {
                self.directories
                    .get(game.profile().steam_app_id)
                    .filter(|dir| Game::detect(dir) == Some(game))
                    .cloned()
            })
    }
}

/// Where `game` is installed: the environment variable, the folder the
/// launcher remembers, else the first one the launchers report. For
/// importing islands from the game not being edited.
pub fn install_dir(game: Game) -> Option<PathBuf> {
    Settings::load()
        .directory(game, game.environment_dir())
        .or_else(|| game.installations().into_iter().next())
}

struct Choice {
    game: Game,
    preferred: Option<PathBuf>,
    detected: Vec<PathBuf>,
    discovering: bool,
}

impl Choice {
    fn directory(&self) -> Option<&PathBuf> {
        self.preferred.as_ref().or_else(|| self.detected.first())
    }

    fn finish_search(&mut self, found: Choice) {
        // A folder chosen while discovery ran, including a CLI argument,
        // must not be replaced by a late launcher result.
        self.preferred = self.preferred.take().or(found.preferred);
        self.detected = found.detected;
        self.discovering = false;
    }
}

enum Screen {
    Picker,
    Editing {
        editor: Entity<Editor>,
        _subscription: Subscription,
    },
}

pub struct Launcher {
    choices: [Choice; 2],
    screen: Screen,
    selected: Game,
    open: Option<String>,
    error: Option<String>,
    focus: FocusHandle,
    settings: Settings,
    requested: Option<Game>,
    selecting: Option<Game>,
}

impl Launcher {
    pub fn new(
        game: Option<Game>,
        dir: Option<PathBuf>,
        open: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let settings = Settings::load();
        let mut launcher = Self {
            choices: Game::ALL.map(|game| Choice {
                game,
                preferred: None,
                detected: Vec::new(),
                discovering: true,
            }),
            screen: Screen::Picker,
            selected: game.or(settings.last_game).unwrap_or(Game::Anno1800),
            open,
            error: None,
            focus: cx.focus_handle(),
            settings,
            requested: None,
            selecting: None,
        };
        if let Some(game) = game {
            if let Some(dir) = dir {
                launcher.choice_mut(game).preferred = Some(dir);
            }
            launcher.choose(game, window, cx);
        }
        for game in Game::ALL {
            let settings = launcher.settings.clone();
            cx.spawn_in(window, async move |this, cx| {
                // A remembered or explicit folder can open before an
                // unrelated launcher library finishes responding.
                let preferred = cx
                    .background_executor()
                    .spawn(async move { settings.directory(game, game.environment_dir()) })
                    .await;
                if this
                    .update_in(cx, |this, window, cx| {
                        let choice = this.choice_mut(game);
                        choice.preferred = choice.preferred.take().or(preferred);
                        if this.requested == Some(game) && this.choice_mut(game).preferred.is_some()
                        {
                            this.choose(game, window, cx);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
                let found = cx
                    .background_executor()
                    .spawn(async move {
                        Choice {
                            game,
                            preferred: None,
                            detected: game.installations(),
                            discovering: false,
                        }
                    })
                    .await;
                this.update_in(cx, |this, window, cx| {
                    this.choice_mut(game).finish_search(found);
                    if this.requested == Some(game) {
                        this.choose(game, window, cx);
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        launcher
    }

    fn choice_mut(&mut self, game: Game) -> &mut Choice {
        self.choices
            .iter_mut()
            .find(|c| c.game == game)
            .expect("game has a card")
    }

    fn choose(&mut self, game: Game, window: &mut Window, cx: &mut Context<Self>) {
        self.requested = None;
        self.selected = game;
        let choice = self.choice_mut(game);
        if choice.preferred.is_none() {
            if choice.discovering {
                self.requested = Some(game);
                self.error = None;
                cx.notify();
                return;
            }
            if choice.detected.len() > 1 {
                self.selecting = Some(game);
                self.error = None;
                cx.notify();
                return;
            }
        }
        let Some(dir) = choice.directory().cloned() else {
            self.error = Some(format!(
                "{} was not found. Choose its installation folder below.",
                game.profile().title
            ));
            cx.notify();
            return;
        };
        if Game::detect(&dir) != Some(game) {
            self.error = Some(format!(
                "{} is no longer available. Choose its installation folder again.",
                dir.display()
            ));
            cx.notify();
            return;
        }
        self.settings.last_game = Some(game);
        self.settings
            .directories
            .insert(game.profile().steam_app_id.to_owned(), dir.clone());
        self.choice_mut(game).preferred = Some(dir.clone());
        if let Err(e) = self.settings.save() {
            eprintln!("Could not remember selected game: {e}");
        }
        self.error = None;
        let open = self.open.take();
        let config = platform::config_dir(game.profile().config_folder);
        let editor = cx.new(|cx| Editor::new(game, config, Some(dir), open, window, cx));
        let subscription = cx.subscribe(&editor, |this, _, _: &SwitchGame, cx| {
            this.screen = Screen::Picker;
            cx.notify();
        });
        self.screen = Screen::Editing {
            editor,
            _subscription: subscription,
        };
        cx.notify();
    }

    fn locate(&mut self, game: Game, cx: &mut Context<Self>) {
        self.requested = None;
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(format!("Choose the {} installation folder", game.profile().title).into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picker.await else { return; };
            let Some(dir) = paths.into_iter().next() else { return; };
            this.update(cx, |this, cx| {
                match Game::detect(&dir) {
                    Some(found) if found == game => {
                        this.settings.directories.insert(game.profile().steam_app_id.to_owned(), dir.clone());
                        this.choice_mut(game).preferred = Some(dir);
                        this.selecting = None;
                        this.selected = game;
                        this.error = this.settings.save().err().map(|e| format!("Folder selected, but it could not be remembered: {e:#}"));
                    }
                    _ => this.error = Some(format!("That folder is not a readable {} installation. It must contain the matching game executable and RDA archives in maindata.", game.profile().title)),
                }
                cx.notify();
            }).ok();
        }).detach();
    }

    fn installation_picker(&self, game: Game, cx: &mut Context<Self>) -> AnyElement {
        let l = &crate::colors::get().launcher;
        let choice = self
            .choices
            .iter()
            .find(|c| c.game == game)
            .expect("game has a card");
        let options = div()
            .id("installation-options")
            .flex()
            .flex_col()
            .gap_2()
            .max_h(px(360.))
            .overflow_y_scroll()
            .children(choice.detected.iter().enumerate().map(|(i, path)| {
                let path = path.clone();
                widgets::button(
                    path.display().to_string(),
                    choice.directory() == Some(&path),
                )
                .id(("installation", i))
                .w_full()
                .h_auto()
                .min_h(px(40.))
                .px_3()
                .py_2()
                .justify_start()
                .text_size(px(12.))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choice_mut(game).preferred = Some(path.clone());
                    this.selecting = None;
                    this.choose(game, window, cx);
                }))
            }));
        widgets::modal(
            "installations",
            widgets::dialog(format!("Choose an {} installation", game.profile().title))
                .w(px(800.))
                .max_w_full()
                .child("Several copies were found. Choose the installation to open.")
                .child(options)
                .when_some(self.error.clone(), |el, error| {
                    el.child(div().text_color(l.error).child(error))
                })
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(widgets::button("Cancel", false).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.selecting = None;
                                cx.notify();
                            },
                        )))
                        .child(widgets::button("Choose another folder…", false).on_click(
                            cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.locate(game, cx)
                            }),
                        )),
                ),
        )
        .into_any_element()
    }

    fn card(&self, choice: &Choice, art: f32, cx: &mut Context<Self>) -> AnyElement {
        let l = &crate::colors::get().launcher;
        let game = choice.game;
        let installed = choice.directory().is_some();
        let searching = choice.discovering && choice.preferred.is_none();
        // The selected card, the one Enter opens: its button leads.
        let lead = self.selected == game;
        let text = crate::game_text::of(game);
        let accent = l.accent(game);
        let image = div()
            .relative()
            // The art gives way first, so the page fits with margins: at
            // 280 px a 960 px window put the legend 8 px off its edge.
            .h(px(art))
            .w_full()
            .overflow_hidden()
            // Rounded as the card is, inside its 1 px border: the card's
            // clip is square, and square art left light notches at the
            // card's rounded top corners.
            .rounded_t(px(7.))
            .bg(l.art)
            .child(
                img(artwork(choice.game))
                    .size_full()
                    .rounded_t(px(7.))
                    .object_fit(ObjectFit::Cover),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(5.))
                    .bg(accent),
            );
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .id(game.profile().steam_app_id)
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(if self.selected == game { accent } else { l.card_edge })
                    .bg(l.card)
                    .cursor_pointer()
                    .hover(|s| s.border_color(accent).bg(l.card_hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.choose(game, window, cx)))
                    .child(image)
                    .child(
                        div()
                            .p_6()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_size(px(10.)).text_color(accent).child(text.era))
                            .child(
                                div()
                                    .flex()
                                    // Bottom-aligned, ANNO lifted by the
                                    // difference in the two sizes' space
                                    // under the baseline: baseline alignment
                                    // came out bottom-aligned, with ANNO
                                    // 11 px below the number's baseline.
                                    .items_end()
                                    .gap_3()
                                    .child(
                                        div()
                                            .mb(px(11.))
                                            .text_size(px(25.))
                                            .font_family("Noto Serif")
                                            .child("ANNO"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(54.))
                                            .font_family("Noto Serif")
                                            .child(game.profile().short_name),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(18.))
                                    .text_color(l.body)
                                    .child(text.subtitle),
                            )
                            .child(
                                div()
                                    .mt_2()
                                    .text_size(px(12.))
                                    .text_color(l.dim)
                                    .child(text.regions),
                            )
                            .child(self.recent_projects(game, cx))
                            // Where the game is, inside the card; changing it
                            // must not also open the editor.
                            .child(
                                div()
                                    .mt_2()
                                    .pt_2()
                                    .border_t_1()
                                    .border_color(l.rule)
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_3()
                                            // The Steam library prefix is the same for both games and
                                            // is what got cut off; the full path is in the tooltip.
                                            .child({
                                                let full = choice
                                                    .directory()
                                                    .map(|p| p.display().to_string());
                                                let shown = choice.directory().map_or_else(
                                                    || "Choose your installation folder".to_owned(),
                                                    |p| {
                                                        let short = crate::project::short_path(p);
                                                        if p.display()
                                                            .to_string()
                                                            .contains("/steamapps/common/")
                                                        {
                                                            format!("Steam library › {short}")
                                                        } else {
                                                            short
                                                        }
                                                    },
                                                );
                                                // The path is the control: one place
                                                // that says which copy opens and
                                                // changes it. "Change game folder…"
                                                // and "2 installations…" beside it
                                                // read as two settings for one.
                                                let several = choice.detected.len() > 1;
                                                // A framed control under its own
                                                // caption, as Recent projects has:
                                                // as grey text it read as a caption.
                                                // Installed is its ✓, so the footer
                                                // below holds only Open.
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .flex()
                                                    .flex_col()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .text_size(px(10.))
                                                            .text_color(l.faint)
                                                            .child("GAME FOLDER"),
                                                    )
                                                    .child(
                                                div()
                                                    .id(SharedString::from(format!(
                                                        "install-path-{}",
                                                        game.profile().steam_app_id
                                                    )))
                                                    .h(px(28.))
                                                    .px_2()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .min_w_0()
                                                    .rounded(px(4.))
                                                    .border_1()
                                                    .border_color(l.rule)
                                                    .bg(l.field)
                                                    .text_size(px(11.))
                                                    .text_color(l.dim)
                                                    .cursor_pointer()
                                                    .hover(|s| s.bg(l.field_hover).text_color(l.body))
                                                    .child(
                                                        gpui_editor_kit::icon::Icon::FOLDER
                                                            .render(13., l.faint),
                                                    )
                                                    .when(installed && !searching, |d| {
                                                        d.child(
                                                            div()
                                                                .flex_shrink_0()
                                                                // Success green, as the
                                                                // Overview's and Play
                                                                // checklist's ✓.
                                                                .text_color(gpui_editor_kit::theme::get().success)
                                                                .child("✓"),
                                                        )
                                                    })
                                                    .child(div().flex_1().min_w_0().truncate().child(shown))
                                                    .child(div().flex_shrink_0().child("▾"))
                                                    // The ✓ said in words on hover.
                                                    .tooltip(widgets::Tooltip::text(
                                                        if installed { "Installed" } else { "Not found" },
                                                        "",
                                                        match (full, several) {
                                                            (Some(full), true) => format!(
                                                                "{full}\n{} copies found; click to choose, or pick another folder",
                                                                choice.detected.len()
                                                            ),
                                                            (Some(full), false) => format!("{full}\nClick to choose another folder"),
                                                            (None, _) => "Click to choose the game’s folder".to_owned(),
                                                        },
                                                    ))
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        if several {
                                                            this.selecting = Some(game);
                                                            this.error = None;
                                                            cx.notify();
                                                        } else {
                                                            this.locate(game, cx);
                                                        }
                                                    })))
                                            })
,
                                    ),
                            )
                            .child(
                                div()
                                    .mt_4()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(if installed { l.found } else { l.missing })
                                            // Installed is the folder's ✓ above;
                                            // only a state to act on is said here.
                                            .child(if searching {
                                                "FINDING INSTALLATIONS…"
                                            } else if installed {
                                                ""
                                            } else {
                                                "INSTALL NOT FOUND"
                                            }),
                                    )
                                    // Filled on the selected card only, the one
                                    // Enter opens; the other card's is
                                    // secondary, so one button leads.
                                    .child(
                                        div()
                                            .px_3()
                                            .py_1()
                                            .rounded(px(4.))
                                            // Both variants carry a border, so
                                            // filled and outlined are one height.
                                            .when(installed && !searching && lead, |d| {
                                                d.bg(gpui_editor_kit::theme::get().primary)
                                                    .border_1()
                                                    .border_color(
                                                        gpui_editor_kit::theme::get().primary,
                                                    )
                                                    .text_color(gpui_editor_kit::theme::get().on_accent)
                                            })
                                            // The app's Secondary: a blue outline
                                            // read as keyboard focus.
                                            .when(installed && !searching && !lead, |d| {
                                                d.bg(gpui_editor_kit::theme::get().secondary)
                                                    .border_1()
                                                    .border_color(
                                                        gpui_editor_kit::theme::get().border,
                                                    )
                                                    .text_color(gpui_editor_kit::theme::get().text)
                                            })
                                            .when(!installed || searching, |d| {
                                                d.text_color(accent)
                                            })
                                            .text_size(px(13.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(if searching {
                                                "Finding your game…"
                                            } else if choice.preferred.is_none()
                                                && choice.detected.len() > 1
                                            {
                                                "Choose installation  →"
                                            } else if installed {
                                                "Open editor  →"
                                            } else {
                                                "Locate game below"
                                            }),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let l = &crate::colors::get().launcher;
        if let Screen::Editing { editor, .. } = &self.screen {
            return div().id("editor-session").size_full().child(editor.clone());
        }
        window.set_window_title("Anno Editor · Choose your game");
        self.focus.focus(window, cx);
        // The cards' art height: what the window leaves after the rest of
        // the page (about 740 px of it) and 32 px margins, within 160-280.
        let art = (f32::from(window.viewport_size().height) - 740.).clamp(160., 280.);
        div()
            .id("game-picker")
            // The editor's face (window_root sets it there); without it the
            // launcher fell back to the default sans and the type changed
            // when the editor opened.
            .font_family("Noto Sans")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .bg(l.page)
            .text_color(l.title)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.selecting.is_some() {
                    if event.keystroke.key == "escape" {
                        this.selecting = None;
                        cx.notify();
                    }
                    return;
                }
                let game = match event.keystroke.key.as_str() {
                    "1" => Game::Anno1800,
                    "2" => Game::Anno117,
                    "enter" => this.selected,
                    _ => return,
                };
                this.choose(game, window, cx);
            }))
            .child(
                div()
                    .w_full()
                    .max_w(px(1100.))
                    .px_8()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(l.heading)
                                    .child("ANNO EDITOR"),
                            )
                            .child(
                                div()
                                    .text_size(px(34.))
                                    .font_family("Noto Serif")
                                    .child("Choose your world"),
                            )
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .text_color(l.dim)
                                    // A fact a modder can use, not a tagline.
                                    .child("Projects and extracted assets stay separate for each game."),
                            ),
                    )
                    .child(
                        div().flex().gap_6().children(
                            self.choices
                                .iter()
                                .map(|choice| self.card(choice, art, cx)),
                        ),
                    )
                    .when_some(self.error.clone(), |el, error| {
                        el.child(
                            div()
                                .text_size(px(12.))
                                .text_color(l.error)
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(l.faint)
                            // The key hints alone, at the right; the line
                            // that sat here is the subtitle now.
                            .child(div())
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(widgets::keycap("1"))
                                    .child("Anno 1800")
                                    .child(widgets::keycap("2"))
                                    .child("Anno 117")
                                    .child(widgets::keycap("Enter"))
                                    // What Enter opens, by name.
                                    .child(format!(
                                        "Open {}",
                                        self.selected.profile().title.split(':').next().unwrap_or_default()
                                    )),
                            ),
                    ),
            )
            .when_some(self.selecting, |root, game| {
                root.child(self.installation_picker(game, cx))
            })
    }
}

impl Launcher {
    /// The game's last projects under its card; a click opens the game with
    /// that one.
    fn recent_projects(&self, game: Game, cx: &mut Context<Self>) -> Div {
        let l = &crate::colors::get().launcher;
        let config = platform::config_dir(game.profile().config_folder);
        let known = crate::project::known(&config, game);
        // Room for two rows, so both cards end level: three left a band
        // under one project and none.
        let mut list = div().mt_3().min_h(px(76.)).flex().flex_col().gap_1();
        // The caption either way, so the empty card has the full one's
        // shape; a lone sentence over an empty band looked half-built.
        list = list.child(
            div()
                .text_size(px(10.))
                .text_color(l.faint)
                .child("RECENT PROJECTS"),
        );
        if known.is_empty() {
            // Plain muted text in a row's place, flush with the caption
            // (a row's inset without the row looked misaligned): a dashed
            // frame read as a drop zone.
            return list.child(
                div()
                    .py_1()
                    .text_size(px(11.))
                    .text_color(l.faint)
                    .child("Opening the editor creates your first project."),
            );
        }
        for (n, p) in known.into_iter().take(3).enumerate() {
            let folder = p.folder.clone();
            let config = config.clone();
            list =
                list.child(
                    div()
                        .id(SharedString::from(format!(
                            "recent-{}-{n}",
                            game.profile().steam_app_id
                        )))
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .bg(l.field)
                        .hover(|s| s.bg(l.field_hover))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .child(div().text_color(l.body).child(p.name))
                        // A muted count: in the card's accent it read as a
                        // link of its own inside the row, which is the target.
                        .child(div().text_color(l.faint).text_size(px(11.)).child(
                            if p.islands == 1 {
                                "1 island  →".to_owned()
                            } else {
                                format!("{} islands  →", p.islands)
                            },
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // The editor opens the most recent project.
                            cx.stop_propagation();
                            crate::project::remember(&config, &folder);
                            this.choose(game, window, cx);
                        })),
                );
        }
        list
    }
}

/// The card art, embedded so every machine shows it. Steam's library hero
/// images (1920×620), re-encoded at JPEG quality 82; the cards are at most
/// 280 px tall, 560 on a 2× display.
fn artwork(game: Game) -> Arc<Image> {
    static ART: [OnceLock<Arc<Image>>; 2] = [OnceLock::new(), OnceLock::new()];
    let (slot, bytes): (_, &'static [u8]) = match game {
        Game::Anno117 => (&ART[0], include_bytes!("launcher/anno117.jpg")),
        Game::Anno1800 => (&ART[1], include_bytes!("launcher/anno1800.jpg")),
    };
    slot.get_or_init(|| Arc::new(Image::from_bytes(ImageFormat::Jpeg, bytes.to_vec())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::{Choice, Settings};
    use anno_formats::game::Game;
    use std::path::{Path, PathBuf};

    fn install(dir: &Path, game: Game) {
        std::fs::create_dir_all(dir.join("Bin/Win64")).unwrap();
        std::fs::create_dir_all(dir.join("maindata")).unwrap();
        let exe = match game {
            Game::Anno117 => "Anno117.exe",
            Game::Anno1800 => "Anno1800.exe",
        };
        std::fs::write(dir.join("Bin/Win64").join(exe), []).unwrap();
        let mut archive = anno_formats::rda::Packer::new(anno_formats::rda::PackOptions::default());
        archive.add("data/fixture.txt", b"fixture").unwrap();
        archive.write(&dir.join("maindata/data0.rda")).unwrap();
    }

    #[test]
    fn environment_overrides_saved_folders_but_invalid_overrides_do_not() {
        let root = tempfile::tempdir().unwrap();
        let saved = root.path().join("saved");
        let configured = root.path().join("configured");
        let wrong_game = root.path().join("wrong game");
        install(&saved, Game::Anno1800);
        install(&configured, Game::Anno1800);
        install(&wrong_game, Game::Anno117);
        let mut settings = Settings::default();
        settings
            .directories
            .insert(Game::Anno1800.profile().steam_app_id.into(), saved.clone());
        assert_eq!(
            settings.directory(Game::Anno1800, Some(configured.clone())),
            Some(configured)
        );
        assert_eq!(
            settings.directory(Game::Anno1800, Some(wrong_game)),
            Some(saved.clone())
        );
        assert_eq!(
            settings.directory(Game::Anno1800, Some(root.path().join("missing"))),
            Some(saved.clone())
        );
        assert_eq!(settings.directory(Game::Anno1800, None), Some(saved));
    }

    #[test]
    fn discovery_cannot_replace_a_folder_chosen_while_it_was_running() {
        let mut choice = Choice {
            game: Game::Anno1800,
            preferred: Some(PathBuf::from("chosen")),
            detected: Vec::new(),
            discovering: true,
        };
        choice.finish_search(Choice {
            game: Game::Anno1800,
            preferred: Some(PathBuf::from("environment")),
            detected: vec![PathBuf::from("Steam"), PathBuf::from("Ubisoft")],
            discovering: false,
        });
        assert_eq!(choice.directory(), Some(&PathBuf::from("chosen")));
        assert_eq!(choice.detected.len(), 2);
        assert!(!choice.discovering);
    }

    #[test]
    fn remembered_folders_must_belong_to_the_selected_game() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Bin/Win64")).unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let mut archive = anno_formats::rda::Packer::new(anno_formats::rda::PackOptions::default());
        archive.add("data/fixture.txt", b"fixture").unwrap();
        archive
            .write(&dir.path().join("maindata/data0.rda"))
            .unwrap();
        std::fs::write(dir.path().join("Bin/Win64/Anno1800.exe"), []).unwrap();
        let mut settings = Settings::default();
        for game in Game::ALL {
            settings
                .directories
                .insert(game.profile().steam_app_id.into(), dir.path().to_owned());
        }
        assert_eq!(
            settings.directory(Game::Anno1800, None).as_deref(),
            Some(dir.path())
        );
        assert_eq!(settings.directory(Game::Anno117, None), None);
        std::fs::remove_dir_all(dir.path().join("Bin")).unwrap();
        assert_eq!(settings.directory(Game::Anno1800, None), None);
    }

    #[test]
    fn picker_settings_read_saved_game_ids() {
        let settings: Settings = serde_json::from_str(
            r#"{"last_game":"1800","directories":{"916440":"/games/Anno 1800"}}"#,
        )
        .unwrap();
        assert_eq!(settings.last_game, Some(Game::Anno1800));
        assert_eq!(
            settings.directories["916440"],
            PathBuf::from("/games/Anno 1800")
        );
        assert!(
            serde_json::from_str::<Settings>(r#"{"last_game":"2070","directories":{}}"#).is_err()
        );
    }
}
