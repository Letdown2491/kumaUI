# Session deployment

What kumaOS needs from the session side of kumaUI, and what the
image wires in. This is the counterpart of `greeter-deploy.md`:
that doc covers the login screen, this one covers what the user
lands in after it.

## The blank-second problem

After login the handoff is serial: the greeter exits (its niri
dies), greetd starts `niri-session`, which imports the environment
into the user manager, starts `niri.service`, waits for it, then
starts `graphical-session.target`, and only then does
`kuma-shell.service` (ordered `After=graphical-session.target`)
come up. Every step is a systemctl round trip. During all of it
the VT shows whatever niri paints, which is black.

kumaUI cannot remove the chain, but it can make it invisible.

## What kumaUI ships

- `/usr/share/kumaos/session-niri.kdl` (from `session-niri.kdl` in
  this repo): the stock session config plus `layout.background-color`
  set to the shell's wallpaper base (`#11111B`) and
  `hotkey-overlay.skip-at-startup`. The gap between niri coming up
  and the shell painting the wallpaper reads as a solid fade
  instead of a black flash.
- `greeter-niri.kdl` carries the same background color, so the
  greeter-to-session handoff never changes shade either.

## What the image wires in

A drop-in for `niri.service` that points the session compositor at
the shipped config:

```
# /etc/systemd/user/niri.service.d/kumaos.conf
[Service]
ExecStart=
ExecStart=/usr/bin/niri --session -c /usr/share/kumaos/session-niri.kdl
```

Everything else (`niri-session`, the narrowed `import-environment`
sed, `kuma-shell.service` ordering) stays as is. If a later
measurement pass shows the shell is gated behind the target for too
long, the next step is re-anchoring `kuma-shell.service` to
`niri.service` (`After=niri.service` instead of
`After=graphical-session.target`), not bypassing systemd.

## Measuring the handoff

To find where the seconds go, log in and read the timestamps:

```
journalctl -b -o short-precise \
  -u greetd.service -u user@1000.service \
  -u niri.service -u graphical-session.target \
  -u kuma-shell.service
```

The interesting spans: greetd `start_session` to `niri.service`
active (the wrapper), niri active to `kuma-shell.service` started
(the target chain), and kuma-shell start to its first surface
(gpui init). If the first span dominates on the first login after
boot, the user manager is starting cold and pre-warming it
(linger or a system-side want) buys more than any ordering change.

Measured on motherbox (2026-10-04, warm re-login): the shell-side
path — exec to surfaces up, read from the shell's own `boot:` phase
logs — took 16 ms. The shell is not the blank-second cost; whatever
the handoff chain spends, it spends before the shell execs, so
re-anchoring and shell-side deferral are near-worthless warm. A
cold-boot (first login) capture is the number still missing.
