# Writing bar widgets

A widget is a directory. The directory holds a `widget.toml` manifest
plus whatever scripts and assets the manifest names, so the whole
widget travels as one unit: copying the directory into place is all
installing means.

## A minimal widget

The degenerate widget is one file:

```
~/.config/kuma-shell/widgets/hello/
  widget.toml
```

```toml
# widget.toml
command = "echo hello"
```

That is a complete widget: `command` is a shell line run on a timer,
and the stdout becomes the bar's label. Create the directory and it is
available; the shell's watcher notices within a second, no restart.

## The manifest

```toml
# widget.toml
label = "Weather"
icon = "sun.svg"
interval = 60
source = "weather.sh"
click = "kuma-shell msg settings"
```

- `label`: the name the settings page shows and the tooltip falls back
  to. Defaults to the directory's name.
- `icon`: a themed icon name (resolved through the icon theme index)
  or a file in the widget directory. A name ending in a known image
  extension (`.svg`, `.png`, `.jpg`) is a file path; anything else is a
  themed name. A file-named icon the directory does not hold falls
  back to themed resolution. Optional: without it the widget renders
  text only. An SVG paints as a glyph in the widget's color, exactly
  like the built-in icons; a raster (PNG, JPG) paints in its own
  colors, right for artwork, wrong for chrome.
- `interval`: seconds between runs. Defaults to 10, clamped from below
  at 1.
- `source`: a file in the widget directory, executed directly on the
  interval (its shebang applies; it is never reinterpreted by a shell,
  so mark it executable).
- `command`: a shell line run as-is on the interval. Give exactly one
  of `source` and `command`.
- `click`: a shell line run when the widget is clicked. Fire and
  forget, bounded at 15 seconds, never blocking the bar. Optional.

The directory's name is the widget's identity. It may not name a
built-in widget (`clock` is taken), and a manifest `id` key must match
the directory name or the widget is rejected. All file paths the
manifest names must stay inside the directory.

## The stdout contract

Plain text is a label:

```
$ ./weather.sh
72 F, partly sunny
```

The JSON upgrade path is Waybar's custom-module convention, so scripts
written to feed a Waybar custom module render here unchanged:

```json
{"text": "72 F", "tooltip": "Partly sunny, feels like 70", "class": "normal"}
```

- `text` is the label, `tooltip` is the hover text, `class` is
  accepted and ignored (Waybar scripts set it for CSS hooks this bar
  does not have).
- Unknown JSON fields are ignored.
- Empty stdout (or an empty `text`) hides the widget entirely: a
  widget with nothing to say says nothing.
- A stdout that starts with `{` but does not parse falls back to plain
  text, so a script caught mid-rewrite cannot blank the bar.

Failure is honest: a run that exits nonzero, times out, or cannot
spawn leaves the last read on the bar, dimmed, with the tooltip naming
the failure. A run's budget follows its interval (floored at ten
seconds, capped at a minute); a slow run skips its own next tick
rather than stacking.

## Installing and placing

Two doors, one truth:

- The settings page's Widgets tab: pool widgets (shipped by the image
  to `/usr/share/kuma-shell/widgets/`) appear under Available to
  install; the Install button copies the directory into
  `~/.config/kuma-shell/widgets/`.
- Or copy any widget directory into `~/.config/kuma-shell/widgets/`
  by hand, with any file manager.

Installing makes a widget *available*. Placing it on the bar is the
same toggle every widget has: the Widgets page's switch adds it to the
bar (appending to the right section), the Ordering tab drags it
between sections. Customs list beside the built-ins, and a custom's
mode pills are the full dial (icon / icon+text / text); built-ins
carry their own dials from the registry (the clock's is date+time /
time, a widget with one option shows no pills). The config follows the
filesystem: deleting a widget's directory removes its placement (and
its line in `config.toml`) live, and reinstalling starts from the
default position again.

## An example with JSON and an icon

```
~/.config/kuma-shell/widgets/battery-lid/
  widget.toml
  lid.sh
```

```toml
# widget.toml
label = "Lid"
icon = "battery.svg"
interval = 30
source = "lid.sh"
click = "kuma-shell msg settings"
```

```sh
#!/bin/sh
# lid.sh: the Waybar contract, one object, no trailing noise
case "$(cat /proc/acpi/button/lid/LID0/state 2>/dev/null)" in
*open*) echo '{"text": "lid open", "tooltip": "The lid is open.", "class": "normal"}' ;;
*closed*) echo '{"text": "lid closed", "tooltip": "The lid is closed.", "class": "urgent"}' ;;
esac
```

Remember the exec bit (`chmod +x lid.sh`): a source file is executed
directly, and a non-executable one is an honest error, not a surprise
shell interpretation.

## Image-side notes

What the image does with the pool: [widgets-deploy.md](widgets-deploy.md).
The Kuma Updates chip is built in, not a pool widget; what it needs
from kumaOS is in the same file.
