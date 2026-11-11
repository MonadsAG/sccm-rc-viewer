//! Wake-on-LAN: send a magic packet, plus a best-effort host→MAC cache populated
//! from the ARP table after a successful connect (so a later attempt to a powered-
//! off host can wake it). Cache: `%LOCALAPPDATA%\sccm-rc\macs.txt` (`host=MAC`).

use std::net::{ToSocketAddrs, UdpSocket};

/// Parse a MAC like `AA-BB-CC-DD-EE-FF` / `aa:bb:...` / `aabbccddeeff`.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(mac)
}

/// Parse a MAC as printed by `arp -a`: Windows prints `aa-bb-cc-dd-ee-ff`,
/// macOS prints `a:b:c:d:e:f` with leading zeros dropped.
fn parse_arp_mac(tok: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = tok.split(['-', ':']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (b, p) in mac.iter_mut().zip(&parts) {
        if p.is_empty() || p.len() > 2 {
            return None;
        }
        *b = u8::from_str_radix(p, 16).ok()?;
    }
    Some(mac)
}

pub fn fmt_mac(mac: [u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

/// Send the Wake-on-LAN magic packet (6×0xFF + 16×MAC) as a UDP broadcast on the
/// usual WoL ports (9 and 7).
pub fn send(mac: [u8; 6]) -> std::io::Result<()> {
    let mut pkt = vec![0xFFu8; 6];
    for _ in 0..16 {
        pkt.extend_from_slice(&mac);
    }
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    for port in [9u16, 7] {
        let _ = sock.send_to(&pkt, ("255.255.255.255", port));
    }
    Ok(())
}

fn cache_path() -> std::path::PathBuf {
    let dir = crate::paths::data_dir();
    dir.join("macs.txt")
}

pub fn cached_mac(host: &str) -> Option<[u8; 6]> {
    let data = std::fs::read_to_string(cache_path()).ok()?;
    for line in data.lines() {
        if let Some((h, m)) = line.split_once('=') {
            if h.trim().eq_ignore_ascii_case(host.trim()) {
                return parse_mac(m);
            }
        }
    }
    None
}

pub fn cache_mac(host: &str, mac: [u8; 6]) {
    let host = host.trim();
    let mut lines: Vec<String> = std::fs::read_to_string(cache_path())
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            l.split_once('=')
                .map(|(h, _)| !h.trim().eq_ignore_ascii_case(host))
                .unwrap_or(false)
        })
        .map(|l| l.to_string())
        .collect();
    lines.push(format!("{host}={}", fmt_mac(mac)));
    // Atomic write (temp + rename) so concurrent viewer instances can't clobber
    // the cache or leave it half-written.
    let p = cache_path();
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, lines.join("\r\n").as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Resolve `host` to an IP and, if it's in the ARP table (i.e. reachable / just
/// contacted), cache its MAC for future Wake-on-LAN. Best-effort, never errors.
pub fn lookup_and_cache(host: &str) {
    let Some(addr) = (host, 2701u16)
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.find(|a| a.is_ipv4()))
    else {
        return;
    };
    let ip = addr.ip().to_string();
    let mut cmd = std::process::Command::new("arp");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    // Windows: `arp -a <ip>` filters the table to that host. BSD/macOS `arp`
    // rejects a host after `-a` (it prints usage), so ask for the one entry
    // with `-n <ip>` there (`-n` = no reverse DNS lookup).
    let flag = if cfg!(windows) { "-a" } else { "-n" };
    let Ok(out) = cmd.args([flag, ip.as_str()]).output() else {
        return;
    };
    let s = String::from_utf8_lossy(&out.stdout);
    for line in s.lines() {
        if !line.contains(&ip) {
            continue;
        }
        if let Some(mac) = line.split_whitespace().find_map(parse_arp_mac) {
            cache_mac(host, mac);
            return;
        }
    }
}

#[cfg(test)]
mod arp_tests {
    use super::parse_arp_mac;

    #[test]
    fn windows_style() {
        assert_eq!(
            parse_arp_mac("aa-bb-cc-dd-ee-ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
    }

    #[test]
    fn macos_style_unpadded() {
        assert_eq!(parse_arp_mac("a:b:c:d:e:f"), Some([10, 11, 12, 13, 14, 15]));
        assert_eq!(
            parse_arp_mac("0:1b:2c:3d:4e:5f"),
            Some([0x00, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f])
        );
    }

    #[test]
    fn rejects_non_macs() {
        assert_eq!(parse_arp_mac("(10.0.0.1)"), None);
        assert_eq!(parse_arp_mac("on"), None);
        assert_eq!(parse_arp_mac("a:b:c:d:e"), None);
        assert_eq!(parse_arp_mac("aaa:b:c:d:e:f"), None);
    }
}
