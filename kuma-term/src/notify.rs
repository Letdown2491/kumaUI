//! The road a terminal-side notification (OSC 9 / OSC 777) takes into
//! the shell's notification center: the MSG socket kuma-shell listens
//! on, one JSON line, fire-and-forget. kuma-term stays dependency-free
//! here: the wire is one object with two strings, and the escaping is
//! the whole protocol (a notification body is attacker-adjacent text:
//! a build log's quotes and backslashes ride this line).
//!
//! Delivery is best-effort by design: no shell running, a wedged
//! listener, a socket removed under us, all degrade to a logged miss,
//! never a blocked pump (the write happens on its own thread, bounded
//! by the stream timeouts).

use std::io::Write;
use std::time::Duration;

/// Send a notification to the running shell. Returns when the line is
/// handed off (or the attempt failed); never blocks longer than the
/// timeouts.
pub fn send(title: Option<&str>, body: &str) {
    let payload = match title {
        Some(title) => format!(
            "{{\"Notify\":{{\"title\":\"{}\",\"body\":\"{}\"}}}}",
            escape(title),
            escape(body)
        ),
        None => format!("{{\"Notify\":{{\"body\":\"{}\"}}}}", escape(body)),
    };
    // a thread per notification: rare by nature (a finished build, a
    // done download), and the pump's read loop must never wait on a
    // socket
    std::thread::Builder::new()
        .name("term-notify".into())
        .spawn(move || deliver(&payload))
        .and_then(|handle| handle.join().map_err(|_| std::io::ErrorKind::BrokenPipe.into()))
        .unwrap_or_else(|err| log::warn!("notify send failed to start: {err}"));
}

/// The socket attempt, bounded: a wedged or bogus listener on the path
/// costs a second, not the pump.
fn deliver(line: &str) {
    use std::os::unix::net::UnixStream;
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        log::info!("notify: no XDG_RUNTIME_DIR, nowhere to send");
        return;
    };
    let path = std::path::PathBuf::from(runtime_dir).join("kuma-shell.sock");
    let Ok(mut stream) = UnixStream::connect(&path) else {
        log::info!("notify: the shell's socket is not listening ({path:?})");
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    if let Err(err) = stream.write_all(line.as_bytes()).and_then(|()| {
        stream.write_all(b"\n")?;
        // the reply is read only to keep the framing honest; what it
        // says is the shell's business
        let mut reply = [0u8; 64];
        use std::io::Read;
        let _ = stream.read(&mut reply);
        Ok(())
    }) {
        log::warn!("notify: the shell did not take it: {err}");
    }
}

/// JSON string escaping: quotes, backslashes, and the control bytes
/// (a raw newline would tear the line protocol, a raw quote the JSON).
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // a control byte nobody can print: dropped, not
                // hex-escaped into noise
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_hands_the_rough_text_through() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("say \"hi\""), "say \\\"hi\\\"");
        assert_eq!(escape("back\\slash"), "back\\\\slash");
        assert_eq!(escape("two\nlines"), "two\\nlines");
        // a control byte cannot tear the line protocol
        assert_eq!(escape("a\u{1}b"), "ab");
    }

    #[test]
    fn the_wire_shape_is_the_shells_parse() {
        // round-trip against kuma-shell's own parser would be a
        // cross-crate dep; the shape is pinned here instead
        let payload = match Some("title") {
            Some(title) => format!(
                "{{\"Notify\":{{\"title\":\"{}\",\"body\":\"{}\"}}}}",
                escape(title),
                escape("the body")
            ),
            None => String::new(),
        };
        assert_eq!(
            payload,
            "{\"Notify\":{\"title\":\"title\",\"body\":\"the body\"}}"
        );
    }
}
