// Process setup that has to happen before GPUI starts: display backend
// selection on Linux and the per-user config directory.
use anyhow::Result;
use std::path::PathBuf;

/// `$XDG_CONFIG_HOME/<app>`, falling back to `~/.config/<app>`; on Windows,
/// `%APPDATA%\<app>`.
#[cfg(not(target_os = "windows"))]
pub fn config_dir(app: &str) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
        .join(app)
}

// HOME is usually unset on Windows; the XDG fallback would be a relative
// `.config` under whatever directory the editor started in.
#[cfg(target_os = "windows")]
pub fn config_dir(app: &str) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(app)
}

/// Picks the display backend before GPUI starts, re-executing the process
/// under XWayland when needed. `override_var` names the environment variable
/// users set to `auto`, `x11` or `wayland`, e.g. `WORLD_EDITOR_DISPLAY_BACKEND`.
///
/// Call it first in `main`, before spawning threads or loading data: on
/// success it may never return.
#[cfg(target_os = "linux")]
pub fn configure_display(override_var: &str) -> Result<()> {
    linux::configure_display(override_var)
}

#[cfg(not(target_os = "linux"))]
pub fn configure_display(_override_var: &str) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, bail, ensure};
    use std::{env, ffi::OsStr, fs, os::unix::process::CommandExt, path::Path, process::Command};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Backend {
        Auto,
        X11,
        Wayland,
    }

    impl Backend {
        fn parse(name: &str, value: Option<&OsStr>) -> Result<Self> {
            match value {
                None => Ok(Self::Auto),
                Some(value) if value == "auto" => Ok(Self::Auto),
                Some(value) if value == "x11" => Ok(Self::X11),
                Some(value) if value == "wayland" => Ok(Self::Wayland),
                Some(value) => bail!("Invalid {name} {value:?}; use auto, x11, or wayland"),
            }
        }

        fn select(self, wayland: bool, x11: bool, nvidia: bool) -> Result<Self> {
            match self {
                Self::Auto => Ok(if x11 && (nvidia || !wayland) {
                    Self::X11
                } else if wayland {
                    Self::Wayland
                } else {
                    Self::Auto
                }),
                Self::X11 => {
                    ensure!(x11, "X11 requested, but DISPLAY is not set");
                    Ok(self)
                }
                Self::Wayland => {
                    ensure!(wayland, "Wayland requested, but WAYLAND_DISPLAY is not set");
                    Ok(self)
                }
            }
        }
    }

    pub fn configure_display(name: &str) -> Result<()> {
        if env::var_os("ZED_HEADLESS").is_some() {
            return Ok(());
        }
        let requested = Backend::parse(name, env::var_os(name).as_deref())?;
        let wayland = env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
        let x11 = env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
        let nvidia = nvidia_display(Path::new("/sys/class/drm"));
        // DISPLAY can name a server that no longer answers (its socket file
        // removed while Xwayland still runs); GPUI then panics at startup.
        // Automatic choice falls back to Wayland; an explicit x11 still tries.
        let x11 = x11 && (requested == Backend::X11 || x11_answers());
        if requested == Backend::Auto
            && !x11
            && env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        {
            eprintln!("DISPLAY does not answer; using Wayland instead.");
        }
        let selected = requested.select(wayland, x11, nvidia)?;
        if selected != Backend::X11 || !wayland {
            return Ok(());
        }
        // GPUI 0.2.2's native Wayland/Blade path loses the device on affected
        // NVIDIA desktops. XWayland is verified on that configuration; native
        // Wayland stays available as an explicit opt-in.
        if requested == Backend::Auto {
            eprintln!("Using XWayland compatibility for NVIDIA. Override with {name}=wayland.");
        } else {
            eprintln!("Using the requested X11 display backend.");
        }
        // exec preserves the PID and arguments without mutating the environment
        // of a multithreaded process. The replacement has no WAYLAND_DISPLAY,
        // so this branch cannot repeat.
        let error = Command::new(env::current_exe().context("Locate the editor executable")?)
            .args(env::args_os().skip(1))
            .env_remove("WAYLAND_DISPLAY")
            .exec();
        Err(error).context("Restart the editor with the X11 display backend")
    }

    /// Whether the local X display named by DISPLAY accepts a connection,
    /// on its socket file or its abstract socket. Remote displays
    /// (host:n) are assumed to answer.
    fn x11_answers() -> bool {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixStream};
        let Some(display) = env::var_os("DISPLAY") else {
            return false;
        };
        let display = display.to_string_lossy().into_owned();
        let Some(number) = display
            .strip_prefix(':')
            .or_else(|| display.strip_prefix("unix:"))
            .map(|rest| rest.split('.').next().unwrap_or(rest).to_owned())
        else {
            return true;
        };
        let path = format!("/tmp/.X11-unix/X{number}");
        UnixStream::connect(&path).is_ok()
            || SocketAddr::from_abstract_name(path.as_bytes())
                .and_then(|addr| UnixStream::connect_addr(&addr))
                .is_ok()
    }

    fn nvidia_display(drm: &Path) -> bool {
        let Ok(entries) = fs::read_dir(drm) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let card = name.to_str().and_then(|name| name.strip_prefix("card"));
            if !card.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) {
                return false;
            }
            let device = entry.path().join("device");
            fs::read_to_string(device.join("vendor"))
                .is_ok_and(|vendor| vendor.trim().eq_ignore_ascii_case("0x10de"))
                && fs::read_link(device.join("driver"))
                    .is_ok_and(|driver| driver.file_name() == Some(OsStr::new("nvidia")))
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn nvidia_prefers_xwayland_and_native_wayland_stays_otherwise() {
            assert_eq!(
                Backend::Auto.select(true, true, true).unwrap(),
                Backend::X11
            );
            assert_eq!(
                Backend::Auto.select(true, true, false).unwrap(),
                Backend::Wayland
            );
            assert_eq!(
                Backend::Auto.select(false, true, false).unwrap(),
                Backend::X11
            );
            assert_eq!(
                Backend::Auto.select(false, false, false).unwrap(),
                Backend::Auto
            );
            assert_eq!(
                Backend::Wayland.select(true, true, true).unwrap(),
                Backend::Wayland
            );
            assert!(Backend::X11.select(true, false, true).is_err());
        }

        #[test]
        fn backend_setting_is_validated() {
            assert_eq!(Backend::parse("X", None).unwrap(), Backend::Auto);
            assert_eq!(
                Backend::parse("X", Some(OsStr::new("x11"))).unwrap(),
                Backend::X11
            );
            assert!(Backend::parse("X", Some(OsStr::new("vulkan"))).is_err());
        }

        #[test]
        fn drm_probe_distinguishes_nvidia_from_nouveau_and_connectors() {
            let root = tempfile::tempdir().unwrap();
            let card = |name: &str, vendor: &str, driver: &str| {
                let device = root.path().join(name).join("device");
                fs::create_dir_all(&device).unwrap();
                fs::write(device.join("vendor"), vendor).unwrap();
                std::os::unix::fs::symlink(
                    format!("../../drivers/{driver}"),
                    device.join("driver"),
                )
                .unwrap();
            };
            card("card0", "0x10de\n", "nouveau");
            card("card0-DP-1", "0x10de\n", "nvidia");
            assert!(!nvidia_display(root.path()));
            card("card1", "0x10de\n", "nvidia");
            assert!(nvidia_display(root.path()));
        }
    }
}
