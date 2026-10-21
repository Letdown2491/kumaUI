# Widget deploy: what kumaOS does with custom widgets

What the image may ship into the widget pool, and what the widgets
need from the image (today: nothing).

## The system pool

The image may ship stock script widgets to
`/usr/share/kuma-shell/widgets/`, one directory per widget, the same
shape a user installs. The directory is the activation unit and the
manifest names its siblings by relative path, so a partial copy is a
broken widget: compose copies each widget in whole. The pool is empty
in the current image; the Kuma Updates chip lives in kuma-shell
itself now, so no pool entry and no grants are needed for it.

## The update chip

The bar's Kuma Updates widget is built in, fed by
`kuma update --check --json` (at boot, then every four hours). The
check is unprivileged: one round-trip to the registry, or to Fedora's
repos for a composed base's kernel, no pull, no build, no root. The
chip shows the moved-package count and tints when security updates are
among them; clicking it opens the panel listing each package's
versions, with an Update now button that runs
`pkexec kuma update --yes --json`: polkit carries the prompt to the
shell's lock dialog, so staging costs the user's password and nothing
else. For that to work end to end, kumaOS must ship whatever polkit
wants to allow `kuma update` as root (pkexec's admin-auth rule
suffices; a dedicated `com.kuma.update` action with a branded prompt
is nicer), and `kuma update` running as root must resolve the
machine's baked declaration the way the unprivileged check does
(`source: machine`).

## What the user sees

- Settings, Widgets tab: customs list beside the built-ins, one
  alphabetical row each with the same placement toggle and mode pills;
  the pool's stock widgets appear under Available to install. The
  config follows the filesystem: deleting a widget's directory prunes
  its placement live. Authoring widgets is [widgets.md](widgets.md).
- The Ordering tab places customs like built-ins: drag the chip.
- A user can also install by hand: copy any widget directory into
  `~/.config/kuma-shell/widgets/`; the watcher makes it live within a
  second, no restart.

## Versioning

The pool copy is owned by the image and updated by the image; an
installed copy in the user's config dir is owned by the user and never
reached by an image update (kumaOS's /usr is immutable, the user's
config is not). A user widget overrides a system one by stem: to take
a newer stock widget, delete the installed copy and install again.
