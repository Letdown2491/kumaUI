# kuma-shell domain glossary

One line per term. These names are the seams in the code; use them in code, docs, and ADRs.

- **Shell**: the whole running kuma-shell program (wallpaper, bar, and every panel). The binary `kuma-shell`; the GUI process.
- **Bar**: the layer-shell strip at the top of the screen. Its geometry (height, offset, width fraction, corner rounding) is applied live by the bar view; the layer surface itself is always a stretched full-width transparent window.
- **Widget**: one piece of information rendered on the Bar (workspaces, window title, apps, cpu, memory, temperature, disk, volume, microphone, brightness, power profile, media, battery, clock, bluetooth, internet, notifications, system tray, nostr signer). Declared in the widget registry (`settings.rs::WIDGETS`); rendered in `bar.rs`. Data comes from the **session state** or the **system monitors**.
- **Session state**: the mirror of the running compositor's state (workspaces, windows, focus), kept current by a compositor adapter's event stream. Lives in `session.rs::SessionState`, fed by adapters into a neutral event enum; `niri.rs` and `sway.rs` are the adapters (picked at startup by which IPC socket exists), and the widgets, dock, and focus commands (`session.rs::focus_workspace`, `focus_window`) never name the compositor (ADR-0012).
- **System monitors**: polled snapshots of machine state (battery, volume, mic, brightness, cpu, bluetooth, network, media, power profile). Lives in `sysmon.rs::SysMon`; a fast pass every 500ms for the audio/brightness trio, a full pass every 2s, subprocess/procfs reads behind an adapter seam. Volume, mute, mic, and brightness changes ride one serialized request seam on the monitors; callers never write a snapshot directly. Outside changes pass a two-read confirm gate before applying, and mute toggles carry a named target plus a bounce guard (the mute key double-fires on some laptops); all of it is ADR-0009.
- **Panel**: a drawer surface that hangs flush under the Bar, centered, with a concave cove silhouette (`panel.rs::drawer_silhouette`). Opens with a **scrim** beneath it: a fullscreen click-catcher that dismisses the panel on outside clicks. Keyboard mode per panel: `Exclusive` (launcher) or `OnDemand` (settings).
- **Panel host**: the gpui Global that owns the open panel's lifecycle and placement: the Bar reports its live geometry into it, and every panel receives its `PanelGeometry` from the host; views never hardcode size. `toggle_panel(kind, cx)` / `close_panels(cx)`. Free functions, because the host is borrowed from `cx` in short blocks.
- **Panel chrome**: the host-owned wrapper every Panel wears: drawer silhouette, input region, cove padding, Esc-to-dismiss. Panel views render content only; the chrome comes from the Panel host.
- **Panel kit**: the shared vocabulary for panels with tabs (`panel_kit.rs`): `tabbed_pane` (its header sits over the right pane only), `rail_tab` (icon-only rail entry with a hover tooltip), `setting_row` (one row on a tabbed page). A panel with tabs wears the kit; the rail never carries text labels.
- **Arm-then-confirm row**: the kit piece (`panel_kit.rs::confirm_actions`) for acts that end something (logout, reboot, poweroff): the clicked action swaps its label for the confirm phrase and fills the alarm, the row's other actions dim and ignore clicks, and the arm expires after 5s, on Esc, or when the panel closes. One armed act at a time; the armed state lives in the row's own view, so panel close disarms for free.
- **OSD**: the on-screen-display toast card that rises under the Bar for volume, mute, microphone, brightness, do-not-disturb, and power profile changes, then expires (`osd.rs::Osd`). One overlay window, opened on the first card; content is pushed into its view, never read live from the monitors (ADR-0010).
- **Persistent surfaces**: the surfaces the Shell cannot work without (wallpaper, Bar, lock screens, dock), watched by `surfaces.rs::SurfaceHost`: a 2s watch probes their handles and recreates the missing ones while displays exist (`cx.displays()` shows output hotplug without holding a window). The Shell never quits because windows closed: it idles displayless holding its DBus names and deliberately takes no part in the suspend path (it holds no logind sleep inhibitor; the sleep guard owns that flow, and lock-before-suspend is the compositor's), and exits only when the compositor's connection breaks, which is session end. This is what a dock unplug or the CI smoke's output-less qemu must survive.
- **Lock screen**: opaque, wallpaper-backed surfaces on every display with exclusive keyboard and one shared password field, opened by logind's session `Lock` signal (`lock.rs::LockState`). The password buffer captures the typed character, never the key name (ADR-0007). While locked, the surfaces watch recreates the surfaces when displays return.
- **PAM service chain**: the ordered services an unlock attempt tries (`kuma-lock` → `swaylock` → `vlock`); the first *installed* service wins. Missing `/etc/pam.d/<service>` files are skipped, because Linux-PAM defers reading a service's stack and answers from the `pam_deny` `other` policy when it's absent, rejecting every password exactly like a wrong one. Anything PAM wants to ask interactively is refused (password-only).
- **Settings**: the persisted configuration (`~/.config/kuma-shell/config.toml`), loaded into an observed entity. Mutators save and notify inside the seam; callers never call notify.
- **Idle watcher**: the Wayland idle clock owner (`idle.rs`), speaking ext-idle-notify-v1 on its own connection. Its three clauses (lock timeout, screen-off timeout, lock before suspend) surface in the settings panel's Idle page, in execution order; each clock wears an on/off toggle and a minute field, and edits commit on Enter or a click away. User edits are runtime user state in the settings store; compiled defaults are what a fresh user gets, nothing outside the shell writes them. The two clocks are independent, documented not enforced: no clamping, and the UI's dim note is the only opinion about screens blanking before the lock engages. A settings change re-mints the watcher through the generation counter; no msg verbs for idle.
- **Usage counts**: launch counts per app, seeded from noctalia's `usage_counts.json`, stored at `~/.local/state/kuma-shell/usage.json`; drives the launcher's most-used-first ordering.
- **Launcher**: the app-list panel (search-as-you-type, fuzzy-scored, arrow-navigated, Enter launches).
- **MSG CLI**: `kuma-shell msg <verb>`, thin client subcommands (volume up/down/mute, mic-mute, brightness up/down, media, launcher, settings, notifications, nostr) that ride the IPC socket (`$XDG_RUNTIME_DIR/kuma-shell.sock`) into the running Shell, falling back to standalone `wpctl`/`brightnessctl` when no Shell listens. This is what niri keybinds spawn.
- **Memory return**: the two seams that keep the Shell's footprint at its working set over weeks: notification icons' atlas tiles drop when their entries leave the history (`notifications.rs::drop_icon`; the `img` element never drops the tile it paints), and a 30 s background `malloc_trim(0)` madvises glibc's freed arena pages back to the OS (`main.rs`, ADR-0011). The memory probe (`scripts/memory-probe.sh`) runs the Shell under scripted load in the test container and reads the per-phase deltas.
- **Vendored gpui**: the gpui framework, vendored from zed main at a pinned commit (`VENDORED.md`); the crates.io release predates layer-shell.

## Ethos

Four rules the code already lives by. They exist so the next decision
has somewhere to start; when practice drifts from this section, fix
the practice or amend the section, never let it lie.

- **Ask, don't assume** (the HATEOAS instinct): state is discovered
  from the environment at the moment it matters, never cached across
  the boundary that owns it. Adapters are detected by which IPC socket
  exists, commands re-probe at call time, the mirror refetches when
  the compositor's flags disagree with it, and surface liveness is
  probed from the handle, not tracked in a ledger. A source that
  cannot answer yields an empty render, not a stale guess (ADR-0012,
  ADR-0003).
- **Hold nothing** (non-attachment): an allocation that outlives its
  reason is a bug. An object leaving a collection takes its pixels
  with it, and the footprint tracks the working set, not the lifetime
  high-water (ADR-0011). Measurements, not intentions, decide:
  mimalloc was tried, did not win, and left no trace.
- **Yield** (wu wei): the Shell contends for nothing. No sleep
  inhibitor, no session lock, no fight against session end. The mirror
  observes and never writes back; commanding the compositor is a
  separate, user-initiated act (a click, a keybind). The watch acts
  only when conditions change and logs nothing when all is well.
- **Own your wires** (cypherpunk): keys never enter this process (the
  nostr daemon is the policy, the CLI the transport), the trust
  boundary is the user account and nothing inside it needs to prove
  itself, every protocol is hand-rolled enough to read (ADR-0003), and
  the msg CLI is the user's road in and out.
