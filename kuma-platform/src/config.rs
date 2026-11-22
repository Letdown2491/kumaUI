//! The key=value state-file store: the grammar Koguma's and Kamuy's
//! state files speak, one home. A record per line, the key split at
//! the first `=` (a value may carry `=`; the filter buffer does),
//! unknown keys and malformed lines die quietly in the reader, so a
//! future key or a hand-edited file cannot break an older reader.
//!
//! The file lands through `files::write_atomic` (one rename, the
//! bytes fsynced first, 0600); the debounce cadence is the caller's
//! policy (a slider drag and a typing buffer quiet at different
//! rates), so only the grammar and the location live here.

use std::path::PathBuf;

/// Where an app's state file lives: the config dir (the spec's
/// fallback chain) under the app's name, then `state`.
pub fn state_file(app: &str) -> Option<PathBuf> {
    Some(crate::xdg::config_dir().join(app).join("state"))
}

/// Parse the file's text into records, in file order. Lines without
/// `=` are skipped (blank lines, hand-edit debris); a value keeps
/// everything after the first `=`, `=` included.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

/// Render records to the file's text. A value carrying a newline
/// would tear the line-based record and silently lose every line
/// after it on the next load (a newline is legal in a Linux path),
/// so that record is dropped rather than saved wrong. A trailing
/// newline because a config file ends with one.
pub fn render<'a, I>(records: I) -> String
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let mut text = String::new();
    for (key, value) in records {
        if value.contains('\n') || value.contains('\r') {
            continue;
        }
        text.push_str(key);
        text.push('=');
        text.push_str(value);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_at_the_first_equals() {
        let records = parse("sort=name\nfilter=a=b=c\n\nnoise\nlast=wins\nsort=size\n");
        assert_eq!(
            records,
            vec![
                ("sort".to_string(), "name".to_string()),
                ("filter".to_string(), "a=b=c".to_string()),
                ("last".to_string(), "wins".to_string()),
                ("sort".to_string(), "size".to_string()),
            ]
        );
    }

    #[test]
    fn parse_of_an_empty_or_absent_file_is_empty() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n").is_empty());
    }

    #[test]
    fn render_writes_one_record_per_line() {
        let text = render([("sort", "name"), ("filter", "a=b"), ("scale", "1.25")]);
        assert_eq!(text, "sort=name\nfilter=a=b\nscale=1.25\n");
    }

    #[test]
    fn render_drops_a_tearing_value_rather_than_saving_it_wrong() {
        let text = render([("tab0", "/home/martin/docs"), ("tab1", "/odd\nname"), ("tab2", "/ok")]);
        assert_eq!(text, "tab0=/home/martin/docs\ntab2=/ok\n");
    }
}
