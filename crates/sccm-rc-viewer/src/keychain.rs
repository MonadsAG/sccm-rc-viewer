//! Optional password storage for the explicit (NTLM) logon, in the user's
//! login Keychain on macOS.
//!
//! Opt-in only: the logon overlay offers a "save password" checkbox, and the
//! App stores the password only after the logon actually succeeded. One item
//! per `DOMAIN\user` (a domain password is the same for every target), as a
//! generic password under [`SERVICE`]. A logon the server rejects deletes the
//! saved item, so a stale password cannot cause repeated failed logons.
//!
//! On other platforms [`AVAILABLE`] is false and every call is a no-op; the
//! overlay then hides the checkbox.

/// Whether this build can store passwords at all.
pub const AVAILABLE: bool = cfg!(target_os = "macos");

/// Keychain service name of the items (shown as "Name" in Keychain Access).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SERVICE: &str = "SCCM RC logon";

/// Signature of [`load`], so the overlay can be tested with a fake store.
pub type Lookup = fn(&str) -> Option<String>;

/// The saved password for `account` (`DOMAIN\user`), if there is one.
/// Errors (no item, access denied in the system dialog) are logged and
/// treated as "nothing saved".
#[cfg(target_os = "macos")]
pub fn load(account: &str) -> Option<String> {
    match security_framework::passwords::get_generic_password(SERVICE, &key(account)) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                let mut bytes = e.into_bytes();
                wipe(&mut bytes);
                tracing::warn!("keychain: saved password is not UTF-8; ignoring it");
                None
            }
        },
        Err(e) if e.code() == ERR_ITEM_NOT_FOUND => None,
        Err(e) => {
            tracing::warn!(code = e.code(), "keychain: cannot read the saved password");
            None
        }
    }
}

/// Store (or replace) the password for `account`.
#[cfg(target_os = "macos")]
pub fn save(account: &str, password: &str) {
    match security_framework::passwords::set_generic_password(
        SERVICE,
        &key(account),
        password.as_bytes(),
    ) {
        Ok(()) => tracing::info!(account, "keychain: password saved"),
        Err(e) => tracing::warn!(code = e.code(), "keychain: cannot save the password"),
    }
}

/// Remove the saved password for `account`, if any.
#[cfg(target_os = "macos")]
pub fn delete(account: &str) {
    match security_framework::passwords::delete_generic_password(SERVICE, &key(account)) {
        Ok(()) => tracing::info!(account, "keychain: saved password removed"),
        Err(e) if e.code() == ERR_ITEM_NOT_FOUND => {}
        Err(e) => tracing::warn!(code = e.code(), "keychain: cannot remove the password"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn load(_account: &str) -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn save(_account: &str, _password: &str) {}

#[cfg(not(target_os = "macos"))]
pub fn delete(_account: &str) {}

/// `errSecItemNotFound`.
#[cfg(target_os = "macos")]
const ERR_ITEM_NOT_FOUND: i32 = -25300;

/// Item key for an account: Windows treats `DOMAIN\user` case-insensitively,
/// so `corp\Alice` and `CORP\alice` share one item.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn key(account: &str) -> String {
    account.trim().to_lowercase()
}

#[cfg(target_os = "macos")]
fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: plain byte write; volatile so it is not optimised away.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ignores_case_and_padding() {
        assert_eq!(key(" CORP\\Alice "), "corp\\alice");
        assert_eq!(key("corp\\alice"), key("CORP\\ALICE"));
    }

    /// Writes to the real login Keychain: `cargo test -p sccm-rc-viewer -- --ignored keychain`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn roundtrip_in_the_real_keychain() {
        let acct = "EXAMPLE\\sccm-rc-selftest";
        delete(acct);
        assert_eq!(load(acct), None);
        save(acct, "first");
        save("example\\SCCM-RC-SELFTEST", "second"); // same item, replaced
        assert_eq!(load(acct).as_deref(), Some("second"));
        delete(acct);
        assert_eq!(load(acct), None);
    }
}
