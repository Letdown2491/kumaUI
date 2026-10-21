# Hardening audit, 2026-10-21

The Phase 1 read-only audit from `docs/hardening-polish-plan.md`. Five
parallel passes (widgets and msg IPC, subprocess edges, the
lock/PAM/greeter chain, the kuma-term parser, parsers and Koguma file
ops), every finding verified against the working tree, the
highest-impact ones re-verified by hand before this report was written.
Findings marked (verified) were confirmed by reading the quoted code in
this repo; the two dependency findings were confirmed against the
dependency sources in the cargo registry.

Severity meanings, per the plan: **BORING** fixes land in 0.5.1
(caps, timeouts, permissions, panic removal, graceful degradation, no
signature change). **ARCHITECTURAL** findings are parked in the 0.6.0
list at the bottom. **INFORMATIONAL** findings are by design and get a
document line at most.

The trust boundary throughout is the user account: the shell runs as
the user, on a single-user desktop OS, so a user corrupting their own
config is not a finding unless it panics, wedges, escalates, or
destroys data. "Escalate" here means anything another local account or
remote content could influence.

## Triage order for 0.5.1

1. **Auth bypass-shaped: lock.rs empty-stack accept** (A2)
2. **Password handling: greeter echo-on prompts** (A4)
3. **Shell crashes: widget interval panic** (W1)
4. **Data loss: Koguma replace-move and lost undo** (K3, K4)
5. **Argument injection: tar without `--`** (K5)
6. **Wedges: the five missing timeouts** (S1, S2, S6, S7, A5)
7. **Memory holes: widget stdout, curl bodies, avatar download** (W2, S3, S4)
8. **IPC hardening: socket perms, client timeout, activation peer check** (W-msg, K8)
9. **Password hygiene: zeroize, polkit -k, greetd timeout** (A3, A6, A7)
10. **kuma-term: URL scheme allowlist, OSC 52 flood, defensive indexes** (T2, T3, T4)
11. **State files: atomic saves, newline paths, partial archives** (K1, K2, K7)

## A. Lock screen, PAM, greeter

### A1. ARCHITECTURAL: no timeout on the lock screen's in-process PAM conversation
`kuma-shell/src/lock.rs:82-83`. `context.authenticate(...)` and
`acct_mgmt` run in-process (pam-client2), no timeout, no cancel.
`attempt()` sets `busy = true` and `handle_key` returns early while
busy, so a wedging PAM module (hung SSSD/LDAP lookup, broken
`pam_exec`) turns the lock screen into an input-dead surface until
reboot or SSH `loginctl unlock-session`. polkit.rs defends against
exactly this with `HELPER_EXCHANGE_TIMEOUT` (polkit.rs:864); lock.rs
does not. Fix options (race the call against a timer and abandon the
thread, or move to the polkit helper-process shape) both touch the
seam: parked for 0.6.0.

### A2. BORING: the PAM chain's existence check accepts an empty or comment-only stack file (verified)
`kuma-shell/src/lock.rs:70-72`. The gate is
`Path::new("/etc/pam.d").join(service).exists()`. A service file that
exists but carries no applicable `auth` rules is an empty stack, and an
empty auth stack does not reject: `pam_authenticate` returns success
having run nothing. A truncated deploy (zero-length `kuma-lock` left by
a failed atomic swap, an editor writing only comments) would accept any
password at the lock screen. Fix: before using a service, read the file
and require at least one non-comment line beginning with `auth`;
otherwise treat it as missing and skip, same semantics. Verify the
empty-stack-success behavior with a manual PAM test when fixing.

### A3. BORING: password material is never zeroized
`kuma-shell/src/lock.rs:100` (`password: String`), `:259`
(`std::mem::take` into the spawn), `:74` (`CString` copy per attempt),
`:47` (conversation clone per prompt). Buffers are `.clear()`ed but the
heap is never scrubbed, so a core dump or swap could recover typed
passwords. Fix: the `zeroize` crate (`Zeroizing` for the CString path,
`ZeroizeOnDrop` for the state buffer). Copies inside gpui keystroke
events remain transient; document that.

### A4. BORING: the greeter answers echo-on (visible) PAM prompts with the password (verified)
`kuma-shell/src/greeter.rs:163-167`. `AuthMessageType::Visible |
AuthMessageType::Secret => Some(password)`. Its siblings do not:
polkit.rs:947-955 answers echo-on with `""` and lock.rs refuses
`prompt_echo_on` outright. A login-chain module that asks an echo-on
question ("login:", a confirmation) receives the password where it can
be echoed or logged. Fix: answer only `Secret` with the password;
`Visible` gets an empty string, matching polkit.rs.

### A5. BORING: greetd socket reads have no timeout
`kuma-shell/src/greeter.rs:74-79`, `:101-112`. The frame handling is
otherwise exemplary (1 MiB cap checked before allocation, truncation
errors, 16-message auth bound). A wedged greetd pins the greeter's
background thread with the UI stuck at "signing in...". Fix:
`set_read_timeout(Some(30s))` at connect, one line, mirrors
polkit.rs:870-871.

### A6. BORING: polkit helper runs under `timeout` without `-k`
`kuma-shell/src/polkit.rs:891-897`, `:914`. A helper (or its PAM
module) that hangs and ignores SIGTERM is never SIGKILLed, `wait()`
blocks forever, and the polkit card stays "checking..." with no retry
until shell restart. Fix: `timeout -k 5 30`.

### A7. INFORMATIONAL: the failure-logging surface is clean
`lock.rs:273` logs only `err:#` from `authenticate`, which can be a NUL
note, a `pam_strerror` code, or the chain-fall-through message; the
password is never formatted. polkit.rs logs only protocol/io text; the
password travels only to stdin/socket. The greeter logs greetd protocol
errors only. No change; this paragraph is the audit trail.

### A8. INFORMATIONAL: fall-through when every PAM service is missing rejects
`lock.rs:89-92`: all three services missing ends in an error, not an
accept; unreadable or garbage service files reject at `authenticate`.
The one exception is A2's empty-file case.

### A9. INFORMATIONAL: greeter fallback reads are unwrap-free
Settings load falls back to defaults on any error (settings.rs:1071-
1088); the wallpaper falls back to the image default, a decode error
leaves the dark base; sessions degrade to `niri-session`. Nothing to
fix.

### A10. INFORMATIONAL: stale doc comment in polkit.rs
`polkit.rs:44-46` says the setuid spawn fallback is "deliberately not
implemented"; `polkit.rs:884-916` implements it in full. Fix the
comment (docs-only, rides along with A6).

### A11. INFORMATIONAL: greeter deploy doc leaves the wrapper-script location unspecified
`docs/greeter-deploy.md:65-66` suggests setting `XDG_CACHE_HOME` via a
wrapper script without saying it must be root-owned under `/usr`. One
sentence fixes it.

## B. Widgets and msg IPC

### W1. BORING: `Duration::from_secs_f64` panics on a hostile manifest interval (verified)
`kuma-shell/src/widgets.rs:199-202`. `interval = -1`, `nan`, `inf`, or
`1e300` in a `widget.toml` panics inside `load` → `discover`, which
runs on the main thread at startup and on every watcher generation
bump. One broken manifest takes down the whole shell, contradicting the
module's own contract (lines 119-122) that one broken widget must not
blank the others. Fix: `.filter(|v| v.is_finite() && *v >= 0.0)` before
conversion, falling back to `DEFAULT_INTERVAL`.

### W2. BORING: widget stdout is not size-capped
`widgets.rs:397-404` (`command.output()` collects everything), cached
unbounded in `reads` (`:506`) and rendered whole (`bar.rs:1258-1317`).
A source printing at 100 MB/s for its 60 s budget is a 6 GB allocation.
Fix: cap the read (`.take(64 KiB)` on stdout, drain the rest), or
truncate in `parse_output`.

### W3. BORING: a pipe-holding grandchild wedges the widget's tick slot forever
`widgets.rs:397-399`. `timeout` kills only its direct child;
`command = "sleep 99999 & echo hi"` keeps the stdout pipe open, so
`output()` blocks past the timeout, the widget stays `in_flight` and
never updates again, and a background thread is pinned per tick. Fix:
bounded read with an explicit kill, or run the child in its own process
group and kill the group.

### W4. BORING: `widget.toml` read has no size cap and runs on the main thread
`widgets.rs:161`, reached from `entity.update` (`:586`, `:533`). A
widget dir whose `widget.toml` is a FIFO blocks the whole shell; a
multi-GB regular file is slurped whole. Fix: `metadata()` first,
require `is_file()`, cap the read; moving `discover` off the main
thread is the 0.6.0-shaped version.

### W5. BORING: the msg socket's permissions are assumed, never enforced
`kuma-shell/src/msg.rs:207-212`. The listener relies on
`XDG_RUNTIME_DIR` being 0700. Fix: `fs::set_permissions(&path, 0o600)`
after bind (or bind inside a 0700 subdir the shell creates).

### W6. BORING: the msg client half has neither timeout nor reply cap
`msg.rs:149-162`. The server has both (`:179`, `:183`, unit-tested);
the client's `read_line` hangs forever against a wedged server and
grows without bound against a bogus listener on the socket path. Fix:
mirror the server: `set_read_timeout` plus `.take(MAX_REQUEST_LINE)`.

### W7. BORING: `to_line` hand-rolls JSON escaping and misses control characters
`msg.rs:106-112`. Only `\` and `"` are escaped; a `nostr` URI argv
containing a raw control byte emits invalid JSON, and the server
silently drops the request with a misleading "unknown request" reply.
Fix: serialize with serde_json.

### W8. BORING: the msg accept loop is single-threaded with a 2 s per-peer read window
`msg.rs:213-225`. N peers that connect and send nothing stall all IPC
for 2N seconds, serially. Not a wedge; responsiveness degrades. Fix:
shorten the per-peer window (real clients write immediately) or thread
per connection; at minimum a comment.

### W9. INFORMATIONAL: the icon path may contain `..`
`widgets.rs:209-213` rejects only absolute icon paths; `icon =
"../../x.png"` renders a file from anywhere the user can read. Same
user, no escalation; aligning the icon with `source`'s
`resolve_relative` containment is cheap defense in depth.

### W10. INFORMATIONAL: `source` follows symlinks out of the widget directory
`widgets.rs:239-243`. A downloaded widget can exec any user-executable
file. Document, or require `symlink_metadata` to be a regular file.

### W11. INFORMATIONAL: `copy_dir` recurses without symlink-cycle or depth guards
`widgets.rs:296-309`. A directory-symlink cycle in a pool widget
overflows the stack. Requires root-authored input today; a two-line
guard (`entry.file_type()`, skip or copy symlinks as symlinks).

### W12. INFORMATIONAL: click children inherit the shell's stdout
`widgets.rs:465-471`. A `yes error` click line floods the journal for
15 s. `Stdio::null()` fixes it.

### W13. INFORMATIONAL: watcher churn holds up
The watcher drops Access events, coalesces bursts into one generation
bump, and cannot self-trigger (rediscover writes nothing watched). No
finding; recorded because it was asked.

## C. Subprocess edges, shell-wide

No ARCHITECTURAL findings here. Every spawn is argv-built; no dynamic
data is interpolated into a shell outside the freedesktop-standard
launcher path (documented below); no blocking output read exists on the
UI thread. Verified: `polkit.rs:1349`'s `sh -c` and kuma-files' three
`sh -c` sites are all inside `#[cfg(test)]` with fixed literals; the
production polkit path spawns a helper directly with the password on
stdin, never in argv.

### S1. BORING: `bluetoothctl trust` has no timeout, the only sibling without one (verified pattern)
`kuma-shell/src/sysmon.rs:1559`. Every arm of `BluetoothAct::execute`
wraps in `timeout` except this one, against the comment at 1525-1527.
A wedged bluetoothd leaves `bt_busy` set forever and the Bluetooth
panel pinned busy until restart. Fix: `timeout 10`.

### S2. BORING: the mint CLI spawn has no timeout
`kuma-shell/src/nostr_panel.rs:1131`. `kuma-nostr bunker --json` with
no `timeout` wrapper, unlike every sibling (15-30 s). A hung vault
daemon leaves the panel minting forever. Fix: `timeout 15`.

### S3. BORING: curl responses are buffered whole in RAM
`kuma-shell/src/weather.rs:179-191` (geolocate) and `:249-252`
(forecast). `--max-time 15` bounds the connection, not the body; a
500 MB response lands in `.output()`'s Vec before parse. Fix:
`--max-filesize 1000000` on both.

### S4. BORING: the nostr avatar download is unbounded and protocol-open
`kuma-shell/src/nostr_panel.rs:313-316`. The URL comes from a remote
kind-0 profile event; curl will follow it to any scheme. Fix:
`--max-filesize 5000000` and `--proto https`.

### S5. BORING: `date` runs with no timeout inside a OnceLock initializer
`kuma-shell/src/night_light.rs:191-194`. If `date` ever blocks, the
night-light thread wedges on first tick and the OnceLock never
initializes, killing the feature until restart. Fix: `timeout 5`, or
read the offset without a child.

### S6. BORING: `gio mount -u` has no timeout
`kuma-files/src/browser.rs:3550-3553`. Unmounting a dead NFS/SMB mount
can hang gio; the status line never updates. Fix: `timeout 30`. (The
`gio mount` connect pump at 3621 has the designed Cancel-kill path;
document only.)

### S7. INFORMATIONAL: fire-and-forget spawns without timeouts
`settings_view.rs:98` (niri quit, systemctl reboot/poweroff),
`idle.rs:87`, `bar.rs:1564` (pkill), `updates.rs:179` (reboot),
`updates.rs:349` (fixed `sh -c` literal, nothing interpolated),
browser.rs spawn-only sites (terminal, xdg-open, open-with, gvfsd).
All background, all spawn-only or trivial output, degradation is a
leaked task. A uniform `timeout` is mechanical if the rule is to be
absolute.

### S8. INFORMATIONAL: launcher `sh -c` is freedesktop-standard
`launcher.rs:453-458`: the Exec value rides as a single argv element
after field-code stripping; no earlier injection. One fidelity nit: the
field-code strip is not quote-aware, so `Exec=foo "%U bar"` can break a
launch, never forge one. Document.

### S9. INFORMATIONAL: updates check output is unbounded but image-controlled
`updates.rs:200-214`. `timeout 600` is present; the peer is the `kuma`
CLI from the image. Cap it for uniformity if desired. The apply path
(`:228-263`) is exemplary: stdout drained not accumulated, stderr
capped at 160 chars per line.

## D. kuma-term

### T1. ARCHITECTURAL: vte 0.15's OSC buffer is unbounded under `std` (verified in dependency source)
`kuma-term/src/term.rs:455` feeds raw pty bytes to
`alacritty_terminal::vte::ansi::Processor`. In vte 0.15
(registry source, lib.rs:62-64, :547-551) `osc_raw` is a `Vec<u8>`
under the default `std` feature and the `is_full()` cap is compiled
out; `MAX_OSC_RAW` (1024) applies only to the no-std ArrayVec. An
unterminated OSC from remote output (`ssh` to a hostile host, `curl`
of a crafted file) grows the buffer without bound while the pump holds
the term lock, stalling every tab; the payload also flows whole into
`Title` events and clipboard base64. Fix shape: a `patches/` overlay on
vte (the repo already has the `[patch.crates-io]` convention for gpui)
or an upstream release pin. Dependency-graph change: parked for 0.6.0.

### T2. BORING: ctrl+click opens `file://` and `mailto:` URLs; no scheme check at the opener (verified)
`kuma-term/src/view.rs:1829` (the scheme list), `:1180-1189`
(`open_url` runs `xdg-open` argv-style, no shell). Remote output that
prints `file:///home/user/...` or a `.desktop` path is one accidental
ctrl+click from execution or disclosure. The argv path closes shell
injection; the gap is purely the scheme set. Fix: allowlist http/https
in `open_url`; file/mailto spans can still underline as text.

### T3. BORING: an OSC 52 flood keeps the keyboard captured indefinitely
`view.rs:549-570` (`:830` the prompt sits above tab chords in
`on_key`). Each new store re-arms the 10 s timer, so a tight loop
swallows every keystroke indefinitely; the user's only escape is
winning a race. Fix: while a prompt is pending, ignore further
ClipboardStore events so the card always self-denies within 10 s.

### T4. BORING: defensive index fixes on the parser-fed paths
`palette.rs:120` (`overrides[index]` panics for index >= 269; today
unreachable only because vte's parse paths clamp, verified) and
`term.rs:886` (`cells[spot.col]` trusting a display_iter density
invariant). Fix: `.get()`/`.get_mut()` with graceful skip. Pure defense
in depth.

### T5. INFORMATIONAL: growth bounds are otherwise solid
Scrollback clamped 0..1_000_000, shaped-row cache 4096 with
half-eviction, search matches 10 000, pty input a bounded sync channel
with drop-on-full, 16 params per sequence, resize math all
saturating/min-bounded, and the old mid-link url_spans slice panic is
fixed and regression-tested. No findings.

### T6. Fuzz target (planned, 0.5.1)
Entry point: `Processor::advance` over raw `&[u8]` into a 80x24 Term,
then `renderable_content()` → `fill_rows` → `build_row` → `url_spans`
→ `hash_row`/search spans, plus `palette::resolve_index` for indices
0..300. Mirrors the existing test harness at term.rs:1037-1049. Note:
no in-crate fuzz target can observe T1's buffer growth; that needs the
vte-side harness or an RSS assertion in the displayless smoke.

## E. State files and Koguma file operations

### K1. BORING: config and state saves are not atomic
`kuma-shell/src/settings.rs:1116` (config.toml),
`kuma-files/src/browser.rs:4278` (state file),
`kuma-shell/src/updates.rs:416` (updates state) all `fs::write` in
place. A crash or power loss mid-write tears the file; the next load
fails the whole parse and silently falls back to defaults, losing every
setting, and the next save overwrites the torn file. displays.rs:755-761
already uses tmp+rename (`write_store`); reuse that pattern in all
three.

### K2. BORING: Koguma state file paths are written raw into a line-based format
`browser.rs:4271, 4275`. A directory with an embedded newline splits
the record and the tab/place is silently lost every restart. Fix: skip
or escape paths containing newlines on write.

### K3. BORING: replace-move destroys the destination before the rename is guaranteed
`browser.rs:429-437`. `remove_dir_all`/`remove_file` then `rename`:
across filesystems the remove succeeds and the rename fails with
EXDEV, reporting "move failed" with the destination already destroyed.
Fix: rename the target aside first and restore on failure, or refuse
cross-device replace with an honest error.

### K4. BORING: undo is lost when the op queue fails mid-run
`browser.rs:3085-3096`. On the first error the accumulated undo
inverses are dropped, so a 5-file paste that fails on file 4 leaves
files 1-3 copied but not undoable. Fix: on failure, still extend
`this.undo` with what succeeded.

### K5. BORING: tar compress passes basenames without `--` (verified)
`browser.rs:629-636`. A file named `--checkpoint-action=...` (a
downloaded archive can create it) is parsed by tar as an option.
Argument injection from a filename. Fix: `.arg("--")` before the
basenames, and before file-roller's `--add-to` lists for symmetry.

### K6. BORING: a FIFO copy source wedges the op queue; copy recursion has no cycle guard
`browser.rs:417, 665-663`. `fs::copy` on a FIFO blocks forever with no
writer, `busy` never clears, and every later op is refused until
restart; `copy_dir_all` recurses with no depth cap (a bind-mount cycle
is a stack overflow; symlinks are safe, readdir reports them as
symlinks). Fix: skip or error on non-regular files; bound the
recursion.

### K7. BORING: a failed compress leaves the partial archive, and retries then fail on "already exists"
`browser.rs:596-598, 643-651`. Fix: on tool failure, best-effort
`remove_file(archive)` before returning the error.

### K8. BORING: the activation socket's `/tmp` fallback is cross-user reachable (verified)
`kuma-platform/src/activation.rs:14-22, 39-47`. With
`XDG_RUNTIME_DIR` unset the socket lands in `/tmp` with umask-default
perms and no peer check: another local user can pre-bind
`/tmp/kuma-files-1000.sock` (every launch hands over and exits 0,
silent DoS) or connect and feed 4096 bytes treated as a path. kumaOS
never hits this (runtime dir is 0700); a laptop without logind does.
Fix: `umask(0o077)` before bind in the fallback, and/or a
`SO_PEERCRED` same-uid check before accepting an activation.

### K9. INFORMATIONAL: local.kdl escaping gap and no round-trip parse
`displays.rs:687-689, 758-761`: `escape()` handles `\` and `"` but not
newlines, and the tmp file is renamed without re-parsing. An output
name with a raw newline (exotic EDID) wedges future pins until a hand
fix. Fix: round-trip `parse_store` on the tmp content before rename.

### K10. INFORMATIONAL: hand-edit clobber window in local.kdl
`displays.rs:425-429`: read-modify-write, last writer wins. Unparseable
hand-edits are safe (the store refuses to rewrite what it cannot
parse, tested). Document the window.

### K11. INFORMATIONAL: zip-slip rests on tool defaults
`browser.rs:482-588`: extraction delegates to tar/unzip/gzip/
file-roller under `timeout 600`; GNU tar and Info-ZIP both refuse or
strip traversal members by default. The in-crate epub reader never
extracts to disk and caps decompressed bytes. Document the tool-version
assumption (VENDORED.md) or add a post-extract containment check.

### K12. INFORMATIONAL: typed names may traverse
`browser.rs:2231, 3883`: `../foo` typed into new-folder or the compress
name escapes the visible tree, by the user's own hand. Reject names
with `/` or `..` in the field validator for honesty with the
"operations stay in the visible tree" invariant.

### K13. INFORMATIONAL: kuma-monitor reads nothing user-writable
No config, no state, only root-owned `/proc`/`/sys` parsed defensively
and the palette. Cleanest surface in the family; nothing to fix.

## 0.6.0 parking list

- **A1: lock screen PAM timeout.** Race the in-process conversation
  against a timer or adopt the polkit helper-process shape. Touches the
  authenticate seam; needs a design pass and a decision about leaking
  the abandoned thread's PAM handle.
- **T1: vte OSC buffer cap.** A `patches/` overlay on vte 0.15 or an
  upstream pin. Dependency-graph change; the mechanism exists, the
  decision (and the upstream conversation) belongs to the owner.
- **W4 (second half): move widget discovery off the main thread.** The
  FIFO/size guards make the current threading safe; the structural fix
  is a 0.6.0 candidate if discovery cost grows.

## What the audit did not find

Recorded because absence is evidence: no dynamic interpolation into a
shell outside the freedesktop launcher path; no blocking subprocess
read on any UI or render thread; no panic reachable from pty bytes in
kuma-term's own code; no password material in any log line across
lock, polkit, or greeter; the PAM chain's fall-through rejects rather
than accepts; the updates apply path drains stdout and caps stderr
lines; kuma-monitor reads nothing user-writable. The codebase's
instincts are sound; nearly everything found is a missing cap or
timeout at an edge that already has the right shape.
