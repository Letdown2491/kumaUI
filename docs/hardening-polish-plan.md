# The hardening and polish release

A plan for the next release cycle: a hardening and security review across
the whole family, then a polish sweep over every widget and every binary.
No new features unless one makes a fix natural. Decisions below were made
with the owner on 2026-10-21, the day v0.5.0 shipped.

## Decisions

- **Patch-style release (0.5.1).** Hardening fixes land as Changed and
  Fixed bullets. The release is the next tag, not a themed event.
- **Two-tier risk.** Boring fixes (caps, timeouts, permissions, panic
  removal, clamps, graceful degradation) land in 0.5.1. Any finding that
  wants to move a seam, change a signature, or reshape a module is parked
  in the 0.6.0 list at the bottom of this plan instead. The owner is away
  for up to two weeks; architectural calls wait for them.
- **Fuzz target for the terminal parser.** A test-only fuzz target against
  kuma-term's vt parser, running in the build container. No shipped-code
  dependency.
- **Greeter in full scope.** It is the pre-auth surface: greetd framing,
  PAM handling, and the hosting-niri deployment path all get audited like
  the four shipping binaries.

## Ground rules for the whole cycle

- Every fix is a small commit with its changelog bullet in the same
  commit, grouped under Changed or Fixed so the bump stays patch.
- Tests run through `./scripts/build.sh`; the host has no toolchain.
- Anything that belongs in kumaOS becomes a handoff prompt
  (`docs/handoffs/`), never a direct change.
- Documentation keeps the no-em-dash rule.
- The audit phase is read-only. Findings are written before any fix that
  needs judgment; mechanical fixes may land as they are found.

## Phase 1: the audit (read-only)

Deliverable: `docs/research/hardening-audit.md`, one section per trust
boundary, each finding rated boring / architectural / informational.

The boundaries and what to hunt at each:

- **msg IPC socket** (`kuma-shell/src/msg.rs`). Bind-time socket
  permissions (currently default, relying on `XDG_RUNTIME_DIR` being
  0700); the unconditional stale-socket removal before bind; request-line
  cap and read timeout (present, verify both on every verb); reply
  behavior on malformed input.
- **Custom widgets** (`kuma-shell/src/widgets.rs`). The newest surface.
  How the manifest's `command` and `click` lines are tokenized and run;
  stdout size caps (an uncapped read violates hold-nothing); Waybar JSON
  parse behavior on hostile output; the `unwrap()`s on `set_permissions`
  (a filesystem hiccup panics the shell); discovery of a user widget
  directory over the system pool; icon decoding of arbitrary SVG and
  raster files; the directory watcher's behavior on rapid churn.
- **Subprocess edges, shell-wide.** Every `Command::new` site: timeout
  presence, output size caps, and argument construction. Known sites to
  scrutinize: `weather.rs` and `nostr_panel.rs` (curl output and
  timeouts), `polkit.rs` (the `sh -c` at line 1349 and what it
  interpolates), `launcher.rs` (`sh -c` on desktop Exec is standard
  freedesktop behavior; document, do not change), `sysmon.rs`,
  `updates.rs`, `bar.rs` (pkill), `night_light.rs` (`date`),
  `settings_view.rs`.
- **Lock screen, PAM, and greeter.** Confirm no password material can
  reach the info-level unlock-failure logs; audit `greeter.rs`'s
  greetd framed-JSON reading the same way (frame caps, timeouts, malformed
  frames); the PAM chain's skip logic against a hostile or malformed
  `/etc/pam.d` layout; the greeter's settings-fallback reads (theme,
  wallpaper) for panic paths on missing files.
- **kuma-term.** The vt parser: build the fuzz target first, let it find
  things, then fix. URL opening on ctrl+click: scheme allowlist before
  anything reaches the opener. OSC 52 prompt is done; verify its deny
  path cannot wedge.
- **State and config parsing, all apps.** config.toml, the updates
  staged-result file (boot-id stamping, bounds on its fields), Koguma's
  state file, niri's `local.kdl` (documented as pausing on parse
  failure; verify), the palette file. Every parser degrades gracefully,
  never panics, on malformed input.
- **Koguma file operations.** The op queue's copy, move, trash, delete,
  and compress paths for traversal and self-destruction (a compress or
  move whose source or destination escapes the intended tree); the
  connect dialog's pump relay on hostile gio output.

## Phase 2: boring fixes into 0.5.1

Qualifies: adding a cap, timeout, or permission; replacing a panic with
graceful degradation; clamping a value; an error message that says what
happened. Does not qualify: changing who owns state, moving a seam,
changing a public signature, altering a documented ADR decision. The
fuzz target lands here, then its findings get triaged by the same rule.

## Phase 3: widget polish sweep

One checklist applied per built-in widget, fixes landing per widget:

- Empty and error states: a failing poll dims or hides, never renders a
  stale guess or a blank.
- Tooltip coverage: every readable value explains itself.
- Value clamping: negative temperatures, disk over 100 percent, absurd
  battery estimates.
- Mode dials behave as the registry declares; a stored mode outside the
  dial clamps at load.
- Crowding: the widget survives a narrow bar, and the bar's overflow
  behavior is consistent across widgets.
- Locale: the clock's date faces follow the system locale like the time.
- Icon fallback: a missing or unparseable icon degrades to the previous
  glyph, not a hole.

Then the custom widget machinery gets the same pass from the outside:
what a user sees when their script fails, times out, or prints garbage.

## Phase 4: binary polish sweep

Per binary (kuma-shell, kuma-files, kuma-term, kuma-monitor, greeter):

- `--version` behaves identically across the family.
- Log hygiene: default verbosity logs nothing when all is well; errors
  say what happened and what to do.
- Startup cost: no synchronous work on the startup path that can wait
  for a beat.
- Single-instance and activation behavior is uniform where it exists.
- Desktop entries, icons, and directory-handler bindings match what the
  build installs.

## Phase 5: verification and release

- `./scripts/build.sh test --release -p kuma-shell -p kuma-files -p
  kuma-term -p kuma-monitor -p kuma-platform`
- The displayless smoke, green.
- The memory probe, no new per-phase deltas versus v0.5.0.
- `./scripts/release.sh`, patch bump, `--tag` if the owner asks.

## The 0.6.0 parking list

Findings from the audit that are architectural land here, each with the
boundary it touches and a sentence on the shape of the fix. The owner
triages this list when back. The full entries live in the audit doc
(docs/research/hardening-audit.md, "0.6.0 parking list"); the short
form:

- **A1: lock screen PAM timeout.** The authenticate seam.
- **T1: vte OSC buffer cap.** A `patches/` overlay or an upstream pin.
- **W4 (second half): widget discovery off the main thread.** Only if
  discovery cost grows.
- **kuma-shell double-start guard.** The activation seam; semantics
  need an owner call.
- **Synchronous niri seed at files/monitor startup.** Only if a wedged
  niri socket ever shows up as a slow first frame.

## Addendum 2026-10-21: the review cycle folded in

The owner, back early, folded a full review round (correctness bugs,
performance, and a first DRY pass) into this release instead of parking
it for 0.6.0. Patch-style rules held: only wrong-behavior and hang
fixes, patch-sized perf wins, and consolidate-now dedup landed. The
findings and their commits are in the changelog under `## 0.5.1`
(Changed and Fixed); nothing architectural moved, so the 0.6.0 list
above is unchanged. New consolidation seams: `kuma_platform::process`
(bounded_output/timed_output), `kuma_platform::files` (write_atomic,
percent_decode), `kuma_platform::logging::init`.
