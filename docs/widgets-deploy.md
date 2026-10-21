# Widget deploy: what kumaOS does with custom widgets

What the image needs from kumaUI's user-made widgets, and what the
widgets need from the image.

## The system pool

The image ships stock widgets to `/usr/share/kuma-shell/widgets/`, one
directory per widget, the same shape a user installs:

```
/usr/share/kuma-shell/widgets/
  updates/
    widget.toml
    updates.sh
    updates.svg
```

The source of truth for the shipped widget is `widgets/updates/` in the
kumaUI repo; the compose copies it in whole (the directory is the
activation unit, and the manifest names its siblings by relative path,
so a partial copy is a broken widget).

## The bootc read

`bootc status --json` wants root. The update chip's script tries
unprivileged first, then `sudo -n`; with neither grant it prints
nothing and the widget hides, so a machine without the grant is quiet,
not broken. On kumaOS, grant the read, not the write: a polkit rule or
a narrow sudoers stanza for `bootc status` is enough, and the chip
comes alive. The write path (`bootc upgrade`) stays with `kuma
upgrade` and its own authentication.

## What the user sees

- Settings, Widgets page: a "User-made widgets" section lists installed
  customs (placement toggle, mode pills), the pool's stock widgets with
  an Install button, and inert placements (a config entry naming a
  widget that is not installed) as dimmed remove-only rows.
- The Ordering page places customs like built-ins: drag the chip.
- A user can also install by hand: copy any widget directory into
  `~/.config/kuma-shell/widgets/`; the watcher makes it live within a
  second, no restart.

## Versioning

The pool copy is owned by the image and updated by the image; an
installed copy in the user's config dir is owned by the user and never
reached by an image update (kumaOS's /usr is immutable, the user's
config is not). A user widget overrides a system one by stem: to take
a newer stock widget, delete the installed copy and install again.
