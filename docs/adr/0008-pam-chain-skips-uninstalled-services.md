# ADR-0008: The PAM chain skips services with no /etc/pam.d file

Status: accepted (2026-10-02)

## Context

The chain tried `kuma-lock` → `swaylock` → `vlock` and fell through to the
next service only when `pam_start` failed. On Linux-PAM that never happens:
`pam_start` succeeds for any name and defers reading the config to the first
call, and a service with no `/etc/pam.d/<service>` file is answered from the
`other` policy (`pam_deny` on kumaOS). Every unlock attempt therefore died
on the first chain entry (`kuma-lock`, which nothing has ever shipped) with
the same `AUTH_ERR` a wrong password returns, so a correct password "failed"
with no trace of why. A standalone probe confirmed it: `pam_start` returns
Success and `pam_authenticate` returns 7 (Authentication failure) both for a
missing service and for a genuinely wrong password on a real one; the two
are indistinguishable from the caller's side.

## Decision

`lock.rs::authenticate` skips a service whose `/etc/pam.d/<service>` file
does not exist, so the chain reaches the first *installed* stack. A real
stack's answer stays final: `authenticate`/`acct_mgmt` errors propagate
immediately instead of falling through. A deny is a definitive answer, and
re-running the same password in the next service could only repeat the deny
and double-count failures for any module that tracks them.

## Consequences

- The shell unlocks on any distro regardless of which locker service files
  exist; on kumaOS today that is always `vlock` (shipped by `kbd`, not by a
  vlock package).
- Never "simplify" the file-existence check away: Linux-PAM gives no other
  signal at chain-selection time, and without it the first missing entry
  rejects every password.
- "First that starts wins" in the glossary (CONTEXT.md) means "first
  *installed* service wins".
- Someday shipping `/etc/pam.d/kuma-lock` from the image build (vlock's
  stack: `auth include system-auth`, `account required pam_permit.so`)
  makes the first chain entry real with no code change; audit logs then
  name `kuma-lock` instead of borrowing `vlock`'s.
