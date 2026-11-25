// Test in game: the dialog that previews which slots of the game's test
// maps change, and installs the mod. The game's `Maps::plan` builds the maps.
use crate::editor::Editor;
use crate::island::fertility;
use crate::project::Project;
use anno_core::game::Module;
pub use anno_core::map::Fill;
use anno_core::project::MapIsland;
pub use anno_core::project::MapPlan;
use anno_formats::assets::Assets;
use anno_formats::vfs::Install;
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    theme,
    widgets::{self, button, hint},
};

/// Builds every test map for the project's islands, without touching the
/// project. `assets` resolves assigned slot resources.
pub fn plan(install: &Install, assets: Option<&Assets>, project: &Project) -> Result<Vec<MapPlan>> {
    let islands = project
        .module
        .islands()
        .context("This game has no islands")?;
    let maps = islands.maps().context("This game has no test maps")?;
    // A project layout marked for testing is the base instead of the
    // game's default: its pool slots take the project's islands.
    let on = (project.layouts.iter().find(|l| l.fill_with_islands))
        .map(|l| crate::map::read_layout(maps, install, l))
        .transpose()?;
    // Level 5 blocks a test map: an island its last save found broken would
    // fail in game, or not load.
    let broken: Vec<String> = (project.islands.iter())
        .filter(|i| i.listing.on_test_map && !i.listing.broken.is_empty())
        .map(|i| {
            format!(
                "{}: {}",
                crate::project::island_name(&i.path),
                crate::island::said(&i.listing.broken)
            )
        })
        .collect();
    anyhow::ensure!(broken.is_empty(), "fix first: {}", broken.join(". "));
    let rules = islands.rules();
    let islands: Vec<MapIsland> = (project.islands.iter())
        .map(|i| MapIsland {
            path: &i.path,
            region: &i.region,
            class: i.class(),
            regular: i.listing.is_regular(rules),
            start: i.listing.is_start(rules),
            on_test_map: i.listing.on_test_map,
            slot_resources: &i.listing.slot_resources,
            fertilities: i.listing.fertilities.as_deref(),
            info: (!i.listing.slot_resources.is_empty())
                .then(|| project.read_file(&format!("{}info", i.path)))
                .flatten(),
            file: (maps.reads_islands())
                .then(|| project.read_file(&i.path))
                .flatten(),
        })
        .collect();
    maps.plan(install, assets, &islands, project.mod_id(), on.as_ref())
}

/// Each project layout's first variant as the dialog previews it: its slots
/// as they stand, pool slots open and fixed ones the game's.
fn layout_previews(install: &Install, project: &Project) -> Vec<(String, MapPlan)> {
    let Some(maps) = project.module.islands().and_then(|i| i.maps()) else {
        return Vec::new();
    };
    (project.layouts.iter())
        .filter_map(|l| {
            let map = crate::map::read_layout(maps, install, l).ok()?;
            let v = map.variants.first()?;
            let slots = (v.slots.iter())
                .map(|s| anno_core::project::PlanSlot {
                    position: [s.at[0].max(0) as u32, s.at[1].max(0) as u32],
                    class: u16::from(s.class),
                    starter: s.role == anno_core::map::Role::Start,
                    fill: if s.fixed() { Fill::Vanilla } else { Fill::Pool },
                })
                .collect();
            let plan = MapPlan {
                region: map.region.clone(),
                island_size: v.setup.island_size,
                size: v.size,
                slots,
                relabels_map_type: false,
                entry: l.variants.first()?.clone(),
                files: Vec::new(),
            };
            Some((l.name.clone(), plan))
        })
        .collect()
}

/// Stages the plans' files and makes them the project's play-test maps.
pub fn apply(plans: Vec<MapPlan>, project: &mut Project) {
    let mut maps = Vec::new();
    for plan in plans {
        for (path, bytes) in plan.files {
            project.stage(&path, bytes);
        }
        maps.push(plan.entry);
    }
    project.set_maps(maps);
}

/// The dialog's wording: maps that take over a shipped map type (1800's
/// Old World Archipelago) are found under its name.
fn playtest_text(module: &dyn Module, plans: &[MapPlan]) -> &'static anno_core::text::Playtest {
    let text = module.text();
    match &text.playtest_old_world {
        Some(old) if plans.iter().any(|m| m.relabels_map_type) => old,
        _ => &text.playtest,
    }
}

/// The open Test in game dialog.
pub struct Playtest {
    plans: Vec<MapPlan>,
    /// The project's layouts, by name, installed with the mod as they are.
    layouts: Vec<(String, MapPlan)>,
}

impl Editor {
    /// Why Test in game cannot run yet, if it cannot: shown on the
    /// disabled menu item and button before anyone clicks.
    pub fn playtest_blocker(&self) -> Option<&'static str> {
        if self.module.packaging().is_none() {
            return Some("This game has no playtest delivery");
        }
        self.test_map_blocker()
    }

    /// Why Make test map cannot run yet, if it cannot. It needs no
    /// packaging: it only writes the maps into the project folder.
    pub fn test_map_blocker(&self) -> Option<&'static str> {
        if self.module.islands().and_then(|i| i.maps()).is_none() {
            return Some("This game has no test maps");
        }
        if !self.project.islands.iter().any(|i| i.listing.on_test_map) {
            return Some(
                "No island is on the test map yet: make one (New island…), keep \"On the test map\" ticked, and save",
            );
        }
        if self.island.as_ref().is_some_and(|i| i.dirty()) {
            return Some("Save the island (Ctrl S) first; the test map uses the saved islands");
        }
        None
    }

    /// File → Test in game…
    pub fn open_playtest(&mut self) {
        let Some(install) = self.install.clone() else {
            return self.message("No install is open", true);
        };
        if let Some(why) = self.playtest_blocker() {
            return self.message(why, true);
        }
        if let Some(why) = self.broken_islands() {
            return self.message(format!("Can't test in game, fix first: {why}"), true);
        }
        match plan(&install, self.assets.as_deref(), &self.project) {
            Ok(plans) => {
                let layouts = layout_previews(&install, &self.project);
                self.playtest = Some(Playtest { plans, layouts });
            }
            Err(e) => self.message(format!("Could not build the test map: {e:#}"), true),
        }
    }

    /// File → Make test map: the game's test maps for the project's saved
    /// islands, written into the project folder and nowhere else. Test in
    /// game does the same and installs; this is the step a game without
    /// packaging (2070) has, whose maps the game lists only once delivery
    /// registers them.
    pub fn make_test_map(&mut self) {
        let Some(install) = self.install.clone() else {
            return self.message("No install is open", true);
        };
        if let Some(why) = self.test_map_blocker() {
            return self.message(why, true);
        }
        if let Some(why) = self.broken_islands() {
            return self.message(format!("No test map, fix first: {why}"), true);
        }
        let plans = match plan(&install, self.assets.as_deref(), &self.project) {
            Ok(plans) => plans,
            Err(e) => return self.message(format!("Could not build the test map: {e:#}"), true),
        };
        let paths: Vec<String> = (plans.iter())
            .flat_map(|p| p.files.iter().map(|f| f.0.clone()))
            .collect();
        apply(plans, &mut self.project);
        match self.project.write() {
            Ok(folder) => {
                self.mount_project();
                self.message(
                    format!(
                        "Wrote {} into {}",
                        paths.join(", "),
                        crate::project::short_path(&folder)
                    ),
                    false,
                );
            }
            Err(e) => self.message(format!("Test map not written: {e:#}"), true),
        }
    }

    fn install_playtest(&mut self, cx: &mut Context<Self>) {
        if self.install.is_none() {
            return;
        }
        let Some(p) = self.playtest.take() else {
            return;
        };
        let hint = playtest_text(self.module, &p.plans).in_game;
        let log = self.module.packaging().map_or("", |p| p.loader_log);
        apply(p.plans, &mut self.project);
        self.install_project(
            move |path| {
                format!(
                    "Installed {}. In game: {hint}. Problems show in {log}",
                    path.display(),
                )
            },
            cx,
        );
    }

    pub fn render_playtest(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let p = self.playtest.as_ref()?;
        let rules = self.module.islands()?.rules();
        let text = playtest_text(self.module, &p.plans);
        let mut maps = div().flex().flex_wrap().gap_3();
        // One preview per region: the Medium variant; the other two follow
        // the same rule on their own slot layouts.
        for plan in p.plans.iter().filter(|m| m.island_size == "Medium") {
            maps = maps.child(self.map_preview(plan));
        }
        // Layouts install as saved: 117's under their own map type, 1800's
        // in place of the shipped maps they play as.
        let tested = self
            .project
            .layouts
            .iter()
            .find(|l| l.fill_with_islands)
            .map(|l| l.name.as_str());
        let mut layouts = div().flex().flex_wrap().gap_3();
        for (name, plan) in &p.layouts {
            let note = if Some(name.as_str()) == tested {
                format!("{name} · filled with your islands")
            } else {
                format!("{name} · as saved")
            };
            layouts = layouts.child(
                self.map_preview(plan).child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::get().muted)
                        .child(note),
                ),
            );
        }
        let mut lines = div().flex().flex_col().gap_1();
        for plan in &p.plans {
            let start = if plan.starts_on_ours() {
                "you start on your island".to_owned()
            } else {
                format!(
                    "start islands stay vanilla (needs a {} island with the Start role)",
                    rules.start_classes()
                )
            };
            lines = lines.child(hint(format!(
                "{} · island size {}: {} of {} slots are yours, {start}",
                plan.region,
                plan.island_size,
                plan.ours(),
                plan.slots.len()
            )));
        }
        let mut islands = div().flex().flex_col().gap_1();
        for i in &self.project.islands {
            let name = crate::project::island_name(&i.path);
            let mut resources: Vec<String> = self
                .assets
                .as_deref()
                .map(|a| {
                    i.listing
                        .slot_resources
                        .values()
                        .map(|&g| fertility::label(a, g))
                        .collect()
                })
                .unwrap_or_default();
            resources.sort();
            resources.dedup();
            let resources = if resources.is_empty() {
                String::new()
            } else {
                format!(" · resources: {}", resources.join(", "))
            };
            islands = islands.child(
                div()
                    .text_size(px(12.))
                    .child(format!("{name} — {} {}{resources}", i.region, i.size)),
            );
        }
        let card = widgets::dialog("Test in game")
            .w(px(640.))
            .gap_2()
            // A click outside closes the dialog; one on it must not.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(hint(text.intro.to_owned()))
            .child(islands)
            .child(maps)
            .child(lines)
            .when(!p.layouts.is_empty(), |c| {
                c.child(widgets::section("This project's map layouts"))
                    .child(layouts)
            })
            .child(hint(text.steps.to_owned()))
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        button("Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                            this.playtest = None;
                            cx.notify();
                        })),
                    )
                    .child(button("Install into the game", true).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.install_playtest(cx);
                            cx.notify();
                        },
                    ))),
            );
        Some(
            gpui_editor_kit::widgets::modal("playtest", card).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.playtest = None;
                    cx.notify();
                }),
            ),
        )
    }

    /// The map's slots to scale: yours filled, pool slots outlined, the rest
    /// faint. Starter slots get a heavier border.
    fn map_preview(&self, plan: &MapPlan) -> Div {
        const SIDE: f32 = 220.;
        let scale = SIDE / plan.size[0].max(plan.size[1]).max(1) as f32;
        let mut map = div()
            .relative()
            .size(px(SIDE))
            .bg(theme::get().viewport)
            .border_1()
            .border_color(theme::get().border);
        // Template classes past the game's largest (1800 has no XL) draw
        // as the largest.
        let classes = (self.module.islands()).map_or(&[][..], |i| i.rules().size_classes);
        for s in &plan.slots {
            let class = classes.get(s.class as usize).or(classes.last());
            let side = class.map_or(0, |c| c.slot) as f32 * scale;
            let mut cell = div()
                .absolute()
                .left(px(s.position[0] as f32 * scale))
                .top(px(s.position[1] as f32 * scale))
                .size(px(side))
                .rounded_sm();
            cell = match s.fill {
                Fill::Ours(_) => cell.bg(theme::get().highlight.alpha(0.8)),
                Fill::Pool => cell.border_1().border_color(theme::get().muted),
                Fill::Vanilla => cell.bg(theme::get().border.alpha(0.53)),
            };
            if s.starter {
                cell = cell.border_2().border_color(theme::get().accent);
            }
            map = map.child(cell);
        }
        div().flex().flex_col().gap_1().child(map).child(
            div()
                .text_size(px(11.))
                .text_color(theme::get().muted)
                .child(format!(
                    "{} · yellow: yours, blue border: start",
                    plan.region
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Fill, apply, plan};
    use crate::project::{IslandEntry, Listing, Project};
    use anno_formats::a7m::template::{MapTemplate, SlotKind};

    // Needs Anno 2070: a 2070 project's test map plans through the shell.
    // Its `.isd` islands are read from the project, and its world is
    // scenario 06's with a start island in a Large start slot and a regular
    // one placed; the plan stages one world file.
    #[test]
    fn a_2070_project_plans_its_test_map_through_the_shell() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let module: &dyn anno_core::game::Module = &anno2070::MODULE;
        let install =
            anno_formats::vfs::Install::open(dir, module.profile().archive_families).unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), module).unwrap();
        for (name, shipped, kind) in [
            ("reef", "n_m02", "Normal"),
            ("haven", "n_l07", "Normal;Starter"),
        ] {
            let path = format!("data/levels/islands/normal/{name}.isd");
            let bytes = install
                .read(&format!("data/levels/islands/normal/{shipped}.isd"))
                .unwrap();
            project.stage(&path, bytes);
            project.register_island(IslandEntry {
                guid: project.island_guid(&path),
                path,
                region: String::new(),
                size: String::new(),
                listing: Listing {
                    kind: kind.into(),
                    on_test_map: true,
                    ..Listing::default()
                },
            });
        }
        let plans = plan(&install, None, &project).unwrap();
        assert_eq!(plans.len(), 1, "one world");
        let p = &plans[0];
        assert_eq!(p.ours(), 2);
        let start = (p.slots.iter())
            .find(|s| s.starter && matches!(s.fill, Fill::Ours(_)))
            .expect("the start island takes a start slot");
        let large = module.islands().unwrap().rules().size_classes.len() - 1;
        assert_eq!(usize::from(start.class), large);
        assert!(p.entry.path.ends_with(".www"), "{}", p.entry.path);
        assert_eq!(p.files.len(), 1);
        apply(plans, &mut project);
        let folder = project.write().unwrap();
        let map = &project.maps[0].path;
        assert!(folder.join(map).is_file(), "{map}");
        // The islands are files of their own in a shared folder.
        assert_eq!(
            project.island_files("data/levels/islands/normal/reef.isd"),
            ["data/levels/islands/normal/reef.isd"]
        );
    }

    // Needs Anno 2070: in the editor, a 2070 project's new island (File →
    // New island, a copy of a surface template, saved) makes its test map
    // with Make test map, which needs no packaging: the world goes into the
    // project folder on disk and places the island. Test in game stays off,
    // saying why.
    #[test]
    fn a_2070_project_makes_its_test_map_from_the_editor() {
        use crate::editor::Editor;
        let Some(dir) = anno_formats::vfs::install_dir("ANNO2070_DIR") else {
            eprintln!("skip: ANNO2070_DIR unset");
            return;
        };
        let mut app = gpui::TestApp::new();
        let config = tempfile::tempdir().unwrap();
        let folder = config.path().to_path_buf();
        let mut w = app.open_window(|window, cx| {
            Editor::new(
                Project::load(&folder, &anno2070::MODULE).unwrap(),
                folder,
                Some(dir.into()),
                None,
                window,
                cx,
            )
        });
        app.run_until_parked();
        w.read(|ed, _| {
            assert_eq!(
                ed.test_map_blocker(),
                Some(
                    "No island is on the test map yet: make one (New island…), keep \"On the test map\" ticked, and save"
                )
            );
        });
        w.update(|ed, _, cx| {
            ed.open_wizard(cx);
            let wizard = ed.wizard.as_mut().expect("the wizard opened");
            wizard.set_name("Test Reef");
            ed.create_island(cx);
        });
        app.run_until_parked();
        let path = w.update(|ed, _, _| {
            let path = ed
                .island
                .as_ref()
                .expect("the island was made")
                .path
                .clone();
            ed.save();
            path
        });
        assert!(path.starts_with("data/levels/islands/"), "{path}");
        w.update(|ed, _, _| {
            assert_eq!(
                ed.playtest_blocker(),
                Some("This game has no playtest delivery")
            );
            assert_eq!(ed.test_map_blocker(), None);
            ed.make_test_map();
            let map = ed.project.maps.first().expect("a test map").path.clone();
            assert!(map.starts_with("data/levels/testworlds/"), "{map}");
            let file = ed.project.folder().join(&map);
            let bytes = std::fs::read(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let doc = anno_formats::isd::Doc::parse(&bytes).unwrap();
            let placed = anno_formats::isd::world::read(&doc).unwrap();
            assert!(
                placed.iter().any(|p| p.file.replace('\\', "/") == path),
                "the world places {path}"
            );
        });
    }

    #[test]
    fn a_test_map_holds_the_project_islands() {
        let Some(dir) = anno_formats::vfs::install_dir("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let families = crate::games::profile(anno_core::game::Game::Anno117).archive_families;
        let install = anno_formats::vfs::Install::open(dir, families).unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), &anno117::MODULE).unwrap();
        for (name, size, kind, on_test_map) in [
            ("nova", "Large", "Normal;Starter", true),
            ("parva", "Medium", "Normal", true),
            ("hidden", "Small", "Normal", false),
        ] {
            let path = format!("data/base/provinces/roman/islands/custom/{name}/{name}.a7m");
            project.register_island(IslandEntry {
                guid: project.island_guid(&path),
                path,
                region: "Roman".into(),
                size: size.into(),
                listing: Listing {
                    kind: kind.into(),
                    on_test_map,
                    ..Listing::default()
                },
            });
        }
        let plans = plan(&install, None, &project).unwrap();
        assert_eq!(plans.len(), 3, "one per setup island size");
        for p in &plans {
            assert!(
                p.starts_on_ours(),
                "{}: a Large island takes the starts",
                p.island_size
            );
            assert!(
                p.slots.iter().any(|s| s.fill == Fill::Ours(1)),
                "the Medium island is used"
            );
        }
        apply(plans, &mut project);
        let folder = project.write().unwrap();

        let maps = project.maps.clone();
        assert_eq!(maps.len(), 3);
        for m in &maps {
            let info = std::fs::read(folder.join(m.path.replace(".a7t", ".a7tinfo"))).unwrap();
            let t = MapTemplate::parse(&info).unwrap();
            let fixed: Vec<&str> = t
                .slots
                .iter()
                .filter_map(|s| match &s.kind {
                    SlotKind::Fixed { path, .. } => Some(path.as_str()),
                    _ => None,
                })
                .collect();
            assert!(fixed.iter().any(|p| p.ends_with("nova.a7m")));
            assert!(folder.join(&m.path).exists());
        }
        let assets =
            std::fs::read_to_string(folder.join("data/base/config/export/assets.xml")).unwrap();
        let ty = project.map_type_guid().unwrap();
        assert!(assets.contains(&format!("<MapType>{ty}</MapType>")));
        assert!(assets.contains(&format!("<MapTemplateType>{ty}</MapTemplateType>")));
        assert!(
            assets.contains("<Template>RandomIsland</Template>"),
            "pool entries stay"
        );
        let texts =
            std::fs::read_to_string(folder.join("data/base/config/gui/texts_german.xml")).unwrap();
        assert!(texts.contains(&format!("<LineId>{ty}</LineId>")));

        // Reloading keeps the maps and does not read generated ModOps back
        // as staged files.
        let back = Project::load(config.path(), &anno117::MODULE).unwrap();
        assert_eq!(back.maps, maps);
        assert!(!back.files.keys().any(|p| p.ends_with(".xml")));
        assert_eq!(back.file_count(), 9);
    }

    // The shape 2070's test maps take: the game writes one world file, and
    // the shell stages and saves it like any other map.
    #[test]
    fn a_game_that_writes_one_world_file_gets_it_staged() {
        use crate::games::fixtures::PartialGame;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("maindata")).unwrap();
        let mut pack = anno_formats::rda::Packer::new(Default::default());
        pack.add("data/fixture.txt", b"fixture").unwrap();
        std::fs::write(
            dir.path().join("maindata/data0.rda"),
            pack.encode().unwrap(),
        )
        .unwrap();
        let install = anno_formats::vfs::Install::open(dir.path(), &[]).unwrap();
        let folder = dir.path().join("project");
        let game = &PartialGame::Islands;
        let mut project = Project::create(&folder, "World", "world", game).unwrap();
        for (n, (path, on_test_map)) in [("data/a.fixture", true), ("addondata/b.fixture", false)]
            .into_iter()
            .enumerate()
        {
            project.register_island(IslandEntry {
                path: path.into(),
                guid: 10 + n as u32,
                region: "Reef".into(),
                size: "Medium".into(),
                listing: Listing {
                    on_test_map,
                    in_pool: false,
                    ..Listing::default()
                },
            });
        }
        let plans = plan(&install, None, &project).unwrap();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].ours(), 1);
        apply(plans, &mut project);
        project.write().unwrap();
        assert_eq!(
            std::fs::read(folder.join("data/maps/world.world")).unwrap(),
            b"data/a.fixture"
        );
        let back = Project::open(&folder, game).unwrap();
        assert_eq!(back.maps.len(), 1);
        assert_eq!(back.maps[0].path, "data/maps/world.world");
        assert_eq!(
            &*back.read_file("data/maps/world.world").unwrap(),
            b"data/a.fixture"
        );
    }

    /// FNV-1a, 64 bit, as island::baselines records saves.
    fn digest(bytes: &[u8]) -> String {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        format!("len {} fnv {h:016x}", bytes.len())
    }

    /// Every file a saved project folder holds outside `.anno-editor`, one
    /// `path = digest` line each, sorted.
    fn folder_digests(folder: &std::path::Path) -> String {
        let mut lines = Vec::new();
        let mut stack = vec![folder.to_owned()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let path = e.unwrap().path();
                let rel = path.strip_prefix(folder).unwrap().to_string_lossy();
                if rel.starts_with(".anno-editor") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else {
                    lines.push(format!(
                        "{rel} = {}",
                        digest(&std::fs::read(&path).unwrap())
                    ));
                }
            }
        }
        lines.sort();
        lines.join("\n")
    }

    /// A project with islands on and off the pool and the test map, its
    /// manifest edited, test maps planned, applied and saved.
    fn golden_project(
        module: &'static dyn anno_core::game::Module,
        install: &anno_formats::vfs::Install,
        folder: &std::path::Path,
        islands: &[(&str, &str, &str, &str, bool)],
        layout: bool,
    ) -> String {
        let mut project = Project::create(folder, "Golden Isles", "golden-isles", module).unwrap();
        project.info.creator = "Recorder".into();
        project.info.difficulty = "easier".into();
        project.info.requires = vec!["shared-lib".into()];
        for &(path, region, size, kind, on_test_map) in islands {
            project.register_island(IslandEntry {
                guid: project.island_guid(path),
                path: path.into(),
                region: region.into(),
                size: size.into(),
                listing: Listing {
                    kind: kind.into(),
                    on_test_map,
                    in_pool: !path.contains("hidden"),
                    fertilities: (region == "Moderate" && size == "Medium").then(Vec::new),
                    ..Listing::default()
                },
            });
        }
        let plans = plan(install, None, &project).unwrap();
        apply(plans, &mut project);
        if layout {
            golden_layout(install, &mut project);
        }
        folder_digests(&project.write().unwrap())
    }

    /// A layout on the game's default base sea, as the Map workspace saves
    /// one: a Start pool slot of the largest class, a Small pool slot and a
    /// fixed island turned once, in every variant.
    fn golden_layout(install: &anno_formats::vfs::Install, project: &mut Project) {
        use anno_core::map::{Role, Slot};
        let islands = project.module.islands().unwrap();
        let maps = islands.maps().expect("the game has maps");
        let rules = maps.rules();
        let shipped = maps.shipped(install, None);
        let bases: Vec<_> = shipped.iter().filter(|s| s.base).collect();
        let base = (bases.iter())
            .find(|s| s.name.contains("default"))
            .or(bases.first())
            .expect("a base sea");
        let mut map = maps.new_map(install, &base.path, "golden_straits").unwrap();
        let largest = rules.size_classes.iter().map(|c| c.slot).max().unwrap();
        let fixed = (islands.shipped(install, None).into_iter())
            .find(|s| s.world[0] <= largest)
            .expect("a shipped island")
            .path;
        let island = map.island(&fixed);
        let [w, h] = map.size.map(|v| v as i32);
        let at = |x: i32, y: i32| [x / 32 * 32, y / 32 * 32];
        let slot = |at, island, role, class, turns| Slot {
            at,
            island,
            role,
            class,
            turns,
            raw_type: rules.raw_type(role).unwrap_or(0),
            source: Slot::NEW,
        };
        for v in &mut map.variants {
            v.slots = vec![
                slot(
                    at(w / 4, h / 4),
                    Slot::POOL,
                    Role::Start,
                    (rules.size_classes.len() - 1) as u8,
                    0,
                ),
                slot(at(w * 5 / 8, h / 4), Slot::POOL, Role::Normal, 0, 0),
                slot(
                    at(w / 2, h * 5 / 8),
                    island,
                    Role::Normal,
                    rules.class_for(largest),
                    1,
                ),
            ];
        }
        let files = maps.write(install, &map, "golden_straits").unwrap();
        let entry = crate::map::layout_entry(&map, &files, project.layouts.iter()).unwrap();
        for (path, bytes) in files {
            project.stage(&path, bytes);
        }
        project.set_layout(entry, rules.registers);
    }

    // Recorded at d51799a, before the games' project facts left
    // the shell: modinfo.json, the generated ModOps and texts, and the test
    // map files, byte for byte. ANNO_EDITOR_BLESS=1 prints fresh ones.
    #[test]
    fn test_map_files_and_modops_match_the_recorded_ones() {
        let bless = std::env::var("ANNO_EDITOR_BLESS").is_ok_and(|v| v == "1");
        let island_117: &[(&str, &str, &str, &str, bool)] = &[
            (
                "data/base/provinces/roman/islands/custom/nova/nova.a7m",
                "Roman",
                "Large",
                "Normal;Starter",
                true,
            ),
            (
                "data/base/provinces/roman/islands/custom/parva/parva.a7m",
                "Roman",
                "Medium",
                "Normal",
                true,
            ),
            (
                "data/base/provinces/roman/islands/custom/hidden/hidden.a7m",
                "Roman",
                "Small",
                "Normal",
                false,
            ),
            (
                "data/base/provinces/celtic/islands/custom/moor/moor.a7m",
                "Celtic",
                "Medium",
                "Normal",
                true,
            ),
        ];
        // The third case: the 117 project with a map layout saved, its
        // files, map type, template assets and type name recorded too.
        let cases: [(&str, &'static dyn anno_core::game::Module, &str, &[_], bool); 3] = [
            (
                "ANNO117_DIR",
                &anno117::MODULE,
                GOLDEN_117,
                island_117,
                false,
            ),
            (
                "ANNO1800_DIR",
                &anno1800::MODULE,
                GOLDEN_1800,
                &[
                    (
                        "data/sessions/islands/custom/haven/haven.a7m",
                        "Moderate",
                        "Large",
                        "Normal;Starter",
                        true,
                    ),
                    (
                        "data/sessions/islands/custom/inlet/inlet.a7m",
                        "Moderate",
                        "Medium",
                        "Normal",
                        true,
                    ),
                    (
                        "data/sessions/islands/custom/hidden/hidden.a7m",
                        "Moderate",
                        "Small",
                        "Normal",
                        false,
                    ),
                    (
                        "data/sessions/islands/custom/cay/cay.a7m",
                        "Colony01",
                        "Medium",
                        "Normal",
                        true,
                    ),
                ],
                false,
            ),
            (
                "ANNO117_DIR",
                &anno117::MODULE,
                GOLDEN_117_LAYOUT,
                island_117,
                true,
            ),
        ];
        for (var, module, golden, islands, layout) in cases {
            let Some(dir) = anno_formats::vfs::install_dir(var) else {
                eprintln!("skip: {var} unset");
                continue;
            };
            let families = module.profile().archive_families;
            let install = anno_formats::vfs::Install::open(dir, families).unwrap();
            let folder = tempfile::tempdir().unwrap();
            let have = golden_project(
                module,
                &install,
                &folder.path().join("golden"),
                islands,
                layout,
            );
            if bless {
                eprintln!(
                    "{var}{}:\n{have}",
                    if layout { " with a layout" } else { "" }
                );
                continue;
            }
            let want = golden.trim();
            if have != want {
                let differ: Vec<&str> = (have.lines())
                    .filter(|l| !want.lines().any(|w| w == *l))
                    .chain(want.lines().filter(|w| !have.lines().any(|l| l == *w)))
                    .collect();
                panic!(
                    "{var}: files differ from the recorded ones:\n  {}",
                    differ.join("\n  ")
                );
            }
        }
    }

    const GOLDEN_117: &str = r#"
data/base/config/export/assets.xml = len 6807 fnv e7dffe643baa8676
data/base/config/gui/texts_brazilian.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_english.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_french.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_german.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_italian.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_japanese.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_korean.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_polish.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_russian.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_simplified_chinese.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_spanish.xml = len 270 fnv 47c909e92afc4a1b
data/base/config/gui/texts_traditional_chinese.xml = len 270 fnv 47c909e92afc4a1b
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7t = len 1521267 fnv 411d56d5bf373142
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7tinfo = len 4808 fnv fdacc5d7a025838c
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7t = len 1499366 fnv a01f69bb0e1f7ac4
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7tinfo = len 3856 fnv dbc15aa4f3068a75
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7t = len 1499371 fnv 0293970d2d2e57f3
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7tinfo = len 4288 fnv 68f790cf34d3e162
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7t = len 377009 fnv 6a35749da8eb2cce
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7tinfo = len 5056 fnv bd23d2f84a84754e
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7t = len 343803 fnv f0a0986fdf7f154e
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7tinfo = len 4624 fnv c63ab671159062ee
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7t = len 343835 fnv 5ee31bc3a17f57d8
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7tinfo = len 5056 fnv 5b885a5b1d86cb04
modinfo.json = len 321 fnv b4ba1e4fab06611f
"#;

    const GOLDEN_1800: &str = r#"
data/config/export/main/asset/assets.xml = len 3422 fnv a49e856036a383d3
data/config/gui/texts_brazilian.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_chinese.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_english.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_french.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_german.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_italian.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_japanese.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_korean.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_polish.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_portuguese.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_russian.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_spanish.xml = len 266 fnv f7b369ddc70a003b
data/config/gui/texts_taiwanese.xml = len 266 fnv f7b369ddc70a003b
data/sessions/maps/custom/colony01/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7t = len 172515 fnv c0a3ea3e4da78143
data/sessions/maps/custom/colony01/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7te = len 24031 fnv 16c4d0a6cd381a46
data/sessions/maps/custom/colony01/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7tinfo = len 1528 fnv 3d11a01f8a5bd04b
data/sessions/maps/custom/moderate/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7t = len 209537 fnv bd049f75ac98e0fa
data/sessions/maps/custom/moderate/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7te = len 29182 fnv f68b8c0c20180803
data/sessions/maps/custom/moderate/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7tinfo = len 2797 fnv 26778b663093931e
data/sessions/maps/custom/moderate/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7t = len 209537 fnv bd049f75ac98e0fa
data/sessions/maps/custom/moderate/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7te = len 29182 fnv f68b8c0c20180803
data/sessions/maps/custom/moderate/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7tinfo = len 2021 fnv 6fe570263495ccf6
data/sessions/maps/custom/moderate/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7t = len 209537 fnv bd049f75ac98e0fa
data/sessions/maps/custom/moderate/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7te = len 29182 fnv f68b8c0c20180803
data/sessions/maps/custom/moderate/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7tinfo = len 2715 fnv 7b69c90cc421ae88
modinfo.json = len 273 fnv abd3e06c3fd37d55
"#;

    // The 117 case with a layout: blessed at the Map workspace's first
    // save path (track 1's 117 `Maps`, rearch 0656164c).
    const GOLDEN_117_LAYOUT: &str = r#"
data/base/config/export/assets.xml = len 9491 fnv 7e393e3c851f5a33
data/base/config/gui/texts_brazilian.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_english.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_french.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_german.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_italian.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_japanese.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_korean.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_polish.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_russian.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_simplified_chinese.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_spanish.xml = len 418 fnv f1004a36f02b364e
data/base/config/gui/texts_traditional_chinese.xml = len 418 fnv f1004a36f02b364e
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7t = len 1521267 fnv 411d56d5bf373142
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7tinfo = len 4808 fnv fdacc5d7a025838c
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7t = len 1499366 fnv a01f69bb0e1f7ac4
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7tinfo = len 3856 fnv dbc15aa4f3068a75
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7t = len 1499371 fnv 0293970d2d2e57f3
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7tinfo = len 4288 fnv 68f790cf34d3e162
data/base/provinces/celtic/templates/custom/golden_straits_easy/golden_straits_easy.a7t = len 1521267 fnv 411d56d5bf373142
data/base/provinces/celtic/templates/custom/golden_straits_easy/golden_straits_easy.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_straits_easy/golden_straits_easy.a7tinfo = len 1216 fnv 0042d2d8707c0bf2
data/base/provinces/celtic/templates/custom/golden_straits_hard/golden_straits_hard.a7t = len 1499366 fnv a01f69bb0e1f7ac4
data/base/provinces/celtic/templates/custom/golden_straits_hard/golden_straits_hard.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_straits_hard/golden_straits_hard.a7tinfo = len 1128 fnv 9166322f034f0dd3
data/base/provinces/celtic/templates/custom/golden_straits_medium/golden_straits_medium.a7t = len 1499371 fnv 0293970d2d2e57f3
data/base/provinces/celtic/templates/custom/golden_straits_medium/golden_straits_medium.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/celtic/templates/custom/golden_straits_medium/golden_straits_medium.a7tinfo = len 1128 fnv 9166322f034f0dd3
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7t = len 377009 fnv 6a35749da8eb2cce
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_easy/golden_isles_editor_islands_easy.a7tinfo = len 5056 fnv bd23d2f84a84754e
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7t = len 343803 fnv f0a0986fdf7f154e
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_hard/golden_isles_editor_islands_hard.a7tinfo = len 4624 fnv c63ab671159062ee
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7t = len 343835 fnv 5ee31bc3a17f57d8
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7te = len 29216 fnv ce1d0ebed9a7decc
data/base/provinces/roman/templates/custom/golden_isles_editor_islands_medium/golden_isles_editor_islands_medium.a7tinfo = len 5056 fnv 5b885a5b1d86cb04
modinfo.json = len 321 fnv b4ba1e4fab06611f
"#;
}
