# ADR-0014: Display settings apply over niri's IPC and persist a field-level delta in local.kdl

Status: accepted (2026-10-06)

## Context

niri's runtime output changes are temporary: `niri msg output` says so in its
own help. A user who sets scale or mode loses it at reboot, and kumaOS owns no
persistence for outputs either. The gap was designed out in kumaOS#20 and the
shell half landed as issue #26, which this ADR records.

Three facts pin the design:

- niri's IPC can change outputs live (`Request::Output` with `OutputAction`),
  but that change is temporary by design.
- The baked `/etc/niri/config.kdl` ends with
  `include optional=true "~/.config/niri/local.kdl"`, positioned last. Niri
  merges includes positionally, so the user's lines win over everything the
  image ships, and an image update that rewrites the baked config still
  reaches the user. Niri also watches includes, so writing the file reloads it
  live.
- `output` is a multipart section: an included output block is inserted as-is,
  not field-merged, and a second block for the same output is a parse error.

## Decision

The settings panel gains a Displays page (`displays.rs` holds the machinery;
the page is its only client):

- **Apply live through niri's own IPC.** The page probes outputs with
  `Request::Outputs` on a 2s tick while open (the event stream has no output
  events) and applies each change as `Request::Output`. No kuma process stands
  between.
- **Persist the delta to `~/.config/niri/local.kdl`, never a full config
  copy.** The store holds one block per output, and only the fields the user
  pinned: changing scale writes `scale 1.25` and nothing else. Unset fields
  follow niri's defaults, which is what keeps image defaults flowing. Picking
  Auto removes the field's line; a block that shrinks to nothing disappears.
  A hard apply error persists nothing: the store records only pins niri
  accepted or reported missing.
- **The store is rewritten whole, never appended or patched in place.** Read,
  update the one output's block, write to a temp file, rename. A second block
  for one output is a parse error in niri, so append-style writing would
  corrupt the session's config on the second change.
- **Refuse to rewrite what the shell cannot parse.** The file is
  machine-written, but nothing stops a hand edit. Live applies keep working
  when the store is unreadable; persistence pauses with an honest error on the
  page until the file is fixed. The shell never destroys content it does not
  understand.
- **No kuma.toml schema, no boot-time converger.** Mode, scale, and position
  per monitor are machine state: two machines built from one declaration have
  different monitors. The setting lives outside the declaration, like
  timezone, and niri itself applies the include at startup, so a converger
  would only be a second writer fighting the page's live applies.
- **Reset is deletion.** Deleting the output's block and writing makes niri's
  reload forget the temporary overrides (a config change drops them by
  contract), so defaults flow again with no reboot. `LoadConfigFile` is sent
  on reset only, to skip the watcher's debounce.
- **Niri only.** Under another compositor the page shows an empty state. The
  session mirror stays neutral (ADR-0012); this page is not the mirror, it is
  the commanding seam, and commanding is where the compositors genuinely
  differ.

## Addendum (2026-10-07): the store carries a second tenant

The store as accepted holds output deltas. It also carries one window-rule
the shell writes: the kuma-term ring rule, serialized ahead of the output
blocks on every write. /usr is immutable, so before the image baked the rule
the include was the one niri-writable seam a running shell had, and the rule
rode along. The 44.6.0 image bakes the same rule into its Kuma look
window-rule, which makes the store's copy a shim for installs older than
that image: the rules are additive and identical, so the shim is redundant
there, not conflicting.

The parser still accepts exactly that one rule and refuses any other
window-rule, so a hand edit is never destroyed silently. When the minimum
supported image carries the baked rule, the shim can retire: the shell wrote
the rule, so it understands it, and a rewrite that stops emitting it drops
the copy on the next pin.

## Consequences

- Display choices survive reboot because niri reads them as its own config,
  not because kuma remembers them: one writer of record, the file.
- The delta stays minimal by construction, so most of the image's config
  behavior keeps reaching the user after any number of display tweaks.
- The page trusts the probe, not a ledger: what the dropdowns show is what
  niri reports, refreshed every 2s and after every apply.
- VRR and transform ride the same seam for free; a monitor that cannot do VRR
  says so instead of hiding the row.
- The enable control is not offered when it cannot be honored. A card whose
  output is the only one on shows no enable row: turning it off blanks the
  session, and the page that would undo it lives on the display just switched
  off. A display that is off always keeps its row, since turning it on is
  always safe.
