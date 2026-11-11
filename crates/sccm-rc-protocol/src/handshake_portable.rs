//! Portable (non-Windows) SSPI-style handshake + message sealing on top of
//! [`sspi`] (sspi-rs).
//!
//! Exposes the same `SspiSession` surface as the native Windows module
//! (`handshake.rs`) so `sccm-rc-core` and the test bins compile unchanged.
//!
//! Differences from the Windows backend:
//!
//! * **Explicit credentials** (`DOMAIN\user` + password). There is no logged-on
//!   identity or ticket cache to fall back on, so `new_for_target` reads them
//!   from the environment (`SCCM_USER`, optional `SCCM_DOMAIN`, `SCCM_PASS`);
//!   use [`SspiSession::new_with_credentials`] to pass them directly.
//! * **NTLM only, raw NTLMSSP tokens.** Kerberos needs a hostname-based SPN
//!   and a reachable KDC; neither exists for a bare IP from a non-domain-joined
//!   machine. sspi-rs' `Negotiate` restricted to NTLM sends the same raw
//!   NTLMSSP tokens but first stalls ~4 s on KDC DNS lookups, so this module
//!   drives `sspi::Ntlm` directly. (The Windows client wraps NTLM in SPNEGO;
//!   a Negotiate acceptor normally takes raw NTLMSSP too. If a target rejects
//!   it, SPNEGO wrapping is the fallback.)
//! * **No mutual authentication.** NTLM cannot prove the server's identity, so
//!   [`SspiSession::mutual_auth`] is always `false`.
//! * **Explicit sequence numbers.** Windows `EncryptMessage` tracks the NTLM
//!   sequence number internally (the original viewer passes `MessageSeqNo = 0`).
//!   sspi-rs mixes the caller-supplied number into the signature, so this
//!   module keeps one counter per direction, starting at 0.

use crate::Error;
use sspi::ntlm::NtlmConfig;
use sspi::{
    AuthIdentity, BufferType, ClientRequestFlags, CredentialUse, DataRepresentation,
    DecryptionFlags, EncryptionFlags, Ntlm, SecurityBuffer, SecurityBufferRef, SecurityStatus,
    Sspi, SspiImpl, Username,
};
use std::fmt;
use tracing::{debug, trace};

/// Builds the SPN passed as the target name. NTLM does not validate it, but
/// the Windows client sends the same string.
pub fn build_spn(target_host: &str) -> String {
    format!("TERMSRV/{target_host}")
}

/// Per-message overhead sizes (`SECPKG_ATTR_SIZES` equivalent). For NTLM the
/// security trailer is the 16-byte message signature.
#[derive(Debug, Clone, Copy)]
pub struct MessageSizes {
    pub cb_max_token: u32,
    pub cb_max_signature: u32,
    pub cb_block_size: u32,
    pub cb_security_trailer: u32,
}

/// One round-trip of the handshake.
#[derive(Debug)]
pub struct HandshakeStep {
    pub output: Vec<u8>,
    pub done: bool,
}

pub use crate::credentials::LogonCredentials;

/// Handshake + sealing state for one SCCM RC connection.
pub struct SspiSession {
    ctx: Ntlm,
    creds: <Ntlm as SspiImpl>::CredentialsHandle,
    spn: String,
    done: bool,
    sizes: Option<MessageSizes>,
    /// `NegotiateFlags` read off the wire: the server's CHALLENGE (Type 2) and
    /// our AUTHENTICATE (Type 3). These, not sspi-rs' (always empty) response
    /// flags, decide whether sealing was really negotiated.
    server_flags: Option<u32>,
    client_flags: Option<u32>,
    /// Per-direction message counters (see module docs).
    send_seq: u32,
    recv_seq: u32,
}

impl fmt::Debug for SspiSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SspiSession")
            .field("done", &self.done)
            .field("send_seq", &self.send_seq)
            .field("recv_seq", &self.recv_seq)
            .finish()
    }
}

fn sspi_err(what: &str, e: sspi::Error) -> Error {
    Error::Sspi(format!("{what}: {e}"))
}

impl SspiSession {
    /// Create a session using credentials from the environment
    /// (`SCCM_USER`, `SCCM_DOMAIN`, `SCCM_PASS`).
    pub fn new_for_target(target_host: &str) -> crate::Result<Self> {
        Self::new_with_credentials(target_host, &LogonCredentials::from_env()?)
    }

    /// Create a session with explicit credentials.
    pub fn new_with_credentials(target_host: &str, c: &LogonCredentials) -> crate::Result<Self> {
        let username = Username::new(
            &c.user,
            if c.domain.is_empty() {
                None
            } else {
                Some(&c.domain)
            },
        )
        .map_err(|e| Error::Sspi(format!("invalid user name: {e}")))?;
        let identity = AuthIdentity {
            username,
            password: c.password.clone().into(),
        };

        let mut ctx = Ntlm::with_config(NtlmConfig::new("SCCM-RC-VIEWER".to_string()));

        let acquired = ctx
            .acquire_credentials_handle()
            .with_credential_use(CredentialUse::Outbound)
            .with_auth_data(&identity)
            .execute(&mut ctx)
            .map_err(|e| sspi_err("acquire_credentials_handle", e))?;

        let spn = build_spn(target_host);
        debug!(%spn, user = %c.user, domain = %c.domain, "portable NTLM session created");

        Ok(Self {
            ctx,
            creds: acquired.credentials_handle,
            spn,
            done: false,
            sizes: None,
            server_flags: None,
            client_flags: None,
            send_seq: 0,
            recv_seq: 0,
        })
    }

    /// Pump one round of the handshake. `input` = bytes from the peer (empty
    /// for the first call).
    pub fn step(&mut self, input: &[u8]) -> crate::Result<HandshakeStep> {
        let mut input_bufs = vec![SecurityBuffer::new(input.to_vec(), BufferType::Token)];
        let mut output_bufs = vec![SecurityBuffer::new(Vec::new(), BufferType::Token)];

        // No MUTUAL_AUTH: NTLM cannot provide it. Everything else mirrors the
        // flag set the native backend requests.
        let flags = ClientRequestFlags::CONFIDENTIALITY
            | ClientRequestFlags::INTEGRITY
            | ClientRequestFlags::REPLAY_DETECT
            | ClientRequestFlags::SEQUENCE_DETECT
            | ClientRequestFlags::ALLOCATE_MEMORY
            | ClientRequestFlags::CONNECTION;

        let result = {
            let builder = self
                .ctx
                .initialize_security_context()
                .with_credentials_handle(&mut self.creds)
                .with_context_requirements(flags)
                .with_target_data_representation(DataRepresentation::Native)
                .with_target_name(&self.spn)
                .with_output(&mut output_bufs);
            let mut builder = if input.is_empty() {
                builder
            } else {
                builder.with_input(&mut input_bufs)
            };
            let resolved = self
                .ctx
                .initialize_security_context_impl(&mut builder)
                .map_err(|e| sspi_err("initialize_security_context", e))?
                .resolve_to_result()
                .map_err(|e| sspi_err("initialize_security_context", e))?;
            resolved
        };

        let output = output_bufs.remove(0).buffer;
        if let Some((MSG_CHALLENGE, f)) = ntlm_message_flags(input) {
            self.server_flags = Some(f);
        }
        if let Some((MSG_AUTHENTICATE, f)) = ntlm_message_flags(&output) {
            self.client_flags = Some(f);
        }
        trace!(status = ?result.status, output_bytes = output.len(), "ISC returned");

        if matches!(
            result.status,
            SecurityStatus::CompleteNeeded | SecurityStatus::CompleteAndContinue
        ) {
            self.ctx
                .complete_auth_token(&mut output_bufs)
                .map_err(|e| sspi_err("complete_auth_token", e))?;
        }

        let done = matches!(
            result.status,
            SecurityStatus::Ok | SecurityStatus::CompleteNeeded
        );
        self.done = done;
        if done {
            debug!(
                server_flags = %fmt_flags(self.server_flags),
                client_flags = %fmt_flags(self.client_flags),
                sealing = self.confidentiality(),
                "NTLM negotiate flags"
            );
        }
        Ok(HandshakeStep { output, done })
    }

    /// Per-message overhead sizes. Cached after the first call.
    pub fn message_sizes(&mut self) -> crate::Result<MessageSizes> {
        let s = self
            .ctx
            .query_context_sizes()
            .map_err(|e| sspi_err("query_context_sizes", e))?;
        let ms = MessageSizes {
            cb_max_token: s.max_token,
            cb_max_signature: s.max_signature,
            cb_block_size: s.block,
            cb_security_trailer: s.security_trailer,
        };
        self.sizes = Some(ms);
        Ok(ms)
    }

    fn ensure_sizes(&mut self) -> crate::Result<MessageSizes> {
        match self.sizes {
            Some(s) => Ok(s),
            None => self.message_sizes(),
        }
    }

    /// Whether the established context encrypts messages.
    ///
    /// sspi-rs' NTLM client reports no granted flags, so this checks the
    /// negotiation itself: the handshake must be complete, the server's
    /// CHALLENGE must offer `NTLMSSP_NEGOTIATE_SEAL`, and our AUTHENTICATE must
    /// confirm it. Anything else (no challenge seen, flag missing) is `false`,
    /// so core's fail-closed check refuses the channel.
    pub fn confidentiality(&self) -> bool {
        self.done && sealing_negotiated(self.server_flags, self.client_flags)
    }

    /// Always `false`: NTLM does not authenticate the server.
    pub fn mutual_auth(&self) -> bool {
        false
    }

    /// The security package in use. Always NTLM here.
    pub fn package_name(&self) -> Option<String> {
        if self.done {
            Some("NTLM".to_string())
        } else {
            None
        }
    }

    /// Seal a plaintext application message into the SecFilter frame body
    /// `[u16 LE data_len][encrypted data][u16 LE token_len][signature]`.
    pub fn seal(&mut self, plaintext: &[u8]) -> crate::Result<Vec<u8>> {
        let sizes = self.ensure_sizes()?;
        let mut data = plaintext.to_vec();
        let mut token = vec![0u8; sizes.cb_security_trailer as usize];

        {
            let mut bufs = [
                SecurityBufferRef::token_buf(&mut token),
                SecurityBufferRef::data_buf(&mut data),
            ];
            self.ctx
                .encrypt_message(EncryptionFlags::empty(), &mut bufs, self.send_seq)
                .map_err(|e| sspi_err("encrypt_message", e))?;
        }
        self.send_seq = self.send_seq.wrapping_add(1);

        if data.len() > u16::MAX as usize || token.len() > u16::MAX as usize {
            return Err(Error::Protocol(format!(
                "sealed frame exceeds u16 length prefix: data={} token={}",
                data.len(),
                token.len()
            )));
        }

        let mut out = Vec::with_capacity(2 + data.len() + 2 + token.len());
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(&(token.len() as u16).to_le_bytes());
        out.extend_from_slice(&token);
        Ok(out)
    }

    /// Unseal an SCCM data-phase frame body back to plaintext. Inverse of
    /// [`seal`](Self::seal). Frames must be unsealed strictly in arrival order.
    pub fn unseal(&mut self, frame_body: &[u8]) -> crate::Result<Vec<u8>> {
        if frame_body.len() < 4 {
            return Err(Error::Protocol("sealed frame too short".into()));
        }
        let data_len = u16::from_le_bytes([frame_body[0], frame_body[1]]) as usize;
        let off_token_len = 2 + data_len;
        if off_token_len + 2 > frame_body.len() {
            return Err(Error::Protocol("sealed frame: bad data_len".into()));
        }
        let token_len =
            u16::from_le_bytes([frame_body[off_token_len], frame_body[off_token_len + 1]]) as usize;
        let token_off = off_token_len + 2;
        if token_off + token_len > frame_body.len() {
            return Err(Error::Protocol("sealed frame: bad token_len".into()));
        }

        let mut data = frame_body[2..2 + data_len].to_vec();
        let mut token = frame_body[token_off..token_off + token_len].to_vec();

        let flags = {
            let mut bufs = [
                SecurityBufferRef::token_buf(&mut token),
                SecurityBufferRef::data_buf(&mut data),
            ];
            self.ctx
                .decrypt_message(&mut bufs, self.recv_seq)
                .map_err(|e| sspi_err("decrypt_message", e))?
        };
        self.recv_seq = self.recv_seq.wrapping_add(1);

        // A frame that was only signed, not encrypted, must not be accepted on
        // a channel we treat as confidential.
        if flags.contains(DecryptionFlags::WRAP_NO_ENCRYPT) {
            return Err(Error::Sspi(
                "unsealed frame was not encrypted (QOP no-encrypt)".into(),
            ));
        }
        Ok(data)
    }
}

/// NTLMSSP message signature and the message types whose flags we read.
const NTLMSSP_SIGNATURE: &[u8; 8] = b"NTLMSSP\0";
const MSG_CHALLENGE: u32 = 2;
const MSG_AUTHENTICATE: u32 = 3;
/// MS-NLMP 2.2.2.5 `NTLMSSP_NEGOTIATE_SEAL`.
const NTLMSSP_NEGOTIATE_SEAL: u32 = 0x0000_0020;

/// `(message type, NegotiateFlags)` of a raw NTLMSSP token, or `None` if it
/// isn't one (or is too short). Flag offsets per MS-NLMP 2.2.1: NEGOTIATE 12,
/// CHALLENGE 20, AUTHENTICATE 60.
fn ntlm_message_flags(tok: &[u8]) -> Option<(u32, u32)> {
    if tok.len() < 12 || &tok[..8] != NTLMSSP_SIGNATURE {
        return None;
    }
    let ty = u32::from_le_bytes(tok[8..12].try_into().ok()?);
    let off = match ty {
        1 => 12,
        MSG_CHALLENGE => 20,
        MSG_AUTHENTICATE => 60,
        _ => return None,
    };
    let f = u32::from_le_bytes(tok.get(off..off + 4)?.try_into().ok()?);
    Some((ty, f))
}

fn sealing_negotiated(server: Option<u32>, client: Option<u32>) -> bool {
    matches!((server, client), (Some(s), Some(c))
        if s & NTLMSSP_NEGOTIATE_SEAL != 0 && c & NTLMSSP_NEGOTIATE_SEAL != 0)
}

fn fmt_flags(f: Option<u32>) -> String {
    f.map(|v| format!("{v:#010x}"))
        .unwrap_or_else(|| "none".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spn_format_matches_original() {
        assert_eq!(build_spn("192.0.2.10"), "TERMSRV/192.0.2.10");
    }

    /// In-process NTLM server paired with `SspiSession`, used to
    /// check the handshake and the seal/unseal frame layout offline.
    struct LoopbackServer {
        ctx: Ntlm,
        creds: <Ntlm as SspiImpl>::CredentialsHandle,
        send_seq: u32,
        recv_seq: u32,
    }

    impl LoopbackServer {
        fn new(c: &LogonCredentials) -> Self {
            let identity = AuthIdentity {
                username: Username::new(&c.user, Some(&c.domain)).unwrap(),
                password: c.password.clone().into(),
            };
            let mut ctx = Ntlm::new();
            let acquired = ctx
                .acquire_credentials_handle()
                .with_credential_use(CredentialUse::Inbound)
                .with_auth_data(&identity)
                .execute(&mut ctx)
                .unwrap();
            Self {
                ctx,
                creds: acquired.credentials_handle,
                send_seq: 0,
                recv_seq: 0,
            }
        }

        /// Returns `(token_for_client, finished)`.
        fn accept(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
            use sspi::ServerRequestFlags;
            let mut inb = vec![SecurityBuffer::new(input.to_vec(), BufferType::Token)];
            let mut outb = vec![SecurityBuffer::new(Vec::new(), BufferType::Token)];
            let status = {
                let builder = self
                    .ctx
                    .accept_security_context()
                    .with_credentials_handle(&mut self.creds)
                    .with_context_requirements(
                        ServerRequestFlags::CONFIDENTIALITY | ServerRequestFlags::ALLOCATE_MEMORY,
                    )
                    .with_target_data_representation(DataRepresentation::Native)
                    .with_input(&mut inb)
                    .with_output(&mut outb);
                let r = self
                    .ctx
                    .accept_security_context_impl(builder)
                    .unwrap()
                    .resolve_to_result()
                    .unwrap();
                r.status
            };
            if matches!(
                status,
                SecurityStatus::CompleteNeeded | SecurityStatus::CompleteAndContinue
            ) {
                self.ctx.complete_auth_token(&mut outb).unwrap();
            }
            let fin = matches!(status, SecurityStatus::Ok | SecurityStatus::CompleteNeeded);
            (outb.remove(0).buffer, fin)
        }

        fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
            let mut data = plain.to_vec();
            let mut token = vec![0u8; 16];
            {
                let mut bufs = [
                    SecurityBufferRef::token_buf(&mut token),
                    SecurityBufferRef::data_buf(&mut data),
                ];
                self.ctx
                    .encrypt_message(EncryptionFlags::empty(), &mut bufs, self.send_seq)
                    .unwrap();
            }
            self.send_seq += 1;
            let mut out = (data.len() as u16).to_le_bytes().to_vec();
            out.extend_from_slice(&data);
            out.extend_from_slice(&(token.len() as u16).to_le_bytes());
            out.extend_from_slice(&token);
            out
        }

        fn unseal(&mut self, body: &[u8]) -> Vec<u8> {
            let dl = u16::from_le_bytes([body[0], body[1]]) as usize;
            let mut data = body[2..2 + dl].to_vec();
            let tl = u16::from_le_bytes([body[2 + dl], body[3 + dl]]) as usize;
            let mut token = body[4 + dl..4 + dl + tl].to_vec();
            {
                let mut bufs = [
                    SecurityBufferRef::token_buf(&mut token),
                    SecurityBufferRef::data_buf(&mut data),
                ];
                self.ctx.decrypt_message(&mut bufs, self.recv_seq).unwrap();
            }
            self.recv_seq += 1;
            data
        }
    }

    fn connected_pair() -> (SspiSession, LoopbackServer) {
        let c = LogonCredentials::parse("CORP\\alice", None, "S3cret!pw").unwrap();
        let mut client = SspiSession::new_with_credentials("192.0.2.10", &c).unwrap();
        let mut server = LoopbackServer::new(&c);

        let mut to_client: Vec<u8> = Vec::new();
        for _ in 0..8 {
            let step = client.step(&to_client).unwrap();
            if step.done && step.output.is_empty() {
                break;
            }
            let (out, fin) = server.accept(&step.output);
            to_client = out;
            if step.done && fin {
                break;
            }
        }
        assert!(client.done, "client handshake did not complete");
        (client, server)
    }

    #[test]
    fn loopback_handshake_negotiates_sealing() {
        let (mut client, _server) = connected_pair();
        assert!(client.confidentiality());
        assert!(!client.mutual_auth());
        assert_eq!(client.package_name().as_deref(), Some("NTLM"));
        let sizes = client.message_sizes().unwrap();
        assert_eq!(sizes.cb_security_trailer, 16);
    }

    #[test]
    fn loopback_seal_unseal_many_frames_in_order() {
        let (mut client, mut server) = connected_pair();
        for i in 0..2000u32 {
            let msg = format!("frame {i} \u{e9}\u{4e16}").into_bytes();
            // client -> server
            let sealed = client.seal(&msg).unwrap();
            assert_eq!(
                u16::from_le_bytes([sealed[0], sealed[1]]) as usize,
                msg.len()
            );
            assert_ne!(
                &sealed[2..2 + msg.len()],
                &msg[..],
                "data must be encrypted"
            );
            assert_eq!(server.unseal(&sealed), msg);
            // server -> client
            let reply = format!("reply {i}").into_bytes();
            let sealed = server.seal(&reply);
            assert_eq!(client.unseal(&sealed).unwrap(), reply);
        }
    }

    #[test]
    fn loopback_rejects_out_of_order_and_tampered_frames() {
        let (mut client, mut server) = connected_pair();
        let f0 = server.seal(b"first");
        let f1 = server.seal(b"second");
        // Skipping f0 breaks the sequence: must error, never panic.
        assert!(client.unseal(&f1).is_err());
        // The RC4 stream is now desynchronised; a fresh pair checks tampering.
        let (mut client, mut server) = connected_pair();
        let mut f = server.seal(b"payload");
        let last = f.len() - 1;
        f[last] ^= 0x01; // flip a signature bit
        assert!(client.unseal(&f).is_err());
        let _ = f0;
    }

    #[test]
    fn unseal_rejects_malformed_frames() {
        let (mut client, _server) = connected_pair();
        assert!(client.unseal(&[]).is_err());
        assert!(client.unseal(&[1, 0]).is_err());
        assert!(client.unseal(&[0xff, 0xff, 0, 0]).is_err());
    }

    fn token(ty: u32, flags_off: usize, flags: u32, len: usize) -> Vec<u8> {
        let mut t = vec![0u8; len];
        t[..8].copy_from_slice(NTLMSSP_SIGNATURE);
        t[8..12].copy_from_slice(&ty.to_le_bytes());
        t[flags_off..flags_off + 4].copy_from_slice(&flags.to_le_bytes());
        t
    }

    #[test]
    fn reads_flags_from_ntlm_messages() {
        assert_eq!(
            ntlm_message_flags(&token(2, 20, 0xe2898215, 56)),
            Some((2, 0xe2898215))
        );
        assert_eq!(ntlm_message_flags(&token(3, 60, 0x20, 88)), Some((3, 0x20)));
        assert_eq!(
            ntlm_message_flags(&token(1, 12, 0xe20882b7, 40)),
            Some((1, 0xe20882b7))
        );
        assert_eq!(ntlm_message_flags(b"not ntlm at all"), None);
        assert_eq!(
            ntlm_message_flags(&token(3, 20, 0, 40)),
            None,
            "AUTHENTICATE too short"
        );
    }

    #[test]
    fn sealing_needs_both_sides() {
        assert!(sealing_negotiated(Some(0x20), Some(0x20)));
        assert!(
            !sealing_negotiated(Some(0x00), Some(0x20)),
            "server did not offer SEAL"
        );
        assert!(
            !sealing_negotiated(Some(0x20), Some(0x00)),
            "client did not confirm SEAL"
        );
        assert!(!sealing_negotiated(None, Some(0x20)), "no challenge seen");
        assert!(!sealing_negotiated(Some(0x20), None));
    }

    #[test]
    fn not_confidential_before_handshake() {
        let c = LogonCredentials::parse("CORP\\alice", None, "pw").unwrap();
        let s = SspiSession::new_with_credentials("192.0.2.10", &c).unwrap();
        assert!(!s.confidentiality());
    }

    #[test]
    fn can_create_session() {
        let c = LogonCredentials::parse("CORP\\alice", None, "pw").unwrap();
        assert!(SspiSession::new_with_credentials("192.0.2.10", &c).is_ok());
    }
}
