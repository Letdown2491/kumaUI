//! Subprocess reads with a hard ceiling. Every poll and verb that
//! runs a CLI (`nmcli`, `bluetoothctl`, `wpctl`, a widget source) is
//! awaited in sequence by its poll or panel, so one hung process must
//! cost seconds, not that subsystem's updates for the rest of the
//! session. Three bounds stack:
//!
//! - the `timeout` binary bounds the direct child (callers prefix it;
//!   see `timed_output` for the common shape),
//! - a process group plus a deadline kill bounds everything the child
//!   backgrounds: a pipe-holding grandchild (`sleep 999 & echo hi`)
//!   keeps stdout open past `timeout`'s SIGTERM, and plain
//!   `.output()` would wait on every writer forever,
//! - the capture is capped, so a source printing at line speed for
//!   its whole budget is not a memory bound.

use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long past the budget the reads may take to wrap up after the
/// group kill: everything died, EOF is immediate, two seconds is
/// plenty.
const GRACE: Duration = Duration::from_secs(2);

/// The common shape: `timeout <secs> <program> <args...>`, output
/// captured, capped at a mebibyte. The bluetooth acts' idiom applied
/// to every poll and verb.
pub fn timed_output(
    program: &str,
    args: &[&str],
    secs: u32,
) -> std::io::Result<Output> {
    let mut command = Command::new("timeout");
    command.arg(secs.to_string()).arg(program).args(args);
    bounded_output(command, secs, 1024 * 1024)
}

/// Run a caller-built command (a `timeout`-prefixed one, per the
/// module's bounds) to completion with the ceiling above.
pub fn bounded_output(
    mut command: Command,
    secs: u32,
    max_output_bytes: usize,
) -> std::io::Result<Output> {
    use std::os::unix::process::CommandExt;
    // its own process group: the deadline kill below must reach the
    // child's backgrounded grandchildren, which hold the pipes
    command.process_group(0);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("the child has no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("the child has no stderr"))?;
    let out_rx = reader(stdout, max_output_bytes);
    let err_rx = reader(stderr, max_output_bytes);

    let deadline = Instant::now() + Duration::from_secs(u64::from(secs) + 5);
    let mut killed = false;
    let stdout = match out_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(bytes) => bytes,
        Err(_) => {
            // the budget ran out with the pipe still open (the
            // grandchild case): close it for them, then give the
            // reader its moment to hand over what it kept
            kill_group(child.id());
            killed = true;
            match out_rx.recv_timeout(GRACE) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Err(std::io::Error::other(
                        "the process's output never ended",
                    ))
                }
            }
        }
    };
    let stderr = match err_rx.recv_timeout(if killed {
        GRACE
    } else {
        deadline.saturating_duration_since(Instant::now())
    }) {
        Ok(bytes) => bytes,
        Err(_) => {
            if !killed {
                kill_group(child.id());
            }
            match err_rx.recv_timeout(GRACE) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Err(std::io::Error::other(
                        "the process's error output never ended",
                    ))
                }
            }
        }
    };

    // reap the direct child under the same deadline. Normally it has
    // already exited (`timeout` exits when its child does, a moment
    // after the reader sees EOF), so no kill ever fires; the kill is
    // for a straggler that closed its pipes and kept running
    // (`sh -c 'exec 1>&-; sleep 999'`), and it fires only past the
    // deadline a healthy run never reaches.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group(child.id());
                break child.wait()?;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Err(err),
        }
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// A capped pipe reader on its own thread; the receiver gets the kept
/// bytes when the stream ends (or nothing, if the sender dies with
/// the read still blocked, which only the group kill unblocks).
fn reader<R: Read + Send + 'static>(mut stream: R, max: usize) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // no up-front capacity: the cap is a ceiling, not a hint, and
        // a 1 MiB reservation per reader turned every short-lived
        // spawn into a page-fault party
        let mut kept = Vec::new();
        let mut sink = [0u8; 8192];
        loop {
            match stream.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // past the cap the bytes are drained, not stored
                    let take = n.min(max.saturating_sub(kept.len()));
                    kept.extend_from_slice(&sink[..take]);
                }
            }
        }
        let _ = tx.send(kept);
    });
    rx
}

/// SIGKILL to a process group: the guarantee that nothing a bounded
/// run spawned outlives it. `kill(-pgid)` fails harmlessly (ESRCH)
/// when everything already exited.
fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_output_runs_and_bounds() {
        let output = timed_output("echo", &["hi"], 5).expect("echo runs");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");

        // a hung child costs its budget, not forever
        let start = Instant::now();
        let output = timed_output("sleep", &["30"], 2).expect("sleep runs");
        assert!(!output.status.success(), "timeout killed it");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_grandchild_holding_the_pipe_cannot_hang_the_read() {
        // the case that killed plain .output(): the direct child exits,
        // its backgrounded grandchild holds stdout open
        let mut command = Command::new("timeout");
        command.arg("2").arg("sh").arg("-c").arg("sleep 999 & echo hi");
        let start = Instant::now();
        let output = bounded_output(command, 2, 1024 * 1024).expect("the read bounds");
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hi");
    }

    #[test]
    fn the_capture_is_capped() {
        let mut command = Command::new("timeout");
        command
            .arg("10")
            .arg("sh")
            .arg("-c")
            .arg("head -c 10000000 /dev/zero");
        let output = bounded_output(command, 10, 64 * 1024).expect("the read caps");
        assert!(output.status.success());
        assert!(
            output.stdout.len() <= 64 * 1024,
            "capped at the budget, got {}",
            output.stdout.len()
        );
    }
}
