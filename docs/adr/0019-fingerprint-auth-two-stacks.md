# ADR-0019: Fingerprint auth rides a split PAM stack, typed passwords never wait behind the reader

Status: accepted (2026-11-23)

## Context

The lock screen, the polkit agent, and the greeter authenticated with
passwords only. An enrolled fingerprint reader (fprintd over D-Bus,
pam_fprintd in the PAM stack) should let a touch unlock, but wiring it
into the same conversation as the password has a trap: pam_fprintd's
no-touch timeout is about 30 seconds, and a conversation that starts
the reader before asking for the password makes every typed password
wait out that timeout first.

The machine's stacks as authselect ships them turned out to already
carry the split: `system-auth` and `password-auth` are fprintd-free,
and `fingerprint-auth` is the fprintd side. A PAM conversation that
starts on `password-auth` never reaches the reader; one that starts on
`fingerprint-auth` is all reader. The module's progress lines ("place
your finger on the fingerprint reader", "Verification timed out")
arrive as ECHO_OFF prompts that ignore the answer, so relaying them
verbatim and answering empty is the whole protocol.

On the polkit card the reader's prompts stream through the same note
line that failure messages use, which first lit the failure styling
(red border) for every touch prompt: the reader talking is not an
emergency.

## Decision

1. **Two stacks, chosen by intent, never mixed.** A typed password
   starts its conversation on `password-auth` when the distro ships
   it (the service-chain probe of ADR-0008 gains a stack check), so
   no password ever waits behind the reader's no-touch timeout. A
   fingerprint attempt starts on `fingerprint-auth`. The chain
   (`kuma-lock` and friends) still owns the password path; the
   fingerprint path is its own conversation, not a chained service.
2. **The exchange is started for the user, not demanded of them.**
   The lock screen runs the fingerprint loop while locked (a touch is
   enough, and a no-touch timeout re-arms it). The polkit card opens
   the listener as soon as it appears (availability probed off the UI
   thread) and re-arms after a timeout; no enter keypress first.
   The greeter keeps enter-on-empty (its fingerprint attempt targets
   the typed username, and a listener without a username would probe
   the wrong identity), and its fake-greetd test pins that.
3. **Typing supersedes listening.** Both helper cancel arms (the
   socket mode shuts the connection down, the spawn mode kills the
   timeout-wrapped child) are handed to the UI the moment an exchange
   connects; the first keystroke uses them. A superseded fingerprint
   exchange ends silent: no "authentication failed" flash for the
   user's own keystrokes.
4. **The reader talks in status, failures wear the alarm.** The note
   line under the password field carries two shapes (`AuthNote`):
   Status (the module's relayed lines) in the dim color with the
   resting border, Failure (a rejected password, a broken helper)
   keeping the red. Both the lock screen and the polkit card share
   the type.
5. **Availability is probed, not assumed, and a probe failure is
   loud.** `fingerprint.rs::available` walks the fprintd D-Bus tree
   (bus name, object paths, and interfaces all carry a capital F on
   fprintd 1.94: `net.reactivated.Fprint`, Manager at
   `/net/reactivated/Fprint/Manager`) and every unavailability reason
   logs at info. A silent false here is a reader that never listens.

## Consequences

- Enrolled machines unlock by touch at all three surfaces; machines
  without prints or without a reader are exactly as they were (the
  probe answers no, the hint line stays off).
- A wrong password is never slower than before: it rides the
  password-only stack, so the reader's timeout is somebody else's
  problem.
- The enter-on-empty greeter gesture means "try the finger": the
  password field's content decides which conversation starts.
- The fprintd spelling is load-bearing. If a future fprintd changes
  its bus casing again, the journal says so at info; do not "fix" the
  capital F without checking `busctl --system introspect`.
- Status and failure stay typed apart. Any new line that wants the
  red border must say so by being a Failure, not by being present.
