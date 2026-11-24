//! Fingerprint auth support: what the lock screen needs to know before
//! it starts a PAM conversation that might wait for a finger.
//!
//! Two facts about pam_fprintd shape everything here, both established
//! against this machine's hardware (Goodix MOC, 2026-11-23):
//!
//! 1. Its status messages ("Place your finger on the fingerprint
//!    reader", "Swipe your finger again", "Verification timed out")
//!    arrive as ECHO_OFF *prompts*, not TEXT_INFO, and the module
//!    ignores whatever the conversation answers. A conversation that
//!    only answers prompts with a password would feed the password to
//!    a prompt nobody reads, and a conversation that refuses extra
//!    prompts would break the exchange outright. Finger-shaped prompts
//!    get an empty answer and their text relayed to the UI.
//!
//! 2. An untouched verify runs ~30 s to the daemon's "Verification
//!    timed out" before the stack falls through (measured: `su` with
//!    stdin at EOF ended at 34.5 s, the fingerprint phase timing out
//!    before pam_unix asked). `fprintd-verify` itself never cancels.
//!    So the fingerprint attempt is bounded, but slow to fail, and a
//!    typed password sent through a fprintd-first stack (system-auth,
//!    which kuma-lock includes) eats that wait before pam_unix runs.
//!    The password attempt therefore prefers the authselect
//!    password-only stack, and the fingerprint attempt rides the
//!    authselect fingerprint-only stack: two services, two
//!    conversations, the same split GDM makes.

/// Whether a PAM prompt (or helper line, or greetd auth message) is
/// pam_fprintd talking about the finger rather than asking for a
/// secret. The module ignores the answer either way; the caller shows
/// the text and answers empty so no password rides into it.
pub fn finger_prompt(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("finger") || lower.contains("timed out")
}

/// The user the availability probe asks about: the process owner
/// (`USER`, else `LOGNAME`). Empty when neither is set, which callers
/// treat as fingerprint-unavailable.
pub fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

/// Whether fingerprint auth can work for the user right now: a reader
/// exists on the system bus and at least one finger is enrolled for
/// them. One D-Bus round-trip pair, meant to run off the UI thread at
/// lock time; a missing daemon, an absent reader, and zero prints all
/// read as unavailable, which keeps the lock screen's behavior
/// unchanged for everyone else.
pub fn available(user: &str) -> bool {
    let Ok(conn) = zbus::blocking::Connection::system() else {
        return false;
    };
    let Ok(manager) = zbus::blocking::Proxy::new(
        &conn,
        "net.reactivated.fprint",
        "/net/reactivated/fprint",
        "net.reactivated.fprint.Manager",
    ) else {
        return false;
    };
    // no reader: the call errors, and so does the whole feature
    let Ok(device) = manager.call::<_, _, zbus::zvariant::OwnedObjectPath>("GetDefaultDevice", &())
    else {
        return false;
    };
    let Ok(device) = zbus::blocking::Proxy::new(
        &conn,
        "net.reactivated.fprint",
        device,
        "net.reactivated.fprint.Device",
    ) else {
        return false;
    };
    // no prints enrolled: an empty list answers cleanly on 1.94, but
    // some builds error instead; both read as unavailable
    device
        .call::<_, _, Vec<String>>("ListEnrolledFingers", &(user,))
        .map(|fingers| !fingers.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::finger_prompt;

    #[test]
    fn finger_prompts_are_classified_and_secrets_are_not() {
        // the strings in this machine's pam_fprintd.so, plus the retry
        // and timeout shapes the module sends mid-verify
        assert!(finger_prompt(
            "Place your finger on the fingerprint reader"
        ));
        assert!(finger_prompt("Place your finger on the reader again"));
        assert!(finger_prompt("Swipe your finger again"));
        assert!(finger_prompt("Verification timed out"));
        // real secret prompts: never classified, whatever they say
        assert!(!finger_prompt("Password: "));
        assert!(!finger_prompt("martin's password"));
        // and the empty-password danger case: a prompt that *mentions*
        // a password stays a secret prompt
        assert!(!finger_prompt("Enter your password to unlock"));
    }
}
