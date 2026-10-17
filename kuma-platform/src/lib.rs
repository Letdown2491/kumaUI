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
//! - `niri`: the IPC envelope, ADR-0003's hand-rolled client (events,
//!   actions, and response types stay per app)
//! - `titlebar`: the family's client-side title bar, behavior and
//!   geometry shared, colors and glyphs per app

pub mod activation;
pub mod color;
pub mod imaging;
pub mod niri;
pub mod palette;
pub mod titlebar;
pub mod version;
