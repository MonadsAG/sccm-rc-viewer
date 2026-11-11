//! Explicit logon credentials (`DOMAIN\user` + password).
//!
//! Used by the non-Windows NTLM backend, which has no logged-on identity to
//! fall back on. Platform-independent so callers (core, viewer) can pass
//! credentials without `cfg` noise; the Windows backend currently ignores them
//! and keeps single sign-on.

use crate::Error;
use std::fmt;

/// `DOMAIN\user` + password. `Debug` never prints the password.
#[derive(Clone, PartialEq, Eq)]
pub struct LogonCredentials {
    pub domain: String,
    pub user: String,
    pub password: String,
}

impl fmt::Debug for LogonCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogonCredentials")
            .field("domain", &self.domain)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Drop for LogonCredentials {
    fn drop(&mut self) {
        // Best-effort wipe of the password bytes before the allocation is freed.
        // SAFETY: overwriting with zero bytes keeps the String valid UTF-8.
        unsafe {
            for b in self.password.as_bytes_mut() {
                std::ptr::write_volatile(b, 0);
            }
        }
    }
}

impl LogonCredentials {
    /// Parse `DOMAIN\user`, `user@domain` or a bare `user`. `domain` is used
    /// when the user string carries none.
    pub fn parse(user: &str, domain: Option<&str>, password: &str) -> crate::Result<Self> {
        let user = user.trim();
        let (d, u) = if let Some((d, u)) = user.split_once('\\') {
            (d.to_string(), u.to_string())
        } else if let Some((u, d)) = user.split_once('@') {
            (d.to_string(), u.to_string())
        } else {
            (domain.unwrap_or("").trim().to_string(), user.to_string())
        };
        if u.is_empty() {
            return Err(Error::Sspi("empty user name".into()));
        }
        Ok(Self {
            domain: d,
            user: u,
            password: password.to_string(),
        })
    }

    /// `DOMAIN\user` (or just `user` when there is no domain), for display.
    pub fn display_name(&self) -> String {
        if self.domain.is_empty() {
            self.user.clone()
        } else {
            format!("{}\\{}", self.domain, self.user)
        }
    }

    /// Read `SCCM_USER` (+ optional `SCCM_DOMAIN`) and `SCCM_PASS`.
    pub fn from_env() -> crate::Result<Self> {
        let user = std::env::var("SCCM_USER")
            .map_err(|_| Error::Sspi("no credentials: SCCM_USER is not set (expected DOMAIN\\user)".into()))?;
        let pass = std::env::var("SCCM_PASS")
            .map_err(|_| Error::Sspi("no credentials: SCCM_PASS is not set".into()))?;
        let domain = std::env::var("SCCM_DOMAIN").ok();
        Self::parse(&user, domain.as_deref(), &pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let c = LogonCredentials::parse("CORP\\alice", None, "pw").unwrap();
        assert_eq!((c.domain.as_str(), c.user.as_str()), ("CORP", "alice"));
        assert_eq!(c.display_name(), "CORP\\alice");
        let c = LogonCredentials::parse("alice@corp.example", None, "pw").unwrap();
        assert_eq!((c.domain.as_str(), c.user.as_str()), ("corp.example", "alice"));
        let c = LogonCredentials::parse(" alice ", Some("CORP"), "pw").unwrap();
        assert_eq!((c.domain.as_str(), c.user.as_str()), ("CORP", "alice"));
        assert!(LogonCredentials::parse("CORP\\", None, "pw").is_err());
        assert!(LogonCredentials::parse("", None, "pw").is_err());
    }

    #[test]
    fn debug_never_prints_the_password() {
        let c = LogonCredentials::parse("d\\u", None, "hunter2").unwrap();
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
