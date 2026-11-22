//! XDG base-directory resolution: the fallback chain every app would
//! otherwise copy-paste. The spec's rule is the law here: an explicit
//! variable that is set but relative (or empty) is "considered an
//! error" and ignored, falling through to the home-derived path.
//!
//! Two bases matter today: `config_dir` (XDG_CONFIG_HOME, the
//! settings and store files) and `state_dir` (XDG_STATE_HOME, state
//! that may survive a reboot without belonging in config).

use std::ffi::OsStr;
use std::path::PathBuf;

/// The pure rule behind the two bases, exposed so the XDG handling is
/// testable without mutating process environment. `suffix` is the
/// home-relative default (".config", ".local/state"). `None` means
/// neither variable carried a usable home: the caller decides (some
/// want a relative last resort, some want to give up).
pub fn base_dir(xdg: Option<&OsStr>, home: Option<&OsStr>, suffix: &str) -> Option<PathBuf> {
    let explicit = xdg.map(PathBuf::from).filter(|dir| dir.is_absolute());
    match explicit {
        Some(dir) => Some(dir),
        None => Some(PathBuf::from(home?).join(suffix)),
    }
}

/// The config base: $XDG_CONFIG_HOME, else ~/.config, else the
/// relative ".config" (a homeless environment still gets a working,
/// if ugly, path).
pub fn config_dir() -> PathBuf {
    base_dir(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        ".config",
    )
    .unwrap_or_else(|| PathBuf::from(".config"))
}

/// The state base: $XDG_STATE_HOME, else ~/.local/state, else the
/// relative ".local/state".
pub fn state_dir() -> PathBuf {
    base_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        ".local/state",
    )
    .unwrap_or_else(|| PathBuf::from(".local/state"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_xdg_value_wins() {
        let dir = base_dir(Some("/etc".as_ref()), Some("/home/u".as_ref()), ".config");
        assert_eq!(dir, Some(PathBuf::from("/etc")));
    }

    #[test]
    fn a_relative_or_empty_xdg_value_is_ignored() {
        // the spec calls a relative path an error: fall through to home
        let dir = base_dir(Some("rel".as_ref()), Some("/home/u".as_ref()), ".config");
        assert_eq!(dir, Some(PathBuf::from("/home/u/.config")));
        let dir = base_dir(Some("".as_ref()), Some("/home/u".as_ref()), ".config");
        assert_eq!(dir, Some(PathBuf::from("/home/u/.config")));
    }

    #[test]
    fn no_home_and_no_xdg_is_none() {
        let dir = base_dir(None, None, ".config");
        assert_eq!(dir, None);
    }
}
