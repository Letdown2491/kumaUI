//! The greetd greeter client: the IPC half of the kuma greeter.
//! Phase one is the protocol layer, pure and testable: framed JSON
//! over the unix socket greetd hands its greeters in `GREETD_SOCK`,
//! the login flow driven message by message, and the session list
//! read from the wayland-sessions desktop files. No UI, no system
//! state touched; the gpui front end (phase two) hangs off these
//! functions. Protocol reference: greetd-ipc(7) on this image.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

// ---------- wire types ----------

/// A greeter request. Reference: greetd-ipc(7) "Requests".
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request<'a> {
    CreateSession { username: &'a str },
    PostAuthMessageResponse { response: Option<&'a str> },
    StartSession { cmd: Vec<&'a str>, env: Vec<&'a str> },
    CancelSession,
}

/// A greeter response. The generated JSON may carry extra fields
/// (greetd adds some over versions); serde ignores what it does not
/// know.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Success,
    Error {
        error_type: String,
        description: String,
    },
    AuthMessage {
        auth_message_type: AuthMessageType,
        auth_message: String,
    },
}

/// The four auth message kinds: a question to answer (visible or
/// secret), or a statement to acknowledge (info or error).
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
    Visible,
    Secret,
    Info,
    Error,
}

// ---------- the client ----------

/// A connection to greetd. One session conversation at a time; the
/// greeter drives it to `start_session` or cancels.
pub struct GreetdClient {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl GreetdClient {
    /// Connect through the environment, the way greetd launches
    /// greeters.
    pub fn connect() -> anyhow::Result<Self> {
        let path = std::env::var("GREETD_SOCK")
            .context("GREETD_SOCK is not set: not running under greetd")?;
        Self::connect_to(&path)
    }

    pub fn connect_to(path: &str) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to greetd at {path}"))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self { stream, reader })
    }

    /// Send one request and read its single response.
    pub fn request(&mut self, request: &Request) -> anyhow::Result<Response> {
        self.send(request)?;
        self.read_response()
    }

    fn send(&mut self, request: &Request) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(request)?;
        self.stream
            .write_all(&(payload.len() as u32).to_ne_bytes())?;
        self.stream.write_all(&payload)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Read one response: a 32-bit native-order length prefix, then
    /// that many bytes of JSON. Reading without sending is what the
    /// auth flow needs between create_session and each answer.
    pub fn read_response(&mut self) -> anyhow::Result<Response> {
        let mut length = [0u8; 4];
        self.reader.read_exact(&mut length)?;
        let length = u32::from_ne_bytes(length) as usize;
        let mut payload = vec![0u8; length];
        self.reader.read_exact(&mut payload)?;
        Ok(serde_json::from_slice(&payload)?)
    }
}

// ---------- the login flow ----------

/// How a login attempt ended: the session is running, or the auth
/// failed with greetd's description (the greeter shows it).
#[derive(Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Started,
    AuthFailed(String),
}

/// Drive one login attempt: create the session for `username`, answer
/// every auth message on the way (the password goes to secret and
/// visible questions; info and error statements are acknowledged),
/// then start `cmd`. A wrong password surfaces as `AuthFailed` and
/// the caller may simply try again: greetd ended that attempt.
pub fn login(
    client: &mut GreetdClient,
    username: &str,
    password: &str,
    cmd: &[&str],
) -> anyhow::Result<LoginOutcome> {
    let mut response = client.request(&Request::CreateSession { username })?;
    loop {
        match response {
            Response::Success => break,
            Response::Error { description, .. } => {
                return Ok(LoginOutcome::AuthFailed(description));
            }
            Response::AuthMessage {
                auth_message_type, ..
            } => {
                // a question takes the password; info and error
                // statements only want an acknowledgement
                let answer = match auth_message_type {
                    AuthMessageType::Visible | AuthMessageType::Secret => Some(password),
                    AuthMessageType::Info | AuthMessageType::Error => None,
                };
                response = client.request(&Request::PostAuthMessageResponse { response: answer })?;
            }
        }
    }
    match client.request(&Request::StartSession {
        cmd: cmd.to_vec(),
        env: vec![],
    })? {
        Response::Success => Ok(LoginOutcome::Started),
        Response::Error { description, .. } => {
            anyhow::bail!("session failed to start: {description}")
        }
        Response::AuthMessage { auth_message, .. } => {
            anyhow::bail!("unexpected auth message at start: {auth_message}")
        }
    }
}

/// Drop whatever session greetd has half-configured: the answer to a
/// cancelled attempt. Best effort by nature.
pub fn cancel(client: &mut GreetdClient) {
    let _ = client.request(&Request::CancelSession);
}

// ---------- the session list ----------

/// One wayland session the greeter can start: its display name and
/// the command greetd runs for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub exec: String,
}

/// Read the wayland sessions from `dir` (normally
/// /usr/share/wayland-sessions): every desktop entry's Name and Exec,
/// hidden entries skipped. Sorted by name so the list is stable.
pub fn wayland_sessions(dir: &Path) -> Vec<Session> {
    let mut sessions = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return sessions;
    };
    for entry in entries.flatten() {
        let Some(session) = parse_session(&entry.path()) else {
            continue;
        };
        sessions.push(session);
    }
    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    sessions
}

/// Parse one desktop file: the `[Desktop Entry]` section's Name and
/// Exec, with NoDisplay or Hidden files rejected.
fn parse_session(path: &Path) -> Option<Session> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut name = None;
    let mut exec = None;
    let mut hidden = false;
    let mut in_entry = false;
    for line in raw.lines() {
        match line.trim() {
            "[Desktop Entry]" => in_entry = true,
            "[]" | "" => in_entry = false,
            _ if in_entry => {
                let (key, value) = line.split_once('=')?;
                match key.trim() {
                    "Name" => name = Some(value.trim().to_string()),
                    "Exec" => exec = Some(value.trim().to_string()),
                    "NoDisplay" | "Hidden" if value.trim() == "true" => hidden = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if hidden {
        return None;
    }
    Some(Session {
        name: name?,
        exec: exec?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A fake greetd: one end of a socket pair, a scripted reply list
    /// served frame by frame, and the requests it received returned
    /// for assertions.
    struct FakeGreetd {
        requests: std::sync::mpsc::Receiver<serde_json::Value>,
        stream: UnixStream,
    }

    impl FakeGreetd {
        fn spawn(responses: &[serde_json::Value]) -> (GreetdClient, FakeGreetd) {
            use std::sync::mpsc;
            let (mine, theirs) = UnixStream::pair().unwrap();
            let (tx, rx) = mpsc::channel();
            let mut responses: Vec<Vec<u8>> = responses
                .iter()
                .map(|reply| serde_json::to_vec(reply).unwrap())
                .collect();
            // served in the order written: pop takes the last
            responses.reverse();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(theirs.try_clone().unwrap());
                let mut theirs = theirs;
                loop {
                    let mut length = [0u8; 4];
                    if reader.read_exact(&mut length).is_err() {
                        break;
                    }
                    let length = u32::from_ne_bytes(length) as usize;
                    let mut payload = vec![0u8; length];
                    reader.read_exact(&mut payload).unwrap();
                    tx.send(serde_json::from_slice(&payload).unwrap()).unwrap();
                    let reply = match responses.pop() {
                        Some(reply) => reply,
                        None => break,
                    };
                    let _ = theirs.write_all(&(reply.len() as u32).to_ne_bytes());
                    let _ = theirs.write_all(&reply);
                }
            });
            let reader = BufReader::new(mine.try_clone().unwrap());
            let fake = FakeGreetd { requests: rx, stream: mine.try_clone().unwrap() };
            (GreetdClient { stream: mine, reader }, fake)
        }

        fn sent(&self, index: usize) -> serde_json::Value {
            self.requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("request {index} never arrived"))
        }
    }

    fn auth_message(kind: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "auth_message",
            "auth_message_type": kind,
            "auth_message": text,
        })
    }

    fn error(kind: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "error_type": kind,
            "description": text,
        })
    }

    #[test]
    fn happy_path_answers_secret_and_starts() {
        let script = vec![
            auth_message("secret", "Password: "),
            serde_json::json!({ "type": "success" }),
            serde_json::json!({ "type": "success" }),
        ];
        let (mut client, fake) = FakeGreetd::spawn(&script);

        let outcome = login(&mut client, "martin", "hunter2", &["niri-session"]).unwrap();
        assert_eq!(outcome, LoginOutcome::Started);

        let create = fake.sent(0);
        assert_eq!(create["type"], "create_session");
        assert_eq!(create["username"], "martin");

        let answer = fake.sent(1);
        assert_eq!(answer["type"], "post_auth_message_response");
        assert_eq!(answer["response"], "hunter2");

        let start = fake.sent(2);
        assert_eq!(start["type"], "start_session");
        assert_eq!(start["cmd"], serde_json::json!(["niri-session"]));
    }

    #[test]
    fn wrong_password_reads_as_auth_failed() {
        let script = vec![
            auth_message("secret", "Password: "),
            error("auth_error", "Authentication failure"),
        ];
        let (mut client, _fake) = FakeGreetd::spawn(&script);

        let outcome = login(&mut client, "martin", "nope", &["niri-session"]).unwrap();
        assert_eq!(
            outcome,
            LoginOutcome::AuthFailed("Authentication failure".into())
        );
    }

    #[test]
    fn info_messages_are_acknowledged_without_password() {
        let script = vec![
            auth_message("info", "Password expires today"),
            auth_message("secret", "Password: "),
            serde_json::json!({ "type": "success" }),
            serde_json::json!({ "type": "success" }),
        ];
        let (mut client, fake) = FakeGreetd::spawn(&script);

        let outcome = login(&mut client, "martin", "hunter2", &["niri-session"]).unwrap();
        assert_eq!(outcome, LoginOutcome::Started);

        let create = fake.sent(0);
        assert_eq!(create["username"], "martin");
        let ack = fake.sent(1);
        assert_eq!(ack["type"], "post_auth_message_response");
        assert_eq!(ack["response"], serde_json::Value::Null);
        assert_eq!(fake.sent(2)["response"], "hunter2");
    }

    #[test]
    fn pam_error_message_is_surfaced_not_answered_with_the_password() {
        // some PAM stacks report failure as an auth_message of type
        // error rather than an error response: the greeter must not
        // feed the password back into it
        let script = vec![
            auth_message("secret", "Password: "),
            auth_message("error", "Authentication token manipulation error"),
            error("auth_error", "Authentication failure"),
        ];
        let (mut client, fake) = FakeGreetd::spawn(&script);

        let outcome = login(&mut client, "martin", "hunter2", &["niri-session"]).unwrap();
        assert!(matches!(outcome, LoginOutcome::AuthFailed(_)));
        // the password went to the question, the error statement got
        // an acknowledgement with no password in it
        assert_eq!(fake.sent(0)["username"], "martin");
        assert_eq!(fake.sent(1)["response"], "hunter2");
        assert_eq!(fake.sent(2)["response"], serde_json::Value::Null);
    }

    #[test]
    fn framing_is_length_prefixed_json() {
        let script = vec![serde_json::json!({ "type": "success" })];
        let (mut client, fake) = FakeGreetd::spawn(&script);

        let response = client
            .request(&Request::StartSession {
                cmd: vec!["niri-session"],
                env: vec![],
            })
            .unwrap();
        assert!(matches!(response, Response::Success));
        assert_eq!(fake.sent(0)["type"], "start_session");
    }

    #[test]
    fn wayland_sessions_parses_and_sorts() {
        let dir = std::env::temp_dir().join(format!("kuma-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("zshell.desktop"),
            "[Desktop Entry]\nName=Z Shell\nExec=zshell --session\nType=Application\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("niri.desktop"),
            "[Desktop Entry]\nName=Niri\nExec=niri-session\nType=Application\nDesktopNames=niri\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("hidden.desktop"),
            "[Desktop Entry]\nName=Hidden\nExec=hidden\nNoDisplay=true\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.desktop"), "garbage\n").unwrap();

        let sessions = wayland_sessions(&dir);
        assert_eq!(
            sessions,
            vec![
                Session {
                    name: "Niri".into(),
                    exec: "niri-session".into()
                },
                Session {
                    name: "Z Shell".into(),
                    exec: "zshell --session".into()
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_session_dir_reads_as_empty() {
        assert!(wayland_sessions(Path::new("/nonexistent-kuma-sessions")).is_empty());
    }
}
