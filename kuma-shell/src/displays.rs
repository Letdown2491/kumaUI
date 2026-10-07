//! Display settings: per-output enable, mode, scale, transform,
//! position, and VRR, applied live over the compositor's IPC and
//! persisted as a field-level delta in niri's local.kdl (issue #26).
//! The settings page reads current outputs here and applies on change;
//! this module owns the wire format and the store grammar, so nothing
//! above it names niri. The store is machine state by design: never a
//! kuma.toml schema, never a full config copy, no boot-time converger.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

// ---- probe and apply: niri's IPC, hand-rolled like the adapter ----

/// Whether the display seam exists at all: the niri IPC socket is
/// discoverable. False means another compositor owns the outputs and
/// the settings page shows its honest empty state.
pub fn available() -> bool {
    crate::niri::socket_path().is_ok()
}

/// One mode an output offers. Refresh rides in millihertz, the IPC's
/// unit, so no precision is lost in the probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u16,
    pub height: u16,
    pub refresh_mhz: u32,
    pub preferred: bool,
}

/// The transform (rotation and flip) of an output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
pub enum Transform {
    #[default]
    Normal,
    #[serde(rename = "90")]
    Rotate90,
    #[serde(rename = "180")]
    Rotate180,
    #[serde(rename = "270")]
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    /// Every transform, in dropdown order.
    pub const ALL: [Transform; 8] = [
        Transform::Normal,
        Transform::Rotate90,
        Transform::Rotate180,
        Transform::Rotate270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    /// The name niri's config parser accepts for the local.kdl store.
    fn kdl_name(self) -> &'static str {
        match self {
            Transform::Normal => "normal",
            Transform::Rotate90 => "90",
            Transform::Rotate180 => "180",
            Transform::Rotate270 => "270",
            Transform::Flipped => "flipped",
            Transform::Flipped90 => "flipped-90",
            Transform::Flipped180 => "flipped-180",
            Transform::Flipped270 => "flipped-270",
        }
    }

    fn from_kdl_name(name: &str) -> Option<Transform> {
        Transform::ALL
            .iter()
            .find(|transform| transform.kdl_name() == name)
            .copied()
    }

    /// The name niri's IPC serde uses for the live apply.
    fn ipc_name(self) -> &'static str {
        match self {
            Transform::Normal => "Normal",
            Transform::Rotate90 => "90",
            Transform::Rotate180 => "180",
            Transform::Rotate270 => "270",
            Transform::Flipped => "Flipped",
            Transform::Flipped90 => "Flipped90",
            Transform::Flipped180 => "Flipped180",
            Transform::Flipped270 => "Flipped270",
        }
    }
}

/// One connected output, as the settings page sees it.
#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,
    pub make: String,
    pub model: String,
    /// False when the output is switched off: niri reports no logical
    /// geometry for it, so scale, position, and size are placeholders.
    pub enabled: bool,
    pub modes: Vec<Mode>,
    /// Index into `modes` of the mode in use, None when disabled.
    pub current_mode: Option<usize>,
    pub vrr_supported: bool,
    pub vrr_enabled: bool,
    pub scale: f64,
    pub transform: Transform,
    pub x: i32,
    pub y: i32,
    pub logical_width: u32,
    pub logical_height: u32,
}

impl Output {
    /// The one-line state under a card title: what the monitor is
    /// doing right now, or that it is off.
    pub fn state_line(&self) -> String {
        if !self.enabled {
            return "Off".to_string();
        }
        let mode = self.current_mode.and_then(|index| self.modes.get(index));
        let geometry = match mode {
            Some(mode) => format!(
                "{}x{} @ {} Hz",
                mode.width,
                mode.height,
                fmt_f64(mode.refresh_mhz as f64 / 1000.)
            ),
            None => "unknown mode".to_string(),
        };
        let mut line = format!("{geometry}, scale {}", fmt_f64(self.scale));
        if self.transform != Transform::Normal {
            line.push_str(", rotated");
        }
        if self.vrr_enabled {
            line.push_str(", VRR");
        }
        line
    }
}

/// The wire shape of niri's output probe reply, field names as the
/// niri-ipc crate spells them.
#[derive(Deserialize)]
struct RawOutput {
    name: String,
    #[serde(default)]
    make: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    modes: Vec<RawMode>,
    current_mode: Option<usize>,
    vrr_supported: bool,
    vrr_enabled: bool,
    logical: Option<RawLogical>,
}

#[derive(Deserialize)]
struct RawMode {
    width: u16,
    height: u16,
    refresh_rate: u32,
    is_preferred: bool,
}

#[derive(Deserialize)]
struct RawLogical {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    scale: f64,
    transform: Transform,
}

/// Read the connected outputs, sorted by name. Modes are sorted
/// largest-and-fastest first and deduplicated, and the current-mode
/// index is remapped to the sorted order.
pub fn probe() -> Result<Vec<Output>> {
    let reply = crate::niri::request(json!("Outputs"))?;
    let map: HashMap<String, RawOutput> = serde_json::from_value(
        reply
            .get("Outputs")
            .cloned()
            .context("niri reply missing Outputs")?,
    )?;

    let mut outputs: Vec<Output> = map
        .into_values()
        .map(|raw| {
            let mut modes: Vec<Mode> = raw
                .modes
                .iter()
                .map(|mode| Mode {
                    width: mode.width,
                    height: mode.height,
                    refresh_mhz: mode.refresh_rate,
                    preferred: mode.is_preferred,
                })
                .collect();
            // largest area first, fastest refresh first; then drop the
            // duplicates some monitors list per pixel clock
            modes.sort_by(|a, b| {
                (b.width as u32 * b.height as u32)
                    .cmp(&(a.width as u32 * a.height as u32))
                    .then(b.refresh_mhz.cmp(&a.refresh_mhz))
            });
            modes.dedup_by(|a, b| {
                a.width == b.width && a.height == b.height && a.refresh_mhz == b.refresh_mhz
            });
            // remap niri's current-mode index into the sorted vec
            let current_mode = raw.current_mode.and_then(|index| {
                raw.modes.get(index).and_then(|current| {
                    modes.iter().position(|mode| {
                        mode.width == current.width
                            && mode.height == current.height
                            && mode.refresh_mhz == current.refresh_rate
                    })
                })
            });
            match raw.logical {
                Some(logical) => Output {
                    name: raw.name,
                    make: raw.make,
                    model: raw.model,
                    enabled: true,
                    modes,
                    current_mode,
                    vrr_supported: raw.vrr_supported,
                    vrr_enabled: raw.vrr_enabled,
                    scale: logical.scale,
                    transform: logical.transform,
                    x: logical.x,
                    y: logical.y,
                    logical_width: logical.width,
                    logical_height: logical.height,
                },
                None => Output {
                    name: raw.name,
                    make: raw.make,
                    model: raw.model,
                    enabled: false,
                    modes,
                    current_mode: None,
                    vrr_supported: raw.vrr_supported,
                    vrr_enabled: false,
                    scale: 1.,
                    transform: Transform::Normal,
                    x: 0,
                    y: 0,
                    logical_width: 0,
                    logical_height: 0,
                },
            }
        })
        .collect();
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(outputs)
}

/// One user-applied setting on one output. `None` payloads mean "back
/// to auto": the line leaves the store and niri picks again.
#[derive(Clone, Debug, PartialEq)]
pub enum Pin {
    Off(bool),
    Scale(Option<f64>),
    Transform(Transform),
    Position(Option<(i32, i32)>),
    Mode(Option<ModeSpec>),
    /// Some(on_demand) pins VRR on; None pins it off.
    Vrr(Option<bool>),
}

/// A mode as the user picked it and the store keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: u16,
    pub height: u16,
    /// Millihertz; None pins the resolution and lets niri pick the
    /// refresh (the store line then has no @ part).
    pub refresh_mhz: Option<u32>,
}

/// What niri answered for an output change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The output is connected and the change took.
    Yes,
    /// The output is not connected right now; niri applies the change
    /// when it appears, and the store keeps it until then.
    OutputMissing,
}

/// Apply one setting live. The IPC change is temporary by niri's
/// design; `persist` is what makes it survive a reboot.
pub fn apply(output: &str, pin: &Pin) -> Result<Applied> {
    let request = json!({"Output": {"output": output, "action": output_action(pin)}});
    let reply = crate::niri::request(request)?;
    Ok(match reply.get("OutputConfigChanged").and_then(Value::as_str) {
        Some("OutputWasMissing") => Applied::OutputMissing,
        _ => Applied::Yes,
    })
}

fn output_action(pin: &Pin) -> Value {
    match pin {
        Pin::Off(true) => json!("Off"),
        Pin::Off(false) => json!("On"),
        Pin::Scale(Some(scale)) => json!({"Scale": {"scale": {"Specific": scale}}}),
        Pin::Scale(None) => json!({"Scale": {"scale": "Automatic"}}),
        Pin::Transform(transform) => {
            json!({"Transform": {"transform": transform.ipc_name()}})
        }
        Pin::Position(Some((x, y))) => {
            json!({"Position": {"position": {"Specific": {"x": x, "y": y}}}})
        }
        Pin::Position(None) => json!({"Position": {"position": "Automatic"}}),
        Pin::Mode(Some(spec)) => {
            json!({"Mode": {"mode": {"Specific": {
                "width": spec.width,
                "height": spec.height,
                "refresh": spec.refresh_mhz.map(|mhz| mhz as f64 / 1000.),
            }}}})
        }
        Pin::Mode(None) => json!({"Mode": {"mode": "Automatic"}}),
        Pin::Vrr(Some(on_demand)) => json!({"Vrr": {"vrr": true, "on_demand": on_demand}}),
        Pin::Vrr(None) => json!({"Vrr": {"vrr": false, "on_demand": false}}),
    }
}

// ---- arrangement: absolute positions from relative placement ----

/// Which edge of an anchor output to place another against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
}

/// The absolute position that puts `mine` (the moving output's logical
/// size) flush against one edge of the anchor's logical rect.
pub fn arrange(anchor: &Output, side: Side, mine: (u32, u32)) -> (i32, i32) {
    match side {
        Side::Left => (anchor.x - mine.0 as i32, anchor.y),
        Side::Right => (anchor.x + anchor.logical_width as i32, anchor.y),
        Side::Above => (anchor.x, anchor.y - mine.1 as i32),
        Side::Below => (anchor.x, anchor.y + anchor.logical_height as i32),
    }
}

/// Whether no output besides `name` is enabled: switching `name` off
/// (the caller knows its own toggle state) would leave the session
/// with no display at all, including the one the page itself is drawn
/// on. The enable toggle refuses that instead of blanking the user
/// out, because nothing in the session can turn a display back on
/// once every display is off.
pub fn would_leave_no_display(outputs: &[Output], name: &str) -> bool {
    !outputs
        .iter()
        .any(|output| output.enabled && output.name != name)
}

// ---- the local.kdl store ----

/// One output's persisted delta: only the fields the user pinned.
/// Absent fields follow niri's defaults, which is the point of the
/// positional include the image ships.
#[derive(Clone, Debug, Default, PartialEq)]
struct OutputBlock {
    name: String,
    off: bool,
    scale: Option<f64>,
    /// None means normal: the line is never stored for normal.
    transform: Option<Transform>,
    position: Option<(i32, i32)>,
    mode: Option<ModeSpec>,
    vrr: bool,
    vrr_on_demand: bool,
}

/// The store file: niri's local.kdl, an include the baked /etc config
/// carries last, so these blocks win over everything the image ships.
fn store_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("niri").join("local.kdl")
}

/// Read the store. A missing file is an empty store (the include is
/// optional by design); anything unparseable is an error the caller
/// must show rather than rewrite over.
fn load_store() -> Result<Vec<OutputBlock>> {
    let path = store_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => bail!("cannot read {}: {err}", path.display()),
    };
    parse_store(&text)
}

/// Pin one setting onto one output and write the store whole: read,
/// update the one output's block, write, never append. A parse failure
/// comes back as an Err and the file is left alone.
pub fn persist(output: &str, pin: &Pin) -> Result<()> {
    let mut blocks = load_store()?;
    upsert(&mut blocks, output, pin);
    write_store(&blocks)
}

/// Forget one output's delta: the block leaves the store, niri's
/// include watch reloads, and the reload drops the temporary overrides
/// the page applied live. Defaults flow again with no reboot.
pub fn reset(output: &str) -> Result<()> {
    let mut blocks = load_store()?;
    blocks.retain(|block| block.name != output);
    write_store(&blocks)?;
    // the file change alone does the work through the watch; the
    // explicit reload just skips the watcher's debounce
    let _ = crate::niri::request(json!({"Action": "LoadConfigFile"}));
    Ok(())
}

fn upsert(blocks: &mut Vec<OutputBlock>, name: &str, pin: &Pin) {
    if !blocks.iter().any(|block| block.name == name) {
        blocks.push(OutputBlock {
            name: name.to_string(),
            ..OutputBlock::default()
        });
    }
    let block = blocks
        .iter_mut()
        .find(|block| block.name == name)
        .expect("block pushed above");
    match pin {
        Pin::Off(value) => block.off = *value,
        Pin::Scale(value) => block.scale = *value,
        Pin::Transform(value) => {
            block.transform = (*value != Transform::Normal).then_some(*value)
        }
        Pin::Position(value) => block.position = *value,
        Pin::Mode(value) => block.mode = *value,
        Pin::Vrr(value) => {
            block.vrr = value.is_some();
            block.vrr_on_demand = value.unwrap_or(false);
        }
    }
    // a delta that shrank back to nothing stops being a block
    let untouched = OutputBlock {
        name: name.to_string(),
        ..OutputBlock::default()
    };
    if *block == untouched {
        blocks.retain(|block| block.name != name);
    }
}

/// The store grammar the shell writes and parses: one `output` block
/// per monitor, one known node per line, `//` comments. Anything else
/// is a parse error, because the shell rewrites the file whole and
/// must never destroy content it does not understand.
fn parse_store(text: &str) -> Result<Vec<OutputBlock>> {
    let mut blocks: Vec<OutputBlock> = Vec::new();
    let mut open: Option<OutputBlock> = None;
    let mut seen: Vec<String> = Vec::new();
    // lines of a window-rule block being collected for comparison against
    // the one rule the shell writes
    let mut rule: Option<Vec<String>> = None;
    for (no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let fail = |what: &str| anyhow::anyhow!("local.kdl line {}: {what}", no + 1);
        if let Some(lines) = &mut rule {
            if line == "}" {
                let lines = rule.take().expect("checked above");
                // the interior of the rule: the opening line is consumed
                // by the match below, comments never reach collection
                let expected: Vec<String> = KUMA_TERM_RULE
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| {
                        !line.is_empty()
                            && !line.starts_with("//")
                            && line != "window-rule {"
                            && line != "}"
                    })
                    .collect();
                if lines != expected {
                    return Err(fail(
                        "unknown window-rule (only the rule kuma-shell writes is understood)",
                    ));
                }
            } else {
                lines.push(line.to_string());
            }
            continue;
        }
        if line == "window-rule {" {
            rule = Some(Vec::new());
            continue;
        }
        if line == "}" {
            match open.take() {
                Some(block) => blocks.push(block),
                None => return Err(fail("} without an open output block")),
            }
            continue;
        }
        match open.as_mut() {
            Some(block) => {
                let node = line.split_whitespace().next().expect("non-empty line");
                if seen.iter().any(|seen| seen == node) {
                    return Err(fail("duplicate node"));
                }
                seen.push(node.to_string());
                match line {
                    "off" => block.off = true,
                    "variable-refresh-rate" => block.vrr = true,
                    _ => {
                        let (node, rest) = line
                            .split_once(' ')
                            .ok_or_else(|| fail("unknown node in output block"))?;
                        match node {
                            "scale" => {
                                let scale: f64 = rest
                                    .parse()
                                    .map_err(|_| fail("bad scale"))?;
                                if !(0.0..=10.0).contains(&scale) {
                                    return Err(fail("scale out of range"));
                                }
                                block.scale = Some(scale);
                            }
                            "transform" => {
                                let name =
                                    quoted(rest).ok_or_else(|| fail("bad transform"))?;
                                block.transform = Some(
                                    Transform::from_kdl_name(name)
                                        .ok_or_else(|| fail("bad transform"))?,
                                );
                            }
                            "mode" => {
                                let spec = quoted(rest).ok_or_else(|| fail("bad mode"))?;
                                block.mode = Some(
                                    parse_mode_string(spec)
                                        .ok_or_else(|| fail("bad mode"))?,
                                );
                            }
                            "position" => {
                                block.position = Some(parse_position(rest)?);
                            }
                            "variable-refresh-rate" => {
                                block.vrr = true;
                                match rest.strip_prefix("on-demand=") {
                                    Some("true") => block.vrr_on_demand = true,
                                    Some("false") => block.vrr_on_demand = false,
                                    _ => return Err(fail("bad variable-refresh-rate")),
                                }
                            }
                            other => {
                                return Err(fail(&format!("unknown node {other:?}")));
                            }
                        }
                    }
                }
            }
            None => {
                let rest = line
                    .strip_prefix("output ")
                    .ok_or_else(|| fail("expected an output block"))?;
                let inner = rest
                    .strip_suffix('{')
                    .ok_or_else(|| fail("missing {{ on the output line"))?
                    .trim();
                let name = quoted(inner).ok_or_else(|| fail("bad output name"))?;
                if blocks.iter().any(|block| block.name == name) {
                    return Err(fail("duplicate output block"));
                }
                open = Some(OutputBlock {
                    name: name.to_string(),
                    ..OutputBlock::default()
                });
                seen.clear();
            }
        }
    }
    if open.is_some() {
        bail!("local.kdl ended inside an output block");
    }
    if rule.is_some() {
        bail!("local.kdl ended inside a window-rule block");
    }
    Ok(blocks)
}

/// The inside of a quoted KDL string argument.
fn quoted(text: &str) -> Option<&str> {
    let text = text.trim();
    let body = text.strip_prefix('"')?.strip_suffix('"')?;
    if body.contains('"') {
        return None;
    }
    Some(body)
}

fn parse_position(rest: &str) -> Result<(i32, i32)> {
    let fail = || anyhow::anyhow!("local.kdl: bad position");
    let mut x = None;
    let mut y = None;
    for token in rest.split_whitespace() {
        if let Some(value) = token.strip_prefix("x=") {
            x = Some(value.parse::<i32>().map_err(|_| fail())?);
        } else if let Some(value) = token.strip_prefix("y=") {
            y = Some(value.parse::<i32>().map_err(|_| fail())?);
        } else {
            return Err(fail());
        }
    }
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(fail()),
    }
}

/// The config-side mode string: `WIDTHxHEIGHT` with an optional
/// `@REFRESH` in hertz, decimals allowed, exactly what niri's
/// ConfiguredMode parser accepts.
fn mode_string(spec: &ModeSpec) -> String {
    let mut text = format!("{}x{}", spec.width, spec.height);
    if let Some(mhz) = spec.refresh_mhz {
        text.push_str(&format!("@{}", fmt_f64(mhz as f64 / 1000.)));
    }
    text
}

fn parse_mode_string(text: &str) -> Option<ModeSpec> {
    let (size, refresh) = match text.split_once('@') {
        Some((size, refresh)) => (size, Some(refresh)),
        None => (text, None),
    };
    let (width, height) = size.split_once('x')?;
    let refresh_mhz = match refresh {
        Some(hz) => Some((hz.parse::<f64>().ok()? * 1000.).round() as u32),
        None => None,
    };
    Some(ModeSpec {
        width: width.parse().ok()?,
        height: height.parse().ok()?,
        refresh_mhz,
    })
}

/// A float as niri's config accepts it: up to three decimals, trailing
/// zeros trimmed ("2", "1.25", "164.966").
pub(crate) fn fmt_f64(value: f64) -> String {
    let mut text = format!("{value:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

fn escape(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"")
}

fn block_text(block: &OutputBlock) -> String {
    let mut out = format!("output \"{}\" {{\n", escape(&block.name));
    if block.off {
        out.push_str("    off\n");
    }
    if let Some(scale) = block.scale {
        out.push_str(&format!("    scale {}\n", fmt_f64(scale)));
    }
    if let Some(transform) = block.transform {
        out.push_str(&format!(
            "    transform \"{}\"\n",
            transform.kdl_name()
        ));
    }
    if let Some((x, y)) = block.position {
        out.push_str(&format!("    position x={x} y={y}\n"));
    }
    if let Some(mode) = &block.mode {
        out.push_str(&format!("    mode \"{}\"\n", mode_string(mode)));
    }
    if block.vrr {
        if block.vrr_on_demand {
            out.push_str("    variable-refresh-rate on-demand=true\n");
        } else {
            out.push_str("    variable-refresh-rate\n");
        }
    }
    out.push_str("}\n");
    out
}

/// The kuma-term window rule the shell writes into the store. kuma-term
/// composites translucently, and niri's focus ring paints a filled rect
/// behind the focused window: a translucent terminal blends over that rect
/// and the ring color bleeds through its background. This rule draws the
/// ring as an outline around the window instead. The shell rewrites the
/// store whole on every pin, so the rule rides along in every write; the
/// parser accepts exactly this block and rejects any other window-rule,
/// so a hand edit can never be destroyed silently.
const KUMA_TERM_RULE: &str = "\
// kuma-term: the ring as an outline, not a filled rect behind the window
window-rule {
    match app-id=\"kuma-term\"
    draw-border-with-background false
}";

fn serialize_store(blocks: &[OutputBlock]) -> String {
    let mut out = String::from(
        "// display settings persisted by kuma-shell; one block per monitor, only the changed fields\n",
    );
    out.push_str(KUMA_TERM_RULE);
    out.push('\n');
    for block in blocks {
        out.push_str(&block_text(block));
    }
    out
}

fn write_store(blocks: &[OutputBlock]) -> Result<()> {
    let path = store_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, serialize_store(blocks))
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("cannot move {} into place", tmp.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(width: u16, height: u16, refresh_mhz: Option<u32>) -> ModeSpec {
        ModeSpec {
            width,
            height,
            refresh_mhz,
        }
    }

    fn block(name: &str) -> OutputBlock {
        OutputBlock {
            name: name.to_string(),
            ..OutputBlock::default()
        }
    }

    #[test]
    fn store_round_trips() {
        let mut first = block("DP-1");
        first.off = true;
        first.scale = Some(1.25);
        first.transform = Some(Transform::Flipped270);
        first.position = Some((-1920, 0));
        first.mode = Some(spec(3440, 1440, Some(165004)));
        first.vrr = true;
        first.vrr_on_demand = true;
        let mut second = block("eDP-1");
        second.scale = Some(2.);
        second.mode = Some(spec(1920, 1080, None));
        let blocks = vec![first, second];
        assert_eq!(parse_store(&serialize_store(&blocks)).unwrap(), blocks);
    }

    #[test]
    fn serialized_store_is_canonical_kdl() {
        let mut pinned = block("DP-1");
        pinned.scale = Some(1.25);
        pinned.mode = Some(spec(3440, 1440, Some(165004)));
        pinned.vrr = true;
        assert_eq!(
            serialize_store(&[pinned]),
            "// display settings persisted by kuma-shell; one block per monitor, only the changed fields\n\
             // kuma-term: the ring as an outline, not a filled rect behind the window\n\
             window-rule {\n    \
             match app-id=\"kuma-term\"\n    \
             draw-border-with-background false\n\
             }\n\
             output \"DP-1\" {\n    \
             scale 1.25\n    \
             mode \"3440x1440@165.004\"\n    \
             variable-refresh-rate\n}\n"
        );
    }

    #[test]
    fn the_kuma_term_rule_survives_a_display_pin() {
        // the store the shell writes carries the kuma-term rule; a later
        // pin reads the file back and must keep the rule
        let text = serialize_store(&[block("eDP-1")]);
        let parsed = parse_store(&text).unwrap();
        assert!(parsed.is_empty() || parsed.len() == 1);
        let repinned = serialize_store(&parsed);
        assert_eq!(repinned, text);
    }

    #[test]
    fn an_unknown_window_rule_is_a_parse_error_not_silence() {
        let text = "window-rule {\n    match app-id=\"other\"\n}\n";
        assert!(parse_store(text).is_err());
        // and a mangled version of the shell's own rule too
        let mangled = serialize_store(&[]).replace("draw-border-with-background false", "");
        assert!(parse_store(&mangled).is_err());
    }

    #[test]
    fn parse_accepts_comments_and_blank_lines() {
        let parsed = parse_store(
            "// hand note\n\noutput \"DP-1\" {\n    // why not\n    scale 2\n}\n",
        )
        .unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].scale, Some(2.));
    }

    #[test]
    fn parse_refuses_unknown_content() {
        for text in [
            "output \"DP-1\" {\n    wat\n}\n",
            "output \"DP-1\" {\n    scale 1\n    scale 2\n}\n",
            "output \"DP-1\" {\n}\noutput \"DP-1\" {\n}\n",
            "output \"DP-1\" {\n",
            "}\n",
            "layout {\n    gaps 8\n}\n",
        ] {
            assert!(parse_store(text).is_err(), "should refuse: {text}");
        }
    }

    #[test]
    fn empty_text_is_an_empty_store() {
        assert_eq!(parse_store("").unwrap(), Vec::new());
    }

    #[test]
    fn pins_merge_field_level() {
        let mut blocks = vec![block("DP-1")];
        upsert(&mut blocks, "DP-1", &Pin::Mode(Some(spec(3440, 1440, Some(165004)))));
        upsert(&mut blocks, "DP-1", &Pin::Scale(Some(1.25)));
        let text = serialize_store(&blocks);
        assert!(text.contains("mode \"3440x1440@165.004\""));
        assert!(text.contains("scale 1.25"));
    }

    #[test]
    fn auto_pins_remove_their_line() {
        let mut blocks = vec![block("DP-1")];
        upsert(&mut blocks, "DP-1", &Pin::Scale(Some(1.25)));
        upsert(&mut blocks, "DP-1", &Pin::Scale(None));
        assert_eq!(blocks, Vec::new());
    }

    #[test]
    fn normal_transform_and_vrr_off_leave_no_line() {
        let mut blocks = vec![block("DP-1")];
        upsert(&mut blocks, "DP-1", &Pin::Transform(Transform::Normal));
        assert_eq!(blocks, Vec::new());
        upsert(&mut blocks, "DP-1", &Pin::Vrr(None));
        assert_eq!(blocks, Vec::new());
        upsert(&mut blocks, "DP-1", &Pin::Vrr(Some(true)));
        upsert(&mut blocks, "DP-1", &Pin::Vrr(None));
        assert_eq!(blocks, Vec::new());
    }

    #[test]
    fn off_pin_keeps_the_block() {
        let mut blocks = vec![block("DP-1")];
        upsert(&mut blocks, "DP-1", &Pin::Off(true));
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].off);
        upsert(&mut blocks, "DP-1", &Pin::Off(false));
        assert_eq!(blocks, Vec::new());
    }

    #[test]
    fn mode_strings_round_trip_through_niris_parser_shape() {
        assert_eq!(mode_string(&spec(3440, 1440, Some(165004))), "3440x1440@165.004");
        assert_eq!(mode_string(&spec(3440, 1440, Some(144000))), "3440x1440@144");
        assert_eq!(mode_string(&spec(1920, 1080, None)), "1920x1080");
        assert_eq!(
            parse_mode_string("2560x1600@165.004"),
            Some(spec(2560, 1600, Some(165004)))
        );
        assert_eq!(parse_mode_string("1920x1080"), Some(spec(1920, 1080, None)));
        assert_eq!(parse_mode_string("1920"), None);
        assert_eq!(parse_mode_string("1920x"), None);
        assert_eq!(parse_mode_string("1920x1080@"), None);
        assert_eq!(parse_mode_string("1920x1080@60Hz"), None);
    }

    #[test]
    fn floats_serialize_as_niri_config_accepts_them() {
        assert_eq!(fmt_f64(2.), "2");
        assert_eq!(fmt_f64(1.25), "1.25");
        assert_eq!(fmt_f64(0.5), "0.5");
        assert_eq!(fmt_f64(100.), "100");
        assert_eq!(fmt_f64(165004f64 / 1000.), "165.004");
    }

    #[test]
    fn transforms_speak_both_dialects() {
        for transform in Transform::ALL {
            let text = transform.kdl_name();
            assert_eq!(Transform::from_kdl_name(text), Some(transform));
            assert!(!text.contains('\"'));
        }
        assert_eq!(Transform::Rotate270.ipc_name(), "270");
        assert_eq!(Transform::Flipped270.ipc_name(), "Flipped270");
    }

    #[test]
    fn output_actions_match_niris_wire_shape() {
        assert_eq!(output_action(&Pin::Off(true)), json!("Off"));
        assert_eq!(output_action(&Pin::Off(false)), json!("On"));
        assert_eq!(
            output_action(&Pin::Scale(Some(1.25))),
            json!({"Scale": {"scale": {"Specific": 1.25}}})
        );
        assert_eq!(
            output_action(&Pin::Scale(None)),
            json!({"Scale": {"scale": "Automatic"}})
        );
        assert_eq!(
            output_action(&Pin::Transform(Transform::Rotate90)),
            json!({"Transform": {"transform": "90"}})
        );
        assert_eq!(
            output_action(&Pin::Position(Some((-1920, 0)))),
            json!({"Position": {"position": {"Specific": {"x": -1920, "y": 0}}}})
        );
        assert_eq!(
            output_action(&Pin::Position(None)),
            json!({"Position": {"position": "Automatic"}})
        );
        assert_eq!(
            output_action(&Pin::Mode(Some(spec(3440, 1440, Some(165004))))),
            json!({"Mode": {"mode": {"Specific": {
                "width": 3440, "height": 1440, "refresh": 165.004
            }}}})
        );
        assert_eq!(
            output_action(&Pin::Mode(Some(spec(1920, 1080, None)))),
            json!({"Mode": {"mode": {"Specific": {
                "width": 1920, "height": 1080, "refresh": null
            }}}})
        );
        assert_eq!(
            output_action(&Pin::Vrr(Some(false))),
            json!({"Vrr": {"vrr": true, "on_demand": false}})
        );
        assert_eq!(
            output_action(&Pin::Vrr(None)),
            json!({"Vrr": {"vrr": false, "on_demand": false}})
        );
    }

    fn anchor() -> Output {
        Output {
            name: "DP-1".to_string(),
            make: String::new(),
            model: String::new(),
            enabled: true,
            modes: Vec::new(),
            current_mode: None,
            vrr_supported: false,
            vrr_enabled: false,
            scale: 1.,
            transform: Transform::Normal,
            x: 1920,
            y: 0,
            logical_width: 1920,
            logical_height: 1080,
        }
    }

    #[test]
    fn arrangement_places_edges_flush() {
        let anchor = anchor();
        let mine = (1000, 500);
        assert_eq!(arrange(&anchor, Side::Left, mine), (920, 0));
        assert_eq!(arrange(&anchor, Side::Right, mine), (3840, 0));
        assert_eq!(arrange(&anchor, Side::Above, mine), (1920, -500));
        assert_eq!(arrange(&anchor, Side::Below, mine), (1920, 1080));
    }

    #[test]
    fn store_refuses_names_it_cannot_round_trip() {
        // quoted() accepts no escapes back: a name that needed them
        // would parse differently than it wrote, so it is refused
        // instead of misparsed. Real connector names are plain.
        let mut named = block("DP\"1\\2");
        named.scale = Some(1.5);
        let text = serialize_store(&[named]);
        assert!(text.contains("output \"DP\\\"1\\\\2\""));
        assert!(parse_store(&text).is_err());
    }

    #[test]
    fn last_display_check_counts_others_only() {
        let mut other = anchor();
        other.name = "DP-3".to_string();
        // another display is on: switching DP-1 off leaves DP-3
        assert!(!would_leave_no_display(&[anchor(), other.clone()], "DP-1"));
        // the other display is off: DP-1 is the last one on
        other.enabled = false;
        assert!(would_leave_no_display(&[anchor(), other], "DP-1"));
        // the output itself does not count as its own other display
        let mut self_off = anchor();
        self_off.enabled = false;
        assert!(would_leave_no_display(&[self_off], "DP-1"));
    }

    #[test]
    fn state_line_speaks_human() {
        let mut output = anchor();
        output.modes = vec![Mode {
            width: 3440,
            height: 1440,
            refresh_mhz: 165004,
            preferred: true,
        }];
        output.current_mode = Some(0);
        output.scale = 1.25;
        assert_eq!(
            output.state_line(),
            "3440x1440 @ 165.004 Hz, scale 1.25"
        );
        output.transform = Transform::Rotate90;
        assert!(output.state_line().contains("rotated"));
        output.vrr_enabled = true;
        assert!(output.state_line().contains("VRR"));
        output.enabled = false;
        assert_eq!(output.state_line(), "Off");
    }
}
