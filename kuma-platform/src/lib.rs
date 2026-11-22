//! Shared wiring for the kuma apps (kuma-shell, kuma-files,
//! kuma-term, kuma-monitor): the setup every app would otherwise
//! copy-paste. A module lands here when a second app needs it; apps
//! declare what they are, the platform owns how.
//!
//! - `version`: the `--version` line, the in-app tag, the changelog
//!   guard, the flag itself
//! - `palette`: the shell's published palette, located, read, and
//!   parsed (the theme application stays per app)
//! - `color`: the HSL space the palettes derive from
//! - `imaging`: the decode-to-`RenderImage` family, the BGRA byte
//!   order gpui expects in one place
//! - `activation`: the single-instance activation socket, keyed by
//!   app id; a second launch hands its request over and exits
//! - `app`: the main preamble (logging, the version flag, the session
//!   gate, the main window's open)
//! - `niri`: the IPC envelope, ADR-0003's hand-rolled client (events,
//!   actions, and response types stay per app)
//! - `titlebar`: the family's client-side title bar, behavior and
//!   geometry shared, colors and glyphs per app
//! - `xdg`: the base-directory fallback chain (config, state), the
//!   spec's ignore-relative rule in one place
//! - `open`: the one URL opener, the scheme allowlist at the gate
//! - `format`: presentation formatting every surface renders the
//!   same way (byte sizes)
//! - `sysfs`: the kernel-state samplers (/proc, /sys, statvfs) the
//!   monitoring surfaces share
//! - `desktop`: the desktop-entry parser, the Exec tokenizer, the
//!   applications walk, and the wayland-sessions variant

pub mod activation;
pub mod app;
pub mod color;
pub mod desktop;
pub mod files;
pub mod format;
pub mod imaging;
pub mod logging;
pub mod niri;
pub mod open;
pub mod palette;
pub mod process;
pub mod sysfs;
pub mod titlebar;
pub mod version;
pub mod xdg;
