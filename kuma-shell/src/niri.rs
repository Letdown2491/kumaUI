//! The niri adapter: translates niri's IPC socket into the neutral
//! session events and commands (`session.rs`). Nothing above this
//! module may mention niri.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};

use anyhow::{Context as _, Result, bail};
use gpui::{App, Entity};
use log::error;
use serde::Deserialize;
use serde_json::{Value, json};
use smol::channel::Sender;

use crate::session::{SessionEvent, SessionState};

#[derive(Deserialize)]
struct WorkspaceActivatedEvent {
    id: u64,
    focused: bool,
}

#[derive(Deserialize)]
struct WindowClosedEvent {
    id: u64,
}

#[derive(Deserialize)]
struct WindowFocusChangedEvent {
    id: Option<u64>,
}

#[derive(Deserialize)]
struct CastStoppedEvent {
    stream_id: u64,
}

pub fn connect(state: &Entity<SessionState>, cx: &mut App) {
    crate::session::mirror(state, "niri", cx, run_event_stream);
}

pub(crate) fn socket_path() -> Result<PathBuf> {
    kuma_platform::niri::socket_path()
}

/// A ceiling on one IPC line, the same insurance the sway reader
/// carries: a broken peer must not balloon the line buffer without
/// limit. Real niri events never approach this.
const MAX_IPC_PAYLOAD: u32 = 2 * 1024 * 1024;

/// One newline-terminated line, capped: a broken peer must not
/// balloon the line buffer without limit (the same insurance the
/// sway reader carries; real niri events never approach the cap). A
/// line past the cap, or EOF mid-line, ends the attempt: a
/// truncated line cannot re-sync the stream.
fn read_capped_line<R: BufRead>(reader: &mut R) -> Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        // the chunk's borrow is confined to this block; the consume
        // happens after it ends
        let (take, line_done, eof) = {
            let chunk = match reader.fill_buf() {
                Ok(chunk) => chunk,
                // a signal mid-read retries, it does not end the attempt
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err.into()),
            };
            let (take, line_done) = match chunk.iter().position(|byte| *byte == b'\n') {
                Some(pos) => {
                    buf.extend_from_slice(&chunk[..=pos]);
                    (pos + 1, true)
                }
                None => {
                    buf.extend_from_slice(chunk);
                    (chunk.len(), false)
                }
            };
            (take, line_done, chunk.is_empty())
        };
        reader.consume(take);
        if buf.len() > MAX_IPC_PAYLOAD as usize {
            bail!("niri sent a line past the {MAX_IPC_PAYLOAD}-byte payload cap");
        }
        if line_done {
            return Ok(Some(String::from_utf8(buf)?));
        }
        if eof {
            // EOF: an unterminated tail is not a line to parse
            return if buf.is_empty() {
                Ok(None)
            } else {
                bail!("niri closed the event stream mid-line")
            };
        }
    }
}

fn run_event_stream(event_tx: Sender<SessionEvent>) -> Result<()> {
    let path = socket_path()?;
    let stream = UnixStream::connect(&path).context("failed to connect to niri IPC socket")?;

    let mut writer = &stream;
    writer.write_all(b"\"EventStream\"\n")?;

    let mut reader = BufReader::new(stream);
    let Some(reply) = read_capped_line(&mut reader)? else {
        bail!("niri closed the event stream before the reply");
    };
    if !reply.starts_with("{\"Ok\"") {
        bail!("unexpected niri IPC reply: {reply}");
    }

    loop {
        let Some(line) = read_capped_line(&mut reader)? else {
            bail!("niri closed the event stream");
        };
        // One bad line must not cost the stream: a reshaped event in
        // some future niri would otherwise end the mirror for the
        // whole session. Skip it and let the next event re-sync. EOF
        // and socket errors above still end the attempt; the mirror
        // reconnects (session.rs).
        match parse_event(&line) {
            Ok(Some(event)) => event_tx.send_blocking(event)?,
            Ok(None) => {}
            Err(err) => error!("niri event skipped, parse failed: {err:#}"),
        }
    }
}

fn parse_event(line: &str) -> Result<Option<SessionEvent>> {
    let value: Value = serde_json::from_str(line)?;
    let Some((key, payload)) = value
        .as_object()
        .and_then(|object| object.into_iter().next())
    else {
        return Ok(None);
    };
    let event = match key.as_str() {
        "WorkspacesChanged" => SessionEvent::Workspaces(serde_json::from_value(
            payload["workspaces"].clone(),
        )?),
        "WorkspaceActivated" => {
            let event: WorkspaceActivatedEvent = serde_json::from_value(payload.clone())?;
            SessionEvent::WorkspaceActivated {
                id: event.id,
                focused: event.focused,
            }
        }
        "WindowsChanged" => {
            let windows: Vec<crate::session::SessionWindow> =
                serde_json::from_value(payload["windows"].clone())?;
            SessionEvent::Windows(
                windows
                    .into_iter()
                    .map(|window| (window.id, window))
                    .collect(),
            )
        }
        "WindowOpenedOrChanged" => SessionEvent::WindowOpenedOrChanged(serde_json::from_value(
            payload["window"].clone(),
        )?),
        "WindowClosed" => SessionEvent::WindowClosed(
            serde_json::from_value::<WindowClosedEvent>(payload.clone())?.id,
        ),
        "WindowFocusChanged" => SessionEvent::WindowFocusChanged(
            serde_json::from_value::<WindowFocusChangedEvent>(payload.clone())?.id,
        ),
        "CastsChanged" => SessionEvent::Casts(serde_json::from_value(payload["casts"].clone())?),
        "CastStartedOrChanged" => SessionEvent::CastStartedOrChanged(serde_json::from_value(
            payload["cast"].clone(),
        )?),
        "CastStopped" => SessionEvent::CastStopped(
            serde_json::from_value::<CastStoppedEvent>(payload.clone())?.stream_id,
        ),
        "KeyboardLayoutsChanged" => SessionEvent::KeyboardLayouts(serde_json::from_value(
            payload["keyboard_layouts"].clone(),
        )?),
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// Send one request line and return niri's Ok payload. The error path
/// keeps niri's own answer text, which is what makes IPC failures
/// diagnosable in the journal. The envelope lives in the platform
/// crate; this stays the app's seam (nothing above this module
/// mentions niri).
pub(crate) fn request(request: Value) -> Result<Value> {
    kuma_platform::niri::request(&request)
}

/// Send one action request and check niri's reply envelope.
fn send_action(action: Value) -> Result<()> {
    request(json!({"Action": action}))?;
    Ok(())
}

/// Focus a workspace by its internal id: the bar's click action.
pub fn focus_workspace(id: u64) -> Result<()> {
    send_action(json!({"FocusWorkspace": {"reference": {"Id": id}}}))
}

/// Focus a workspace by its 1-based number, the MSG CLI path, where the
/// human (or keybind) speaks in workspace numbers, not niri's internal ids.
pub fn focus_workspace_index(index: u32) -> Result<()> {
    send_action(json!({"FocusWorkspace": {"reference": {"Index": index}}}))
}

/// Focus one window: the dock's click action.
pub fn focus_window(id: u64) -> Result<()> {
    send_action(json!({"FocusWindow": {"id": id}}))
}

/// Force-stop a screencast session: the bar's sharing indicator click.
/// Niri stops PipeWire sessions only; a wlr-screencopy consumer owns
/// its stream, and the indicator never offers the click for one.
pub fn stop_cast(session_id: u64) -> Result<()> {
    send_action(json!({"StopCast": {"session_id": session_id}}))
}

/// Power the monitors off: the idle watcher's ScreenOff clause. Niri
/// wakes the outputs at the next input.
pub fn power_off_monitors() -> Result<()> {
    send_action(json!({"PowerOffMonitors": null}))
}

/// Switch the keyboard layout: next, prev, or a 0-based index into the
/// configured list. The wire's struct-variant shape (`{"layout": …}`)
/// is niri's own serde: a bare `{"SwitchLayout":"Next"}` reads as a
/// parse error, this does not.
pub fn switch_layout(target: crate::session::LayoutTarget) -> Result<()> {
    send_action(switch_layout_action(target))
}

/// The action's wire value, pure for the test that pins it: the
/// shape took a live probe to get right, and a future niri must not
/// take it apart unnoticed.
fn switch_layout_action(target: crate::session::LayoutTarget) -> Value {
    let layout = match target {
        crate::session::LayoutTarget::Next => json!("Next"),
        crate::session::LayoutTarget::Prev => json!("Prev"),
        crate::session::LayoutTarget::Index(index) => json!({"Index": index}),
    };
    json!({"SwitchLayout": {"layout": layout}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_line_and_reports_the_clean_eof() {
        let mut reader = BufReader::new(&b"first\nsecond\n"[..]);
        assert_eq!(read_capped_line(&mut reader).unwrap(), Some("first\n".into()));
        assert_eq!(read_capped_line(&mut reader).unwrap(), Some("second\n".into()));
        // a clean EOF reads None: the caller ends the attempt
        assert_eq!(read_capped_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn a_line_past_the_cap_ends_the_attempt() {
        let long = "x".repeat(MAX_IPC_PAYLOAD as usize + 1);
        let mut reader = BufReader::new(long.as_bytes());
        assert!(read_capped_line(&mut reader)
            .unwrap_err()
            .to_string()
            .contains("payload cap"));
    }

    #[test]
    fn an_unterminated_tail_is_not_a_line_to_parse() {
        // a peer that dies mid-line (or mid-cap) must end the
        // attempt, not hand a truncated line to the parser
        let mut reader = BufReader::new(&b"{\"WorkspacesChanged\""[..]);
        assert!(read_capped_line(&mut reader).is_err());
    }

    #[test]
    fn a_line_split_across_reads_still_parses() {
        // BufReader over a pipe hands out whatever arrived; the
        // reader must reassemble across fills
        let mut reader = BufReader::with_capacity(4, &b"{\"a\":1}\n{\"b\":2}\n"[..]);
        assert_eq!(read_capped_line(&mut reader).unwrap(), Some("{\"a\":1}\n".into()));
        assert_eq!(read_capped_line(&mut reader).unwrap(), Some("{\"b\":2}\n".into()));
    }

    #[test]
    fn parses_workspaces_changed_event() {
        let event = parse_event(
            r#"{"WorkspacesChanged":{"workspaces":[{"id":4,"idx":3,"name":null,"output":"eDP-1","is_urgent":false,"is_active":false,"is_focused":false,"active_window_id":null}]}}"#,
        )
        .unwrap();
        match event {
            Some(SessionEvent::Workspaces(workspaces)) => {
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].id, 4);
                assert_eq!(workspaces[0].idx, 3);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_focus_and_close_events() {
        assert!(matches!(
            parse_event(r#"{"WindowFocusChanged":{"id":null}}"#).unwrap(),
            Some(SessionEvent::WindowFocusChanged(None))
        ));
        assert!(matches!(
            parse_event(r#"{"WindowClosed":{"id":12}}"#).unwrap(),
            Some(SessionEvent::WindowClosed(12))
        ));
    }

    #[test]
    fn ignores_unknown_events() {
        assert!(matches!(
            parse_event(r#"{"ConfigLoaded":{"failed":false}}"#).unwrap(),
            None
        ));
        assert!(parse_event("not json").is_err());
    }

    #[test]
    fn parses_keyboard_layouts_event() {
        // the real niri 26.04 shape, straight off the event stream:
        // the layouts ride in a `keyboard_layouts` field, and the
        // action wire's struct-variant spelling is pinned here so the
        // next niri cannot silently reshape it
        let event = parse_event(
            r#"{"KeyboardLayoutsChanged":{"keyboard_layouts":{"names":["English (US)","German"],"current_idx":1}}}"#,
        )
        .unwrap();
        match event {
            Some(SessionEvent::KeyboardLayouts(layouts)) => {
                assert_eq!(layouts.names, vec!["English (US)", "German"]);
                assert_eq!(layouts.current_idx, 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // the layouts arrive as serde defaults: an event that omits
        // `names` is an empty list, not an error
        let event = parse_event(r#"{"KeyboardLayoutsChanged":{"keyboard_layouts":{}}}"#).unwrap();
        match event {
            Some(SessionEvent::KeyboardLayouts(layouts)) => assert!(layouts.names.is_empty()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn the_switch_layout_action_builds_niris_struct_variant() {
        // the wire shape this probed out: `SwitchLayout` is a struct
        // variant in niri's Action enum, so every spelling without the
        // `layout` field answers {"Err":"error parsing request"}
        let action = switch_layout_action(crate::session::LayoutTarget::Next);
        assert_eq!(action, json!({"SwitchLayout": {"layout": "Next"}}));
        let action = switch_layout_action(crate::session::LayoutTarget::Index(2));
        assert_eq!(action, json!({"SwitchLayout": {"layout": {"Index": 2}}}));
    }

    #[test]
    fn parses_cast_events() {
        // Real niri 26.04 shapes, CastStartedOrChanged included: after
        // the connect-time CastsChanged snapshot, niri updates the
        // mirror incrementally and the state has to keep up.
        assert!(matches!(
            parse_event(
                r#"{"CastsChanged":{"casts":[{"stream_id":1,"session_id":2,"kind":"PipeWire","target":{"Output":{"name":"DP-1"}},"is_dynamic_target":false,"is_active":true,"pid":null,"pw_node_id":77}]}}"#
            )
            .unwrap(),
            Some(SessionEvent::Casts(ref casts))
                if casts.len() == 1
                    && casts[0].stream_id == 1
                    && casts[0].session_id == 2
                    && casts[0].kind == crate::session::CastKind::PipeWire
        ));
        assert!(matches!(
            parse_event(
                r#"{"CastStartedOrChanged":{"cast":{"stream_id":3,"session_id":4,"kind":"WlrScreencopy","target":{"Window":{"id":5}},"is_dynamic_target":false,"is_active":true,"pid":4242,"pw_node_id":null}}}"#
            )
            .unwrap(),
            Some(SessionEvent::CastStartedOrChanged(ref cast))
                if cast.stream_id == 3
                    && matches!(cast.target, crate::session::CastTarget::Window { id: 5 })
                    && cast.kind == crate::session::CastKind::WlrScreencopy
                    && !cast.kind.stoppable()
        ));
        assert!(matches!(
            parse_event(r#"{"CastStopped":{"stream_id":9}}"#).unwrap(),
            Some(SessionEvent::CastStopped(9))
        ));
    }

    #[test]
    fn reshaped_payloads_are_parse_errors_the_stream_skips() {
        // A missing required field in a known event (say, a future
        // niri reshaping its payloads) parses as an error; the stream
        // loop logs and moves on instead of ending the mirror. The
        // niri #4044 class of breakage, one new event variant killing
        // every strict client, must not happen here twice.
        assert!(parse_event(r#"{"WorkspacesChanged":{"workspaces":{}}}"#).is_err());
        assert!(parse_event(r#"{"WindowClosed":{}}"#).is_err());
    }
}
