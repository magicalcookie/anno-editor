// Anno editor: game selection, asset browser, island editor and UI editor.
mod browser;
mod colors;
mod editor;
mod games;
mod graphics;
#[cfg(any(test, feature = "heap-profile"))]
mod heap;
mod island;
mod launcher;
mod map;
mod model;
mod playtest;
mod project;
mod terrain;
mod ui;

use crate::games::GameExt;
use anno_core::game::Game;
use anyhow::{Context as _, Result, bail};
use gpui_editor_kit::{gpui::*, platform};
use std::path::PathBuf;

#[cfg(any(test, feature = "heap-profile"))]
#[global_allocator]
static HEAP: heap::Sampler = heap::Sampler;

/// Allocations of 16 MB and up get pages of their own, handed back to the
/// system when freed. glibc otherwise raises its mmap threshold to the size
/// of each large block freed (up to 32 MB), so the big buffers an island
/// load frees stay in its per-thread arenas. ram_bench over the continental
/// island, the pirate island, moderate_c_01 and the continental island
/// again, RSS plus swap: 1457, 1114, 1433, 1738 MB as glibc ships, 1291,
/// 812, 1069, 1314 so, peak 1993 to 1839, loads as fast. jemalloc,
/// tcmalloc and mimalloc held 300-450 MB more after the switches; a 4 MB
/// threshold slowed the continental load 2x, capping arenas at 8 the UI's
/// parallel layouts by 40%.
fn tune_malloc() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        const M_MMAP_THRESHOLD: i32 = -3;
        unsafe extern "C" {
            fn mallopt(param: i32, value: i32) -> i32;
        }
        // SAFETY: glibc's own call; made before other threads start.
        unsafe { mallopt(M_MMAP_THRESHOLD, 16 << 20) };
    }
}

fn main() -> Result<()> {
    tune_malloc();
    // ANNO_EDITOR_HEAP=seconds (with `--features heap-profile`): the
    // resident size and the live heap's owners that long after start.
    #[cfg(feature = "heap-profile")]
    if let Some(secs) = std::env::var("ANNO_EDITOR_HEAP")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        heap::start();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            heap::trim();
            eprintln!("heap profile after {secs} s: {}", heap::resident());
            heap::report(40);
        });
    }
    if std::env::var_os("ANNO_EDITOR_PROFILE").is_some() {
        gpui_editor_kit::profile::enable();
    }
    // ANNO_EDITOR_LOSE_DEVICE=seconds: destroys the windows' GPU device
    // that long after start, as a driver reset would, to see every view
    // recover onto the new one.
    if let Some(secs) = std::env::var("ANNO_EDITOR_LOSE_DEVICE")
        .ok()
        .and_then(|s| s.parse::<f32>().ok())
    {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs_f32(secs.max(0.)));
            if gpui_editor_kit::gpu::lose_window_device() {
                eprintln!("ANNO_EDITOR_LOSE_DEVICE: destroyed the windows' GPU device");
            } else {
                eprintln!("ANNO_EDITOR_LOSE_DEVICE: no view had drawn yet; nothing lost");
            }
        });
    }
    let mut install = None;
    let mut open = None;
    let mut game = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                let names: Vec<&str> = Game::ALL.map(|g| g.profile().short_name).into();
                let names = names.join("|");
                println!("usage: anno-editor [GAME_DIR] [--game {names}] [--open VIRTUAL_PATH]");
                return Ok(());
            }
            "--open" => open = Some(args.next().context("--open needs a virtual path")?),
            "--game" => {
                let name = args.next().unwrap_or_default();
                let found = (Game::ALL.into_iter()).find(|g| g.profile().short_name == name);
                let names: Vec<&str> = Game::ALL.map(|g| g.profile().short_name).into();
                game =
                    Some(found.with_context(|| format!("--game must be {}", names.join(" or ")))?);
            }
            _ if arg.starts_with('-') => bail!("unknown option {arg}"),
            _ => {
                if install.replace(PathBuf::from(arg)).is_some() {
                    bail!("pass only one game folder");
                }
            }
        }
    }
    if let Some(dir) = &install {
        let detected = Game::detect(dir).with_context(|| {
            let titles: Vec<&str> = Game::ALL.map(|g| g.profile().title).into();
            format!("folder is not an install of {}", titles.join(", "))
        })?;
        if game.is_some_and(|game| game != detected) {
            bail!("game folder does not match --game");
        }
        game = Some(detected);
    }
    platform::configure_display("ANNO_EDITOR_DISPLAY_BACKEND")?;
    gpui_editor_kit::gpu::install();
    gpui_editor_kit::gpui_platform::application().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1600.), px(960.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |window, cx| {
                colors::restore(window, cx);
                window
                    .observe_window_appearance(|window, cx| colors::follow(window, cx))
                    .detach();
                cx.new(|cx| launcher::Launcher::new(game, install, open, window, cx))
            },
        )
        .expect("open editor window");
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    Ok(())
}
