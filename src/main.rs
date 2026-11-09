// Anno editor: game selection, asset browser, island editor and UI editor.
mod browser;
mod colors;
mod editor;
mod game_text;
mod island;
mod launcher;
mod playtest;
mod project;
mod terrain;
mod ui;

use anno_formats::game::Game;
use anyhow::{Context as _, Result, bail};
use gpui_editor_kit::{gpui::*, platform};
use std::path::PathBuf;

fn main() -> Result<()> {
    if std::env::var_os("ANNO_EDITOR_PROFILE").is_some() {
        gpui_editor_kit::profile::enable();
    }
    let mut install = None;
    let mut open = None;
    let mut game = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: anno-editor [GAME_DIR] [--game 117|1800] [--open VIRTUAL_PATH]");
                return Ok(());
            }
            "--open" => open = Some(args.next().context("--open needs a virtual path")?),
            "--game" => {
                game = Some(match args.next().as_deref() {
                    Some("117") => Game::Anno117,
                    Some("1800") => Game::Anno1800,
                    _ => bail!("--game must be 117 or 1800"),
                });
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
        let detected =
            Game::detect(dir).context("folder is not an Anno 117 or Anno 1800 install")?;
        if game.is_some_and(|game| game != detected) {
            bail!("game folder does not match --game");
        }
        game = Some(detected);
    }
    platform::configure_display("ANNO_EDITOR_DISPLAY_BACKEND")?;
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
