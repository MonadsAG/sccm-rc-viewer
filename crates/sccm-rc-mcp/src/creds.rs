//! Logon credentials for the headless session. No prompt: the user comes from
//! `SCCM_USER` or the viewer's last user, the password from the viewer's
//! Keychain item for that user or else `SCCM_PASS`.

use sccm_rc_core::keychain::Lookup;
use sccm_rc_core::LogonCredentials;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredError {
    #[error("no user: set SCCM_USER (DOMAIN\\user) or log on once with the viewer")]
    NoUser,
    #[error("invalid user name: {0}")]
    BadUser(String),
    #[error("no password for {0}: save it in the viewer (Keychain) or set SCCM_PASS")]
    NoPassword(String),
}

/// The viewer's last explicit-logon user (`data_dir()/last_user.txt`).
pub fn last_user() -> Option<String> {
    std::fs::read_to_string(sccm_rc_core::paths::data_dir().join("last_user.txt"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn resolve(
    env: impl Fn(&str) -> Option<String>,
    last_user: Option<String>,
    keychain: Lookup,
) -> Result<LogonCredentials, CredError> {
    let var = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    let user = var("SCCM_USER")
        .or(last_user.filter(|u| !u.trim().is_empty()))
        .ok_or(CredError::NoUser)?;
    let domain = var("SCCM_DOMAIN");
    let mut creds = LogonCredentials::parse(&user, domain.as_deref(), "")
        .map_err(|e| CredError::BadUser(e.to_string()))?;
    // The viewer keys its Keychain items by `DOMAIN\user`.
    let account = creds.display_name();
    creds.password = keychain(&account)
        .or_else(|| env("SCCM_PASS").filter(|p| !p.is_empty()))
        .ok_or(CredError::NoPassword(account))?;
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn kc_alice(account: &str) -> Option<String> {
        (account == "CORP\\alice").then(|| "from-keychain".to_string())
    }

    fn kc_none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn env_user_and_keychain_password() {
        let c = resolve(
            env_of(&[("SCCM_USER", "CORP\\alice"), ("SCCM_PASS", "from-env")]),
            Some("OTHER\\bob".into()),
            kc_alice,
        )
        .unwrap();
        assert_eq!(c.display_name(), "CORP\\alice");
        assert_eq!(c.password, "from-keychain");
    }

    #[test]
    fn last_user_when_no_env() {
        let c = resolve(env_of(&[]), Some("CORP\\alice".into()), kc_alice).unwrap();
        assert_eq!(c.display_name(), "CORP\\alice");
        assert_eq!(c.password, "from-keychain");
    }

    #[test]
    fn domain_from_env_applies_to_bare_user() {
        let c = resolve(
            env_of(&[("SCCM_USER", "alice"), ("SCCM_DOMAIN", "CORP")]),
            None,
            kc_alice,
        )
        .unwrap();
        assert_eq!(c.password, "from-keychain");
    }

    #[test]
    fn env_password_fallback() {
        let c = resolve(
            env_of(&[("SCCM_USER", "CORP\\alice"), ("SCCM_PASS", "from-env")]),
            None,
            kc_none,
        )
        .unwrap();
        assert_eq!(c.password, "from-env");
    }

    #[test]
    fn no_password_errors() {
        let e = resolve(env_of(&[("SCCM_USER", "CORP\\alice")]), None, kc_none).unwrap_err();
        assert_eq!(e, CredError::NoPassword("CORP\\alice".into()));
    }

    #[test]
    fn no_user_errors() {
        assert_eq!(
            resolve(env_of(&[("SCCM_PASS", "x")]), None, kc_none).unwrap_err(),
            CredError::NoUser
        );
        assert_eq!(
            resolve(env_of(&[("SCCM_USER", "  ")]), Some(" ".into()), kc_none).unwrap_err(),
            CredError::NoUser
        );
    }
}
