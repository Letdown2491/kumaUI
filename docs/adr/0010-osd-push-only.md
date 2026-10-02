# ADR-0010: The OSD is a push-only toast; its view reads no entity

Status: accepted (2026-10-02)

## Context

Volume, mute, microphone, brightness, do-not-disturb, and power profile
changes are made blind (a hardware key, a click, an outside tool), so a
centered card under the bar confirms them. The window opens on the first
card, and gpui renders a window's first frame inside `open_window`, which
runs while the OSD entity is still mid-update. The natural view design,
reading state in render, panics: "cannot read kuma_shell::osd::Osd while it
is already being updated".

## Decision

`Osd` observes SysMon and Settings, diffs a snapshot, and pushes finished
card content into the window's view (`view.content = Some(content)`); the
view reads no entity, so it can render safely inside the window's opening
update. Repeats update the same window and bump a generation counter, and
the 1.5s expiry timer dismisses only the generation it armed. First
sightings record without toasting, so the startup fill and the dnd and
profile first reads never raise cards.

## Consequences

- One overlay window lives for the session after its first card; expiry
  closes it via `panel::defer_close`, the required close path.
- A new card kind is one content function plus a diff arm in
  `content_for_change`; the dnd and profile arms require a previous value
  to diff against, so their first sighting stays silent.
- The card is plain data (icon, label, value, percent, urgent), so the
  diff logic is unit-tested without a window.
