// Test in game: copies of shipped map templates with the project's islands
// in fixed slots. 117 adds a map type; 1800 redirects existing map assets
// (`game::TestMaps`). The dialog previews which slots change and installs
// the mod.
use crate::editor::Editor;
use crate::island::fertility;
use crate::project::{IslandEntry, MapEntry, Project};
use anno_formats::a7minfo::A7mInfo;
use anno_formats::assets::Assets;
use anno_formats::game::{Game, TestMaps};
use anno_formats::template::{MapTemplate, SIZE_NAMES, SlotKind, TYPE_NORMAL, TYPE_STARTER};
use anno_formats::vfs::Install;
use anyhow::{Context as _, Result};
use gpui_editor_kit::{
    gpui::{prelude::*, *},
    theme,
    widgets::{self, button, hint},
};

/// The Default templates a play-test map copies, by setup island size.
const VARIANTS: [(&str, &str); 3] = [("easy", "Large"), ("medium", "Medium"), ("hard", "Small")];

/// The smallest class that may take a starter slot: the shipped starters
/// are all Large or XL.
const STARTER_CLASS: u16 = 2;

#[derive(Clone, Debug, PartialEq)]
pub enum Fill {
    /// One of the project's islands, by index into `Project::islands`.
    Ours(usize),
    /// Left to the vanilla pool.
    Pool,
    /// A vanilla fixed island or a non-island element.
    Vanilla,
}

#[derive(Clone, Debug)]
pub struct PlanSlot {
    pub position: [u32; 2],
    /// Size class index into `SIZE_NAMES`.
    pub class: u16,
    pub starter: bool,
    pub fill: Fill,
}

/// One generated template.
pub struct MapPlan {
    pub region: String,
    pub island_size: &'static str,
    pub size: [u32; 2],
    pub slots: Vec<PlanSlot>,
    entry: MapEntry,
    files: Vec<(String, Vec<u8>)>,
}

impl MapPlan {
    fn ours(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| matches!(s.fill, Fill::Ours(_)))
            .count()
    }

    fn starts_on_ours(&self) -> bool {
        self.slots
            .iter()
            .any(|s| s.starter && matches!(s.fill, Fill::Ours(_)))
    }
}

/// An island the test map may use.
#[derive(Clone, Copy)]
struct Candidate {
    /// Index into `Project::islands`.
    island: usize,
    class: u16,
    regular: bool,
    start: bool,
}

/// Which island each random Normal or Starter slot gets: the largest
/// island of the matching role that fits the slot class, rotating through
/// ties so several islands share the map. Starter slots only take Large or
/// XL. Other slot types (decoration, third parties, volcanoes) stay vanilla.
fn assign(slots: &[(u16, u16)], islands: &[Candidate]) -> Vec<Option<usize>> {
    let mut turn = [[0usize; SIZE_NAMES.len()]; 2];
    slots
        .iter()
        .map(|&(class, island_type)| {
            let (min, start) = match island_type {
                TYPE_NORMAL => (0, false),
                TYPE_STARTER => (STARTER_CLASS, true),
                _ => return None,
            };
            let fits = |c: &&Candidate| {
                (if start { c.start } else { c.regular }) && c.class >= min && c.class <= class
            };
            let best = islands.iter().filter(fits).map(|c| c.class).max()?;
            let ties: Vec<usize> = islands
                .iter()
                .filter(fits)
                .filter(|c| c.class == best)
                .map(|c| c.island)
                .collect();
            let turn = &mut turn[start as usize][best as usize];
            let pick = ties[*turn % ties.len()];
            *turn += 1;
            Some(pick)
        })
        .collect()
}

/// What a fixed slot holding `island` carries for its resources:
/// `FertilityGuids` and the slot object IDs to force on. Islands without
/// assigned resources keep both empty, leaving them to the map generator.
/// Assignments are checked against the saved `.a7minfo` slot list, the one
/// the generator reads.
fn island_resources(
    install: &Install,
    assets: Option<&Assets>,
    project: &Project,
    island: &IslandEntry,
    start: bool,
) -> Result<(Vec<u32>, Vec<u64>)> {
    let (Some(assets), false) = (assets, island.listing.slot_resources.is_empty()) else {
        return Ok(Default::default());
    };
    let info_path = format!("{}info", island.path);
    let bytes = match project.files.get(&info_path) {
        Some(b) => b.clone(),
        None => install.read(&info_path)?,
    };
    let info = A7mInfo::parse(&bytes).with_context(|| format!("Reading {info_path}"))?;
    let mut slot_types: Vec<&str> = Vec::new();
    let mut active = Vec::new();
    let mut assigned = Vec::new();
    for group in &info.random_slots() {
        slot_types.extend(fertility::slot_type_of(assets, group.guid));
        for o in &group.objects {
            if let Some(&g) = island.listing.slot_resources.get(&o.object_id) {
                active.push(o.object_id);
                assigned.push(g);
            }
        }
    }
    let list = fertility::island_list(assets, &island.region, start, &slot_types, &assigned);
    Ok((list, active))
}

/// Builds every play-test template for the project's islands, without
/// touching the project. `assets` resolves assigned slot resources; without
/// it every island's resources stay random.
pub fn plan(install: &Install, assets: Option<&Assets>, project: &Project) -> Result<Vec<MapPlan>> {
    let profile = install.game().profile();
    // Redirected maps are found, and their slots checked, in assets.xml.
    let loaded;
    let assets = if profile.test_maps == TestMaps::Redirect && assets.is_none() {
        loaded = Assets::load(install)?;
        Some(&loaded)
    } else {
        assets
    };
    let mut regions: Vec<&str> = project
        .islands
        .iter()
        .filter(|i| i.listing.on_test_map)
        .map(|i| i.region.as_str())
        .collect();
    regions.sort();
    regions.dedup();
    let mut out = Vec::new();
    for region in regions {
        let lower = region.to_ascii_lowercase();
        let islands: Vec<Candidate> = project
            .islands
            .iter()
            .enumerate()
            .filter(|(_, i)| i.region == region && i.listing.on_test_map)
            .filter_map(|(n, i)| {
                Some(Candidate {
                    island: n,
                    class: class_of(i)?,
                    regular: i.listing.is_regular(),
                    start: i.listing.is_start(),
                })
            })
            .collect();
        for (variant, island_size) in VARIANTS {
            let (source, source_guids) = match profile.test_maps {
                TestMaps::Redirect => {
                    if region != "Moderate" && island_size != "Medium" {
                        continue;
                    }
                    let assets = assets.context("1800 map assets unavailable")?;
                    let candidates: Vec<_> = assets
                        .iter()
                        .filter_map(|a| a.map.as_ref().map(|m| (a.guid, m)))
                        .filter(|(_, m)| {
                            m.region == region
                                && !m.path.contains("/scenario")
                                && (region != "Colony01" || m.path.contains("/pool/"))
                                && (region != "Moderate"
                                    || (m.map_type == "Archipelago"
                                        && m.island_size == island_size))
                        })
                        .collect();
                    let source = candidates
                        .iter()
                        .find(|(_, m)| m.size == "Large")
                        .or_else(|| candidates.first())
                        .with_context(|| format!("no shipped {region} test-map template"))?
                        .1
                        .path
                        .trim_end_matches(".a7t")
                        .to_owned();
                    (source, candidates.iter().map(|(g, _)| *g).collect())
                }
                TestMaps::MapType => {
                    let name = format!("{lower}_province_default_01_{variant}");
                    (
                        format!("data/base/provinces/{lower}/templates/pool/{name}/{name}"),
                        Vec::new(),
                    )
                }
            };
            let mut template = MapTemplate::parse(
                &install
                    .read(&format!("{source}.a7tinfo"))
                    .with_context(|| format!("Reading {source}.a7tinfo"))?,
            )?;
            let keys: Vec<(u16, u16)> = template
                .slots
                .iter()
                .map(|s| match s.kind {
                    SlotKind::Random { size, island_type } => (size, island_type),
                    _ => (0, u16::MAX),
                })
                .collect();
            let picks = assign(&keys, &islands);
            let mut slots = Vec::new();
            for (n, pick) in picks.iter().enumerate() {
                let (class, island_type) = keys[n];
                let fill = match (pick, &template.slots[n].kind) {
                    (Some(i), _) => Fill::Ours(*i),
                    (None, SlotKind::Random { .. }) => Fill::Pool,
                    _ => Fill::Vanilla,
                };
                slots.push(PlanSlot {
                    position: template.slots[n].position,
                    class: class.min(SIZE_NAMES.len() as u16 - 1),
                    starter: island_type == TYPE_STARTER,
                    fill,
                });
                if let Some(i) = pick {
                    let island = &project.islands[*i];
                    match profile.test_maps {
                        TestMaps::Redirect => {
                            let mut mapping = Vec::new();
                            if !island.listing.slot_resources.is_empty() {
                                let assets = assets.context("slot assets unavailable")?;
                                let path = format!("{}info", island.path);
                                let bytes = project
                                    .files
                                    .get(&path)
                                    .cloned()
                                    .map(Ok)
                                    .unwrap_or_else(|| install.read(&path))?;
                                let info = A7mInfo::parse(&bytes)?;
                                for group in info.random_slots() {
                                    if !assets
                                        .get(group.guid)
                                        .is_some_and(|a| a.slot_type.as_deref() == Some("Random"))
                                    {
                                        continue;
                                    }
                                    let replacements =
                                        assets.slot_replacements(group.guid, &island.region);
                                    for object in group.objects {
                                        let chosen = island
                                            .listing
                                            .slot_resources
                                            .get(&object.object_id)
                                            .copied()
                                            .or_else(|| replacements.first().map(|a| a.guid))
                                            .unwrap_or(0);
                                        anyhow::ensure!(
                                            chosen == 0
                                                || replacements.iter().any(|a| a.guid == chosen),
                                            "slot {}: resource {chosen} is not a replacement for {}",
                                            object.object_id,
                                            group.guid
                                        );
                                        mapping.push((object.object_id, chosen));
                                    }
                                }
                            }
                            if let Some(fertilities) = &island.listing.fertilities {
                                let allowed = fertility::choices(
                                    assets.context("1800 assets unavailable")?,
                                    &island.region,
                                );
                                for guid in fertilities {
                                    anyhow::ensure!(
                                        allowed.iter().any(|f| f.guid == *guid),
                                        "fertility {guid} is not available in {}",
                                        island.region
                                    );
                                }
                            }
                            template.fix_1800(
                                n,
                                &island.path,
                                &mapping,
                                island.listing.fertilities.as_deref(),
                            )?;
                        }
                        TestMaps::MapType => {
                            let (fertilities, active) = island_resources(
                                install,
                                assets,
                                project,
                                island,
                                island_type == TYPE_STARTER,
                            )?;
                            template.fix(n, &island.path, &fertilities, &active)?;
                        }
                    }
                }
            }
            let [before, after] = profile.custom_maps_folder;
            let prefix = format!("{before}{lower}{after}");
            let name = format!(
                "{}_editor_islands_{variant}",
                project.mod_id().replace('-', "_")
            );
            let stem = format!("{prefix}/{name}/{name}");
            let mut files = vec![(format!("{stem}.a7tinfo"), template.encode())];
            // The .a7t (session gamedata, no island references) and the
            // editor-only .a7te are copied as shipped.
            for ext in ["a7t", "a7te"] {
                files.push((
                    format!("{stem}.{ext}"),
                    install
                        .read(&format!("{source}.{ext}"))
                        .with_context(|| format!("Reading {source}.{ext}"))?,
                ));
            }
            out.push(MapPlan {
                region: region.to_owned(),
                island_size,
                size: template.size,
                slots,
                entry: MapEntry {
                    path: format!("{stem}.a7t"),
                    region: region.to_owned(),
                    island_size: island_size.to_owned(),
                    source_guids,
                },
                files,
            });
        }
    }
    Ok(out)
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

fn class_of(island: &IslandEntry) -> Option<u16> {
    SIZE_NAMES
        .iter()
        .position(|s| *s == island.class())
        .map(|c| c as u16)
}

/// The dialog's wording: 1800 maps that include the Old World relabel its
/// Archipelago map type.
fn playtest_text(game: Game, plans: &[MapPlan]) -> &'static crate::game_text::Playtest {
    let text = crate::game_text::of(game);
    match &text.playtest_old_world {
        Some(old) if plans.iter().any(|m| m.region == "Moderate") => old,
        _ => &text.playtest,
    }
}

/// The open Test in game dialog.
pub struct Playtest {
    plans: Vec<MapPlan>,
}

impl Editor {
    /// Why Test in game cannot run yet, if it cannot: shown on the
    /// disabled menu item and button before anyone clicks.
    pub fn playtest_blocker(&self) -> Option<&'static str> {
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
        match plan(&install, self.assets.as_deref(), &self.project) {
            Ok(plans) => self.playtest = Some(Playtest { plans }),
            Err(e) => self.message(format!("Could not build the test map: {e:#}"), true),
        }
    }

    fn install_playtest(&mut self) {
        let (Some(p), Some(install)) = (self.playtest.take(), self.install.clone()) else {
            return;
        };
        let hint = playtest_text(self.game, &p.plans).in_game;
        apply(p.plans, &mut self.project);
        let installed = self.project.install(install.dir());
        self.mount_project();
        match installed {
            Ok(path) => self.message(
                format!(
                    "Installed {}. In game: {hint}. Problems show in mods/mod-loader.log",
                    path.display(),
                ),
                false,
            ),
            Err(e) => self.message(format!("Install failed: {e:#}"), true),
        }
    }

    pub fn render_playtest(&self, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let p = self.playtest.as_ref()?;
        let profile = self.game.profile();
        let text = playtest_text(self.game, &p.plans);
        let mut maps = div().flex().flex_wrap().gap_3();
        // One preview per region: the Medium variant; the other two follow
        // the same rule on their own slot layouts.
        for plan in p.plans.iter().filter(|m| m.island_size == "Medium") {
            maps = maps.child(self.map_preview(plan));
        }
        let mut lines = div().flex().flex_col().gap_1();
        for plan in &p.plans {
            let start = if plan.starts_on_ours() {
                "you start on your island".to_owned()
            } else {
                format!(
                    "start islands stay vanilla (needs a {} island with the Start role)",
                    profile.start_classes()
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
            let name = i
                .path
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_end_matches(".a7m");
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
                            this.install_playtest();
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
        let classes = self.game.profile().size_classes;
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
    use super::{Candidate, Fill, apply, assign, plan};
    use crate::project::{IslandEntry, Listing, Project};
    use anno_formats::template::{MapTemplate, SlotKind, TYPE_NORMAL, TYPE_STARTER};

    #[test]
    fn slots_take_the_largest_island_that_fits() {
        // (class, type): Small normal, Medium normal ×2, Large starter,
        // decoration (type 3).
        let slots = [
            (0, TYPE_NORMAL),
            (1, TYPE_NORMAL),
            (1, TYPE_NORMAL),
            (2, TYPE_STARTER),
            (1, 3),
        ];
        let island = |island, class, regular, start| Candidate {
            island,
            class,
            regular,
            start,
        };
        // Two regular Medium islands and one Small that is both.
        let islands = [
            island(0, 1, true, false),
            island(1, 1, true, false),
            island(2, 0, true, true),
        ];
        assert_eq!(
            assign(&slots, &islands),
            [Some(2), Some(0), Some(1), None, None],
            "Medium islands alternate; a start slot needs a Large one"
        );
        let start_only = [island(7, 2, false, true)];
        assert_eq!(
            assign(&slots, &start_only),
            [None, None, None, Some(7), None],
            "a start-only island stays out of regular slots"
        );
        let regular = [island(7, 2, true, false)];
        assert_eq!(
            assign(&slots, &regular),
            [None, None, None, None, None],
            "a regular Large island is no start, and too big for Medium"
        );
    }

    #[test]
    fn a_test_map_holds_the_project_islands() {
        let Some(dir) = std::env::var_os("ANNO117_DIR") else {
            eprintln!("skip: ANNO117_DIR unset");
            return;
        };
        let install = anno_formats::vfs::Install::open(dir).unwrap();
        let config = tempfile::tempdir().unwrap();
        let mut project = Project::load(config.path(), anno_formats::game::Game::Anno117);
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
                    SlotKind::Fixed { path } => Some(path.as_str()),
                    _ => None,
                })
                .collect();
            assert!(fixed.iter().any(|p| p.ends_with("nova.a7m")));
            assert!(folder.join(&m.path).exists());
        }
        let assets =
            std::fs::read_to_string(folder.join("data/base/config/export/assets.xml")).unwrap();
        let ty = project.map_type_guid();
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
        let back = Project::load(config.path(), anno_formats::game::Game::Anno117);
        assert_eq!(back.maps, maps);
        assert!(!back.files.keys().any(|p| p.ends_with(".xml")));
        assert_eq!(back.files.len(), 9);
    }
}
