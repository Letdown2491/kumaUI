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
//! - `niri`: the IPC envelope, ADR-0003's hand-rolled client (events,
//!   actions, and response types stay per app)

pub mod color;
pub mod niri;
pub mod palette;
pub mod version;
