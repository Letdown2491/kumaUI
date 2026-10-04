# Greeter deployment

What kumaOS needs from kumaUI's greeter, and what the greeter needs
from kumaOS. The rehearsal that validated this lives at the bottom.

## Pieces

- `/usr/bin/kuma-greeter`: built from `kuma-shell/src/bin/kuma-greeter.rs`
  (same crate as the Shell, same gpui). Bake it into the image next to
  `/usr/bin/kuma-shell`; the build produces both binaries from one
  `cargo build --release`. `/usr/bin` files get the `bin_t` label, so
  SELinux needs nothing special for it.
- `/usr/share/kumaos/greeter-niri.kdl`: the greeter compositor's niri
  config (`greeter-niri.kdl` in this repo). Minimal on purpose: one
  layer-shell window, no keybinds, and `hotkey-overlay` with
  `skip-at-startup` so the Important Hotkeys popup never covers the
  login fields.
- `/etc/greetd/config.toml`:

  ```toml
  [terminal]
  vt = 1

  [default_session]
  # kuma-greeter: keep the tuigreet line as a commented fallback,
  # reverting is a comment swap from a TTY
  command = "niri -c /usr/share/kumaos/greeter-niri.kdl -- /usr/bin/kuma-greeter"
  user = "greetd"
  # command = "tuigreet --time --remember --greeting 'Welcome to kumaOS' --cmd niri-session"
  # user = "greetd"
  ```

## How it works

greetd launches niri on VT1 as the `greetd` user with `GREETD_SOCK`
in its environment; niri launches kuma-greeter as its startup
command, and the socket flows down to it. The greeter renders one
layer-shell surface (the lock screen's shape: overlay layer, all
four anchors, exclusive keyboard), shows the default wallpaper and
theme from the settings fallbacks, and speaks greetd's IPC:
create_session, answers for every auth message, start_session. On
success the greeter exits and greetd starts the session.

## SELinux notes

The greeter runs in the `xdm_t` context (inherited from greetd).
Two denials surfaced during rehearsal on a developer's home
directory (fonts labeled `user_home_t`, fontconfig cache files):
neither applies to the shipped configuration, because the `greetd`
user's home is not a user home and the wallpaper and fonts it
reads are image paths (`usr_t`). Still, the pre-ship gate is one
rehearsal as the actual `greetd` user (below); if fontconfig's
cache writes are denied, give the greeter its own cache directory:

```
# /usr/lib/tmpfiles.d/kuma-greeter.conf
d /var/cache/kuma-greeter 0700 greetd greetd -
```

plus `XDG_CACHE_HOME=/var/cache/kuma-greeter` in the greeter's
environment (a wrapper script or the greetd command's `sh -c`).

## Resilience

- greetd.service restart policy: if the greeter dies, greetd exits
  with it. A drop-in with `Restart=on-failure` and
  `RestartSec=2` keeps a broken greeter from stranding a boot;
  TTY logins (getty) remain the always-there escape hatch.
- The greeter never panics on a failed window: it logs and exits
  nonzero, so the journal stays readable.

## The rehearsal (validated 2026-10-03)

From inside a running session, a second greetd on VT2, touching
nothing in `/etc/greetd/config.toml`:

```
sudo systemd-run greetd --config /path/to/greetd-test.toml
```

with `greetd-test.toml` setting `vt = 2` and the greeter command.
Log in for real: greetd starts a second niri session next to the
running one. Escape hatches: Ctrl+Alt+F1 back, then
`sudo pkill -f greetd-test.toml`.

Issues found and fixed by the rehearsal, all in kumaUI (shipped):

- a plain toplevel dies on niri with a `wp_viewport` protocol
  error, so the greeter window rides the lock screen's layer-shell
  shape
- a scale event landing before the layer configure sent
  `set_destination(0, 0)`, which viewporter forbids; vendored gpui
  now skips zero destinations (the panel is 162 DPI and niri's
  default scale there is 2, which exposed the latent bug)
- the hotkey overlay covered the fields; `skip-at-startup` in the
  greeter compositor config
- the greeter shows the settings wallpaper and theme, dimmed,
  like the lock screen
