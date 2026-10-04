//! The kuma greeter, phase one: a headless smoke binary over the
//! greeter protocol module. Given a username (argument) and a
//! password (stdin), it runs the full greetd flow against
//! `GREETD_SOCK` and reports the outcome. The gpui front end
//! (phase two) replaces the argument and stdin parts; the login
//! flow underneath is the same code.
//!
//! Usage: `echo "$password" | kuma-greeter martin [command...]`
//! The session command defaults to the first wayland session's
//! Exec, then `niri-session`.

use std::io::BufRead;

use kuma_shell::greeter::{self, GreetdClient, LoginOutcome};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let username = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: kuma-greeter USERNAME [session command...]"))?;
    let cmd: Vec<String> = args.collect();

    // the session command: caller's choice, else the first installed
    // wayland session, else the niri default
    let default = {
        let sessions =
            greeter::wayland_sessions(std::path::Path::new("/usr/share/wayland-sessions"));
        sessions.first().map(|s| s.exec.clone())
    };
    let cmd: Vec<String> = if cmd.is_empty() {
        default
            .unwrap_or_else(|| "niri-session".to_string())
            .split_whitespace()
            .map(String::from)
            .collect()
    } else {
        cmd
    };
    let cmd: Vec<&str> = cmd.iter().map(String::as_str).collect();

    // the password comes from stdin, one line, never echoed back
    let mut password = String::new();
    std::io::stdin().lock().read_line(&mut password)?;
    let password = password.trim_end_matches(['\n', '\r']);

    let mut client = GreetdClient::connect()?;
    match greeter::login(&mut client, &username, password, &cmd)? {
        LoginOutcome::Started => {
            println!("session started: {cmd:?}");
            Ok(())
        }
        LoginOutcome::AuthFailed(description) => {
            anyhow::bail!("login failed: {description}")
        }
    }
}
