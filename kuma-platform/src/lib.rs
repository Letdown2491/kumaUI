//! Shared wiring for the kuma apps (kuma-shell, kuma-files, kuma-term):
//! the setup every app would otherwise copy-paste. A module lands here
//! when a second app needs it; apps declare what they are, the platform
//! owns how.

pub mod version;
