//! The single-instance activation socket: the first instance listens
//! here, later ones hand their request over and exit, so two copies
//! never fight over one job. The socket is keyed by app id, the
//! request is one line of the app's own choosing (Koguma carries a
//! path, a bare line is its "bring the window forward" poke), and the
//! stale-socket recovery is the crashed-instance case: a connect
//! refused removes the file and rebinds.

use std::io::Write as _;
use std::path::PathBuf;

/// The activation socket's path, keyed by app id: the session runtime
/// dir when the session names one, a uid-suffixed tmp file otherwise.
fn socket_path_in(runtime: Option<std::ffi::OsString>, app_id: &str) -> PathBuf {
    match runtime {
        Some(runtime) => PathBuf::from(runtime).join(format!("{app_id}.sock")),
        None => PathBuf::from(format!(
            "/tmp/{app_id}-{}.sock",
            unsafe { libc::getuid() }
        )),
    }
}

pub fn socket_path(app_id: &str) -> PathBuf {
    socket_path_in(std::env::var_os("XDG_RUNTIME_DIR"), app_id)
}

/// Either we own the single-instance socket or another instance
/// already does. In the handed-over case this process exits after
/// sending the request, so the return value is always Some here.
fn become_single_instance_in(
    app_id: &str,
    request: &str,
    runtime: Option<std::ffi::OsString>,
) -> Option<std::os::unix::net::UnixListener> {
    let path = socket_path_in(runtime, app_id);

    // someone else's session: send the request over and leave
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&path) {
        let _ = writeln!(stream, "{request}");
        std::process::exit(0);
    }

    // stale socket from a crashed instance (connect refused): remove
    // and rebind
    let _ = std::fs::remove_file(&path);
    match std::os::unix::net::UnixListener::bind(&path) {
        Ok(listener) => {
            log::info!("single instance: listening on {}", path.display());
            Some(listener)
        }
        Err(err) => {
            // cannot listen and cannot hand over: run anyway, the
            // worst case is two instances
            log::error!("single instance bind {}: {err}", path.display());
            None
        }
    }
}

/// The app-facing single-instance gate: the app id keys the socket,
/// the request line is what a second instance hands the first.
pub fn become_single_instance(
    app_id: &str,
    request: &str,
) -> Option<std::os::unix::net::UnixListener> {
    become_single_instance_in(app_id, request, std::env::var_os("XDG_RUNTIME_DIR"))
}

/// Accept loop on a plain thread (blocking is fine there), forwarded
/// through an async channel so the UI thread only wakes per
/// activation. Each activation is one line's text, trimmed: the app
/// decides what it means. One small message per connection: a client
/// that connects and never writes would otherwise stall every later
/// activation, and an unbounded read would let it balloon the pump's
/// memory.
pub fn pump<F>(listener: std::os::unix::net::UnixListener, cx: &mut gpui::App, on_activation: F)
where
    F: FnMut(&str, &mut gpui::App) + 'static,
{
    use futures::StreamExt as _;

    let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
    std::thread::Builder::new()
        .name("activation-pump".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(_) => return,
                };
                use std::io::Read as _;
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
                let mut line = String::new();
                if (&mut stream).take(4096).read_to_string(&mut line).is_err() {
                    continue;
                }
                if tx.unbounded_send(line.trim().to_string()).is_err() {
                    return;
                }
            }
        })
        .expect("spawn activation pump");

    let mut on_activation = on_activation;
    cx.spawn(async move |cx| {
        while let Some(text) = rx.next().await {
            cx.update(|cx| on_activation(&text, cx));
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_lives_in_the_runtime_dir_or_a_uid_tmp_file() {
        let named = socket_path_in(Some("/run/user/1000".into()), "kuma-test");
        assert_eq!(named, PathBuf::from("/run/user/1000/kuma-test.sock"));

        let fallback = socket_path_in(None, "kuma-test");
        assert_eq!(
            fallback,
            PathBuf::from(format!("/tmp/kuma-test-{}.sock", unsafe { libc::getuid() }))
        );
    }

    #[test]
    fn a_stale_socket_is_removed_and_rebound() {
        let runtime = std::env::temp_dir();
        let app_id = "kuma-test-stale";
        let path = socket_path_in(Some(runtime.clone().into_os_string()), app_id);
        let _ = std::fs::remove_file(&path);

        // a listener that existed and died leaves the file behind: a
        // crashed instance's signature
        drop(std::os::unix::net::UnixListener::bind(&path).expect("stale bind"));

        let listener =
            become_single_instance_in(app_id, "", Some(runtime.into_os_string()))
                .expect("stale socket recovers");
        assert!(listener.local_addr().is_ok());
        // the rebound socket is live: a second bind cannot take it
        assert!(std::os::unix::net::UnixListener::bind(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
