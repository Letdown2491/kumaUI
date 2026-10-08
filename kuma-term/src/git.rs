//! The bar's git segment: facts about the working tree under the bar's
//! cwd. Queried by spawning the system git, one debounced call per
//! trigger, always off the render path. git being absent or the cwd
//! living outside a repo just means no segment.

/// The git facts the bar shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitSummary {
    /// checked-out branch, or the short commit id when detached
    pub branch: String,
    /// status entries: staged, unstaged, unmerged, and untracked all count
    pub dirty: usize,
    pub ahead: usize,
    pub behind: usize,
}

/// Query git for `cwd`. None on any failure: the bar treats git as
/// optional decoration, never a hard dependency.
pub fn summary(cwd: &str) -> Option<GitSummary> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain=v2", "--branch", "--untracked-files=normal"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `git status --porcelain=v2 --branch` output. The `# branch.*`
/// headers carry the branch and ahead/behind; every non-header line is a
/// changed, renamed, unmerged, or untracked entry, which all count as
/// dirty for a summary line.
fn parse(text: &str) -> Option<GitSummary> {
    let mut head = None;
    let mut oid = None;
    let (mut ahead, mut behind) = (0, 0);
    let mut dirty = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            head = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.oid ") {
            oid = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            let mut parts = rest.trim().split(' ');
            if let Some(a) = parts.next() {
                ahead = a.trim_start_matches('+').parse().unwrap_or(0);
            }
            if let Some(b) = parts.next() {
                behind = b.trim_start_matches('-').parse().unwrap_or(0);
            }
        } else if line.starts_with('#') || line.is_empty() {
            // the other headers carry nothing the bar shows
        } else {
            dirty += 1;
        }
    }
    let head = head?;
    let branch = match head.as_str() {
        "(detached)" => {
            let short: String = oid.unwrap_or_default().chars().take(7).collect();
            if short.is_empty() { head } else { short }
        },
        _ => head,
    };
    Some(GitSummary { branch, dirty, ahead, behind })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_dirty_and_ahead_behind() {
        let text = "\
# branch.oid 1234567890abcdef1234567890abcdef12345678
# branch.head main
# branch.upstream origin/main
# branch.ab +2 -1
1 .M N100 0 0 file-a
2 R. N100 0 0 old.txt new.txt
? build/
";
        let summary = parse(text).unwrap();
        assert_eq!(summary.branch, "main");
        assert_eq!(summary.dirty, 3);
        assert_eq!(summary.ahead, 2);
        assert_eq!(summary.behind, 1);
    }

    #[test]
    fn no_upstream_means_no_ahead_behind() {
        let text = "\
# branch.oid 1234567890abcdef1234567890abcdef12345678
# branch.head trunk
1 .M N100 0 0 file-a
";
        let summary = parse(text).unwrap();
        assert_eq!(summary.branch, "trunk");
        assert_eq!(summary.dirty, 1);
        assert_eq!(summary.ahead, 0);
        assert_eq!(summary.behind, 0);
    }

    #[test]
    fn detached_head_shows_the_short_oid() {
        let text = "\
# branch.oid 1234567890abcdef1234567890abcdef12345678
# branch.head (detached)
";
        let summary = parse(text).unwrap();
        assert_eq!(summary.branch, "1234567");
    }

    #[test]
    fn text_without_a_branch_header_is_not_a_repo() {
        assert_eq!(parse("fatal: not a git repository"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn summary_of_a_real_repo_matches_the_porcelain_shape() {
        // skip when git is missing; the unit tests above carry the parser
        let probe = std::process::Command::new("git").arg("--version").output();
        let Ok(_) = probe else { return };

        let dir = std::env::temp_dir().join(format!("kuma-term-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        run(&["init", "-q"]);
        run(&["commit", "--allow-empty", "-q", "-m", "init"]);

        let clean = summary(dir.to_str().unwrap()).unwrap();
        assert_eq!(clean.dirty, 0);
        assert!(!clean.branch.is_empty());

        std::fs::write(dir.join("dirt.txt"), "changed\n").unwrap();
        let dirty = summary(dir.to_str().unwrap()).unwrap();
        assert_eq!(dirty.dirty, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
