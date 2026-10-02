# ADR-0007: The lock screen types from key_char, never the key name

Status: accepted (2026-10-02)

## Context

The first cut built the password buffer from `keystroke.key`, the keysym
name, with a "no modifiers" guard. On Wayland that silently corrupts
anything but plain lowercase US-ASCII: shift+a reports key `"a"` with
`shift` set (dropped by the guard; without it the lowercase letter would be
typed), caps lock reports key `"a"` too (lowercased despite the lock),
dead-key accents compose to names like `"aacute"` (dropped), and keys with
no ASCII equivalent fall through to a US-layout guess by physical keycode
(`guess_ascii`: on a German layout, `ö` types as `s`). Space reports key
`"space"`, which is why a space-armed build still failed on a correct
lowercase password containing no capitals. The dot count stays right in
every case, so the field looks fine while the buffer is a different
password.

## Decision

`lock.rs::handle_key` only ever inserts `keystroke.key_char`, the actual
typed character. Shift is the one modifier that may be set (it still yields
a character); ctrl/alt/super are rejected. Enter, backspace, and tab stay
name-based. Space needs no special arm: its `key_char` is `" "`.

## Consequences

- Never "simplify" back to key-name matching; the failure is silent and
  only shows up as PAM rejecting a password the user typed correctly.
- Mid-compose (after a dead key) `key_char` is `None` and nothing is
  inserted. That is correct: the composed character lands on the key that
  completes the sequence.
