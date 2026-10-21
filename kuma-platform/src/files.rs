//! Files that must never tear. State and config records land through
//! a temp sibling and one rename: a crash or power loss mid-write
//! tears the temp file, never the record.

use std::io;
use std::path::Path;

/// Write `bytes` to `path` atomically: the bytes go to a `.tmp`
/// sibling that one rename moves into place. A torn `config.toml`
/// loads as defaults, and the next save would seal the loss, so the
/// record is replaced whole or not at all; a failed rename removes
/// the temp file instead of littering.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = std::fs::remove_file(&tmp);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_write_never_tears_the_record() {
        let dir = std::env::temp_dir().join(format!("kuma-write-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("record.toml");
        std::fs::write(&path, "original = true\n").unwrap();

        write_atomic(&path, b"replaced = true\n").expect("the write lands");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replaced = true\n");
        assert!(
            !dir.join("record.toml.tmp").exists(),
            "the temp sibling is gone after the rename"
        );

        // a write into a directory that vanished fails and leaves the
        // old record alone (and no temp litter)
        let ghost = dir.join("gone").join("record.toml");
        assert!(write_atomic(&ghost, b"x").is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "replaced = true\n"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
