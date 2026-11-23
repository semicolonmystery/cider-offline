//! Widevine Content Decryption Module: WVD parsing, license negotiation,
//! and content key extraction.

use aes::Aes128;
use base64::prelude::*;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use cmac::Cmac;
use cmac::digest::KeyInit as _;
use hmac::{Hmac, Mac};
use prost::Message;
use rand::Rng;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::{Oaep, PrivateKey};
use sha1::Sha1;
use sha2::Sha256;

use crate::error::DrmError;

// ---------------------------------------------------------------------------
// Protobuf message definitions (minimal subset for Widevine license exchange)
// ---------------------------------------------------------------------------

const MSG_TYPE_LICENSE_REQUEST: i32 = 1;
const MSG_TYPE_LICENSE: i32 = 2;
const MSG_TYPE_ERROR_RESPONSE: i32 = 3;
const MSG_TYPE_SERVICE_CERTIFICATE_REQUEST: i32 = 4;
const MSG_TYPE_SERVICE_CERTIFICATE: i32 = 5;
const REQUEST_TYPE_NEW: i32 = 1;
const PROTOCOL_VERSION_2_1: i32 = 21;
const LICENSE_TYPE_STREAMING: i32 = 1;
const KEY_TYPE_CONTENT: i32 = 2;

/// Pretty-prints `SignedMessage.msg_type` for diagnostic logs.
fn msg_type_name(t: i32) -> &'static str {
    match t {
        MSG_TYPE_LICENSE_REQUEST => "LICENSE_REQUEST",
        MSG_TYPE_LICENSE => "LICENSE",
        MSG_TYPE_ERROR_RESPONSE => "ERROR_RESPONSE",
        MSG_TYPE_SERVICE_CERTIFICATE_REQUEST => "SERVICE_CERTIFICATE_REQUEST",
        MSG_TYPE_SERVICE_CERTIFICATE => "SERVICE_CERTIFICATE",
        _ => "UNKNOWN",
    }
}

/// Widevine system id for PSSH boxes.
const WIDEVINE_SYSTEM_ID: [u8; 16] = [
    0xED, 0xEF, 0x8B, 0xA9, 0x79, 0xD6, 0x4A, 0xCE, 0xA3, 0xC8, 0x27, 0xDC, 0xD5, 0x1D, 0x21, 0xED,
];

#[derive(Clone, PartialEq, Message)]
struct SignedMessage {
    #[prost(int32, optional, tag = "1")]
    msg_type: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    msg: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    signature: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    session_key: Option<Vec<u8>>,
    /// `OEMCrypto` core-message (API v16+). If present, Widevine servers HMAC
    /// over `oemcrypto_core_message || msg` instead of just `msg`, per
    /// pywidevine's `parse_license`.
    #[prost(bytes = "vec", optional, tag = "9")]
    oemcrypto_core_message: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct LicenseRequest {
    /// Raw serialized `ClientIdentification` from the WVD — wire-compatible
    /// with a `message` field (both are length-delimited).
    #[prost(bytes = "vec", optional, tag = "1")]
    client_id: Option<Vec<u8>>,
    #[prost(message, optional, tag = "2")]
    content_id: Option<ContentIdentification>,
    #[prost(int32, optional, tag = "3")]
    request_type: Option<i32>,
    #[prost(uint32, optional, tag = "4")]
    request_time: Option<u32>,
    #[prost(int32, optional, tag = "6")]
    protocol_version: Option<i32>,
    #[prost(uint32, optional, tag = "7")]
    key_control_nonce: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
struct ContentIdentification {
    #[prost(message, optional, tag = "1")]
    widevine_pssh_data: Option<WidevinePsshData>,
}

#[derive(Clone, PartialEq, Message)]
struct WidevinePsshData {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pssh_data: Vec<Vec<u8>>,
    #[prost(int32, optional, tag = "2")]
    license_type: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    request_id: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct WidevineCencHeader {
    #[prost(int32, optional, tag = "1")]
    algorithm: Option<i32>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    key_id: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct License {
    #[prost(message, repeated, tag = "3")]
    key: Vec<KeyContainer>,
}

#[derive(Clone, PartialEq, Message)]
struct KeyContainer {
    #[prost(bytes = "vec", optional, tag = "1")]
    id: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    iv: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    key: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    key_type: Option<i32>,
}

// ---------------------------------------------------------------------------
// WVD (Widevine Device) file parsing
// ---------------------------------------------------------------------------

/// Parsed Widevine device credentials.
struct WvdDevice {
    private_key: PrivateKey,
    /// RSASSA-PSS (SHA-1) signer built from `private_key` once at
    /// parse time, so every license challenge reuses the same instance
    /// instead of cloning the ~2 KiB RSA key on each request.
    signing_key: rsa::pss::SigningKey<Sha1>,
    client_id: Vec<u8>,
}

fn parse_wvd(data: &[u8]) -> Result<WvdDevice, DrmError> {
    if data.len() < 9 || &data[..3] != b"WVD" {
        return Err(DrmError::InvalidWvd("missing WVD magic".into()));
    }
    let pk_len = usize::from(u16::from_be_bytes([data[7], data[8]]));
    if data.len() < 9 + pk_len + 2 {
        return Err(DrmError::InvalidWvd("truncated private key".into()));
    }
    let pk_der = &data[9..9 + pk_len];
    let cid_off = 9 + pk_len;
    let cid_len = usize::from(u16::from_be_bytes([data[cid_off], data[cid_off + 1]]));
    if data.len() < cid_off + 2 + cid_len {
        return Err(DrmError::InvalidWvd("truncated client id".into()));
    }
    let client_id = data[cid_off + 2..cid_off + 2 + cid_len].to_vec();

    let private_key = PrivateKey::from_pkcs1_der(pk_der)
        .map_err(|e| DrmError::InvalidWvd(format!("bad RSA key: {e}")))?;
    let signing_key = rsa::pss::SigningKey::<Sha1>::new(private_key.clone());

    Ok(WvdDevice {
        private_key,
        signing_key,
        client_id,
    })
}

// ---------------------------------------------------------------------------
// CDM (holds device credentials, creates sessions)
// ---------------------------------------------------------------------------

/// Widevine CDM holding device credentials.
pub struct Cdm {
    device: WvdDevice,
}

impl Cdm {
    /// Creates a CDM from raw WVD bytes.
    pub fn from_wvd(wvd_bytes: &[u8]) -> Result<Self, DrmError> {
        Ok(Self {
            device: parse_wvd(wvd_bytes)?,
        })
    }

    /// Creates a CDM from a base64-encoded WVD string.
    pub fn from_wvd_base64(b64: &str) -> Result<Self, DrmError> {
        let bytes = BASE64_STANDARD
            .decode(b64)
            .map_err(|e| DrmError::InvalidWvd(format!("bad base64: {e}")))?;
        Self::from_wvd(&bytes)
    }

    /// Starts a new license session from a Widevine PSSH box, without a
    /// service certificate (the client id goes in cleartext either way). Used
    /// for the enhancedHls (spatial) keys, which come with no
    /// `widevine-cert-url`.
    ///
    /// # Errors
    ///
    /// Returns [`DrmError::InvalidWvd`] if `init_data` is not a parseable
    /// Widevine PSSH box carrying a 16-byte KID (R6: no "whole blob is the
    /// KID" fallback).
    pub fn open_session(&self, init_data: &[u8]) -> Result<CdmSession<'_>, DrmError> {
        let (key_id, pssh_data) = parse_init_data(init_data)?;
        Ok(CdmSession {
            device: &self.device,
            key_id,
            pssh_data,
            service_cert: None,
            license_request_msg: None,
        })
    }

    /// Starts a new license session with a Widevine service certificate
    /// attached. The service cert is accepted and stored, but the client id
    /// is still sent in cleartext, which Apple's Widevine license server
    /// accepts.
    ///
    /// `init_data` must be a Widevine PSSH box; the 16-byte KID is extracted
    /// from its `WidevineCencHeader` payload. Callers that have only a bare
    /// KID should wrap it first via [`build_pssh_box`].
    ///
    /// # Errors
    ///
    /// Returns [`DrmError::InvalidWvd`] if `init_data` is not a parseable
    /// Widevine PSSH box carrying a 16-byte KID.
    pub fn open_session_with_cert(
        &self,
        init_data: &[u8],
        service_cert: Vec<u8>,
    ) -> Result<CdmSession<'_>, DrmError> {
        let (key_id, pssh_data) = parse_init_data(init_data)?;
        Ok(CdmSession {
            device: &self.device,
            key_id,
            pssh_data,
            service_cert: Some(service_cert),
            license_request_msg: None,
        })
    }
}

// ---------------------------------------------------------------------------
// CDM session (one per download)
// ---------------------------------------------------------------------------

/// A single Widevine license exchange.
pub struct CdmSession<'cdm> {
    device: &'cdm WvdDevice,
    key_id: [u8; 16],
    /// The PSSH box's `WidevineCencHeader` payload, verbatim. Sent as the
    /// challenge's `pssh_data`, as Chromium's CDM does: re-encoding it would
    /// drop fields such as enhancedHls' `protection_scheme = 'cbcs'`.
    pssh_data: Vec<u8>,
    #[expect(
        dead_code,
        reason = "reserved for privacy-mode client-id encryption; today the client id goes cleartext (matches Apple's accepted flow) but holding the cert keeps the shape right for the privacy-mode patch"
    )]
    service_cert: Option<Vec<u8>>,
    /// The serialized `LicenseRequest` protobuf bytes sent in the challenge.
    /// Populated by [`create_license_request`] and consumed by
    /// [`parse_license_response`] as the Widevine key-derivation context
    /// (`derive_context(license_request_bytes)` in pywidevine).
    license_request_msg: Option<Vec<u8>>,
}

impl CdmSession<'_> {
    /// Builds the serialized `SignedMessage` containing the license request.
    /// Returns the bytes to send as the challenge, and saves the raw
    /// `LicenseRequest` protobuf bytes for later HMAC-key derivation.
    ///
    /// # Errors
    ///
    /// Returns [`DrmError::License`] on RSASSA-PSS signing failure.
    pub fn create_license_request(&mut self) -> Result<Vec<u8>, DrmError> {
        let mut rng = rand::rng();
        let request_id: [u8; 16] = rng.random();

        #[expect(
            clippy::cast_possible_truncation,
            reason = "Widevine request_time is u32 Unix seconds — overflow is after year 2106"
        )]
        let request_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let license_req = LicenseRequest {
            client_id: Some(self.device.client_id.clone()),
            content_id: Some(ContentIdentification {
                widevine_pssh_data: Some(WidevinePsshData {
                    pssh_data: vec![self.pssh_data.clone()],
                    license_type: Some(LICENSE_TYPE_STREAMING),
                    request_id: Some(request_id.to_vec()),
                }),
            }),
            request_type: Some(REQUEST_TYPE_NEW),
            request_time: Some(request_time),
            protocol_version: Some(PROTOCOL_VERSION_2_1),
            key_control_nonce: Some(rng.random()),
        };

        let msg = license_req.encode_to_vec();

        // `WvdDevice::signing_key` is built once at `parse_wvd`, so every
        // challenge signs with the same ~2 KiB RSA key instance instead of
        // cloning it per request.
        let signature = self
            .device
            .signing_key
            .try_sign(&msg)
            .map_err(|e| DrmError::License(format!("PSS signing failed: {e}")))?;

        let sig_bytes = signature.to_vec();

        tracing::debug!(
            license_request_msg_len = msg.len(),
            signature_len = sig_bytes.len(),
            key_id = %hex::encode(&self.key_id),
            "create_license_request: signed LicenseRequest (PSS-SHA1, salt=20)"
        );

        let signed = SignedMessage {
            msg_type: Some(MSG_TYPE_LICENSE_REQUEST),
            msg: Some(msg.clone()),
            signature: Some(sig_bytes),
            session_key: None,
            oemcrypto_core_message: None,
        };

        // Save the raw LicenseRequest bytes — the Widevine KDF context is the
        // EXACT bytes that were signed, not a re-serialization of the parsed
        // `LicenseRequest`. Re-encoding can reorder prost fields and break the
        // HMAC even when every semantic field matches.
        self.license_request_msg = Some(msg);

        Ok(signed.encode_to_vec())
    }

    /// Parses a license response and returns the content key for our key id.
    ///
    /// Must be called after [`create_license_request`] has stored the raw
    /// `LicenseRequest` bytes used as the Widevine KDF context.
    ///
    /// # Errors
    ///
    /// - [`DrmError::LicenseServerError`] if the response carries
    ///   `SignedMessage.type == ERROR_RESPONSE`.
    /// - [`DrmError::License`] on malformed/unexpected responses, failed RSA
    ///   decryption, HMAC verification failure, or an empty / unauthorized
    ///   license body.
    /// - [`DrmError::KeyNotFound`] if no content-key container matches the
    ///   session's `key_id`.
    pub fn parse_license_response(&self, response_bytes: &[u8]) -> Result<Vec<u8>, DrmError> {
        let signed = SignedMessage::decode(response_bytes)
            .map_err(|e| DrmError::License(format!("bad SignedMessage: {e}")))?;

        let msg_type = signed.msg_type.unwrap_or(-1);
        let msg_len = signed.msg.as_ref().map_or(0, Vec::len);
        let sig_len = signed.signature.as_ref().map_or(0, Vec::len);
        let session_key_len = signed.session_key.as_ref().map_or(0, Vec::len);

        tracing::debug!(
            response_bytes_len = response_bytes.len(),
            msg_type,
            msg_type_name = %msg_type_name(msg_type),
            signed_msg_len = msg_len,
            signed_sig_len = sig_len,
            session_key_present = signed.session_key.is_some(),
            session_key_len,
            key_id = %hex::encode(&self.key_id),
            "parse_license_response: SignedMessage decoded"
        );

        // Classify early so a server-side rejection surfaces as its own error
        // variant rather than a confusing "HMAC verification failed".
        if msg_type == MSG_TYPE_ERROR_RESPONSE {
            let detail = hex::encode(signed.msg.as_deref().unwrap_or_default());
            return Err(DrmError::LicenseServerError(format!(
                "ERROR_RESPONSE SignedMessage (body_hex={})",
                truncate_for_log(&detail, 128)
            )));
        }
        if signed.msg_type != Some(MSG_TYPE_LICENSE) {
            return Err(DrmError::License(format!(
                "unexpected SignedMessage.type: {:?} ({})",
                signed.msg_type,
                msg_type_name(msg_type)
            )));
        }

        let enc_session_key = signed
            .session_key
            .as_deref()
            .ok_or_else(|| DrmError::License("missing session_key".into()))?;
        let msg = signed
            .msg
            .as_deref()
            .ok_or_else(|| DrmError::License("missing msg".into()))?;

        // The KDF context is the LicenseRequest bytes from the CHALLENGE we
        // sent, not the License bytes in the response.
        let license_request_msg = self.license_request_msg.as_deref().ok_or_else(|| {
            DrmError::License(
                "create_license_request must be called before parse_license_response".into(),
            )
        })?;

        // Decrypt session key with RSA-OAEP (SHA1 hash, MGF1-SHA1) — Widevine
        // uses SHA1 here, not SHA256.
        let session_key = self
            .device
            .private_key
            .decrypt(Oaep::<Sha1>::new(), enc_session_key)?;

        // pywidevine's parse_license HMACs over `oemcrypto_core_message || msg`
        // when the field is present (OEMCrypto API v16+). Apple's server
        // typically omits it, but include it when present so the HMAC matches.
        let core_msg = signed.oemcrypto_core_message.as_deref().unwrap_or_default();

        self.verify_and_extract(
            msg,
            sig_of(&signed),
            &session_key,
            license_request_msg,
            core_msg,
        )
    }

    /// RSA-decryption-free portion of license parsing: given the already-
    /// recovered Widevine session key, derive the KDF keys, HMAC-verify the
    /// response body, and pull the content key out. Extracted so unit tests
    /// can exercise the KDF + HMAC + AES-CBC path without paying the RSA
    /// crate's rand-version-mismatch tax in the test harness.
    fn verify_and_extract(
        &self,
        msg: &[u8],
        sig: Option<&[u8]>,
        session_key: &[u8],
        license_request_msg: &[u8],
        oemcrypto_core_message: &[u8],
    ) -> Result<Vec<u8>, DrmError> {
        // Widevine KDF (NIST SP 800-108 counter mode, PRF = AES-CMAC-128):
        //   enc_context    = b"ENCRYPTION\x00"     + license_request_msg + 0x00000080
        //   mac_context    = b"AUTHENTICATION\x00" + license_request_msg + 0x00000200
        //   enc_key        = AES-CMAC(session_key, 0x01 || enc_context)      // 16 bytes
        //   mac_key_server = AES-CMAC(session_key, 0x01 || mac_context)
        //                 || AES-CMAC(session_key, 0x02 || mac_context)      // 32 bytes
        //
        // Matches pywidevine `Cdm.derive_context` + `Cdm.derive_keys` and
        // node-widevine `Session.parseLicense` byte-for-byte. The earlier
        // HMAC-SHA256 KDF produced the "HMAC verification failed" on a
        // well-formed Apple LICENSE response because the server derives
        // mac_key_server with AES-CMAC, not HMAC-SHA256.
        let enc_key = derive_enc_key(session_key, license_request_msg)?;
        let mac_key_server = derive_mac_key_server(session_key, license_request_msg)?;

        // Verify HMAC-SHA256 signature over `oemcrypto_core_message || msg`
        // with mac_key_server. Widevine prepends `oemcrypto_core_message`
        // when present (OEMCrypto API v16+); for Apple it is typically empty.
        //
        // Uses `Mac::verify_slice` for a constant-time compare (idiomatic
        // RustCrypto); a plain `!=` byte compare would not be timing-safe.
        // Diagnostic prefixes are recomputed only on the error path so the
        // happy path pays no extra cost.
        let sig = sig.ok_or_else(|| DrmError::License("missing signature".into()))?;
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&mac_key_server)
            .map_err(|e| DrmError::License(format!("HMAC init: {e}")))?;
        mac.update(oemcrypto_core_message);
        mac.update(msg);

        // Clone the stateful MAC so we can both verify (consumes) and still
        // recover the computed tag for the diagnostic log on failure.
        let verifier = mac.clone();
        if verifier.verify_slice(sig).is_err() {
            let computed = mac.finalize().into_bytes();
            let computed_prefix = hex::encode(&computed[..4.min(computed.len())]);
            let sig_prefix = hex::encode(&sig[..4.min(sig.len())]);
            tracing::warn!(
                hmac_message_len = msg.len(),
                mac_key_server_len = mac_key_server.len(),
                signature_len = sig.len(),
                session_key_decrypted_len = session_key.len(),
                license_request_msg_len = license_request_msg.len(),
                oemcrypto_core_message_len = oemcrypto_core_message.len(),
                computed_hmac_prefix = %computed_prefix,
                response_sig_prefix = %sig_prefix,
                "HMAC verification failed — if lengths match (mac_key=32, sig=32, session_key=16) \
                 and prefixes differ, the response was signed by the server with a different mac_key_server, \
                 which points to a device blob the server considers invalid or revoked rather than a KDF bug"
            );
            return Err(DrmError::License("HMAC verification failed".into()));
        }

        // Parse the license and extract content keys.
        let license =
            License::decode(msg).map_err(|e| DrmError::License(format!("bad License: {e}")))?;

        let kid_hex = hex::encode(&self.key_id);
        for container in &license.key {
            if container.key_type != Some(KEY_TYPE_CONTENT) {
                continue;
            }
            let id = container.id.as_deref().unwrap_or_default();
            if id != self.key_id.as_slice() {
                continue;
            }
            let enc_content_key = container
                .key
                .as_deref()
                .ok_or_else(|| DrmError::License("key container has no key".into()))?;
            let iv = container
                .iv
                .as_deref()
                .ok_or_else(|| DrmError::License("key container has no iv".into()))?;

            return decrypt_content_key(&enc_key, iv, enc_content_key);
        }

        Err(DrmError::KeyNotFound(kid_hex))
    }
}

/// Convenience: borrow the `signature` field of a `SignedMessage` as a slice.
fn sig_of(signed: &SignedMessage) -> Option<&[u8]> {
    signed.signature.as_deref()
}

/// Clips a long string for diagnostic logging. Avoids pulling ~KB of hex into
/// a single log line.
fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_owned()
    } else {
        format!("{}…({} total chars)", &s[..max], s.len())
    }
}

// ---------------------------------------------------------------------------
// Key derivation helpers (Widevine KDF: NIST SP 800-108 counter mode, PRF = AES-CMAC-128)
// ---------------------------------------------------------------------------

/// Widevine key-size suffixes (NIST SP 800-108 counter-mode `L`, big-endian).
const KDF_L_SUFFIX_128: [u8; 4] = [0x00, 0x00, 0x00, 0x80]; // 128 bits
const KDF_L_SUFFIX_512: [u8; 4] = [0x00, 0x00, 0x02, 0x00]; // 512 bits

/// Runs AES-128-CMAC via the `cmac` crate. The returned tag is 16 bytes.
///
/// The `RustCrypto` `cmac` crate (RFC 4493) shares the `cipher` 0.5 family
/// with our `aes` / `cbc` / `ctr` deps and offers constant-time primitives.
fn aes128_cmac(key: &[u8; 16], message: &[u8]) -> [u8; 16] {
    #[expect(
        clippy::expect_used,
        reason = "Cmac::<Aes128>::new_from_slice accepts any 16-byte key; failure is impossible"
    )]
    let mut mac = Cmac::<Aes128>::new_from_slice(key).expect("Cmac::<Aes128> accepts 16-byte keys");
    mac.update(message);
    mac.finalize().into_bytes().into()
}

/// Session key must be exactly 16 bytes for Widevine's AES-CMAC-128 KDF.
fn session_key_as_array(session_key: &[u8]) -> Result<[u8; 16], DrmError> {
    session_key.try_into().map_err(|_| {
        DrmError::License(format!(
            "session_key must be 16 bytes for AES-CMAC KDF, got {}",
            session_key.len()
        ))
    })
}

/// Derives the 16-byte AES-CBC content-key-wrapping key.
/// `enc_key = AES-CMAC(session_key, 0x01 || "ENCRYPTION\x00" || license_request_msg || L(128))`
fn derive_enc_key(session_key: &[u8], license_request_msg: &[u8]) -> Result<[u8; 16], DrmError> {
    let key = session_key_as_array(session_key)?;
    let mut input = Vec::with_capacity(1 + 11 + license_request_msg.len() + 4);
    input.push(0x01);
    input.extend_from_slice(b"ENCRYPTION\x00");
    input.extend_from_slice(license_request_msg);
    input.extend_from_slice(&KDF_L_SUFFIX_128);
    Ok(aes128_cmac(&key, &input))
}

/// Derives the 32-byte server MAC key used to verify the response HMAC.
/// `mac_key_server = CMAC(…, 0x01||ctx) || CMAC(…, 0x02||ctx)` where
/// `ctx = "AUTHENTICATION\x00" || license_request_msg || L(512)`.
fn derive_mac_key_server(
    session_key: &[u8],
    license_request_msg: &[u8],
) -> Result<[u8; 32], DrmError> {
    let key = session_key_as_array(session_key)?;
    // Build context once; the only byte that changes between CMAC inputs
    // is the leading counter.
    let mut input = Vec::with_capacity(1 + 15 + license_request_msg.len() + 4);
    input.push(0x01);
    input.extend_from_slice(b"AUTHENTICATION\x00");
    input.extend_from_slice(license_request_msg);
    input.extend_from_slice(&KDF_L_SUFFIX_512);

    let r1 = aes128_cmac(&key, &input);
    input[0] = 0x02;
    let r2 = aes128_cmac(&key, &input);

    let mut out = [0u8; 32];
    out[..16].copy_from_slice(&r1);
    out[16..].copy_from_slice(&r2);
    Ok(out)
}

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// Decrypts a content key with AES-128-CBC.
fn decrypt_content_key(enc_key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, DrmError> {
    let mut buf = data.to_vec();
    if enc_key.len() != 16 || iv.len() != 16 {
        return Err(DrmError::License(
            "AES-CBC init: invalid key/iv length".into(),
        ));
    }
    let key_arr: [u8; 16] = enc_key
        .try_into()
        .map_err(|_| DrmError::License("AES-CBC init: invalid key length".into()))?;
    let iv_arr: [u8; 16] = iv
        .try_into()
        .map_err(|_| DrmError::License("AES-CBC init: invalid iv length".into()))?;
    let decryptor = Aes128CbcDec::new(&key_arr.into(), &iv_arr.into());
    let decrypted = decryptor
        .decrypt_padded::<cbc::cipher::block_padding::Pkcs7>(&mut buf)
        .map_err(|e| DrmError::License(format!("AES-CBC decrypt: {e}")))?;
    Ok(decrypted.to_vec())
}

/// Extracts the 16-byte Widevine KID from `init_data`.
///
/// `init_data` must be a well-formed Widevine PSSH box; the KID comes from
/// its payload (`WidevineCencHeader.key_id[0]` for v0, or `kid_ids[0]` for
/// v1). Callers that have only a bare 16-byte KID must wrap it first via
/// [`build_pssh_box`]. There is no "use the whole blob" fallback: it would
/// defer malformed-init-data errors to a confusing `KeyNotFound` much later
/// in the pipeline.
///
/// # Errors
///
/// Returns [`DrmError::InvalidWvd`] if `init_data` is not a parseable
/// Widevine PSSH box or its payload carries no KID of the expected 16-byte
/// length.
#[cfg(test)]
fn extract_kid_from_init_data(init_data: &[u8]) -> Result<[u8; 16], DrmError> {
    parse_init_data(init_data).map(|(kid, _)| kid)
}

/// Splits a Widevine PSSH box into its 16-byte KID and its verbatim
/// `WidevineCencHeader` payload.
///
/// # Errors
///
/// [`DrmError::InvalidWvd`] if `init_data` is not a parseable Widevine PSSH
/// box or its KID is not 16 bytes.
fn parse_init_data(init_data: &[u8]) -> Result<([u8; 16], Vec<u8>), DrmError> {
    let (kid, payload) = parse_widevine_pssh(init_data).ok_or_else(|| {
        DrmError::InvalidWvd(
            "init_data is not a parseable Widevine PSSH box: wrap bare KIDs via build_pssh_box \
             before opening a session"
                .into(),
        )
    })?;
    let kid = <[u8; 16]>::try_from(kid.as_slice()).map_err(|_| {
        DrmError::InvalidWvd(format!(
            "Widevine PSSH key_id must be 16 bytes, got {}",
            kid.len()
        ))
    })?;
    Ok((kid, payload.to_vec()))
}

/// Parses `data` as a Widevine PSSH box and returns its first KID and the
/// raw `WidevineCencHeader` payload. Returns `None` if `data` is not a
/// well-formed Widevine PSSH box or carries no KID.
fn parse_widevine_pssh(data: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    // Minimum v0 PSSH box: 4 size + 4 "pssh" + 1 version + 3 flags + 16 sys_id + 4 data_size = 32
    if data.len() < 32 || &data[4..8] != b"pssh" {
        return None;
    }
    let version = data[8];
    if data[12..28] != WIDEVINE_SYSTEM_ID {
        return None;
    }
    let mut off = 28_usize;
    if version == 1 {
        // v1: kid_count (u32 BE) + N * 16-byte KID, then data_size + data.
        let kid_count = u32::from_be_bytes(data.get(off..off + 4)?.try_into().ok()?) as usize;
        off = off.checked_add(4)?;
        let v1_kid = if kid_count > 0 {
            data.get(off..off.checked_add(16)?).map(<[u8]>::to_vec)
        } else {
            None
        };
        off = off.checked_add(kid_count.checked_mul(16)?)?;
        if let Some(kid) = v1_kid {
            let payload = pssh_payload(data, off).unwrap_or_default();
            return Some((kid, payload));
        }
    }
    let payload = pssh_payload(data, off)?;
    let header = WidevineCencHeader::decode(payload).ok()?;
    Some((header.key_id.into_iter().next()?, payload))
}

/// The `data_size`-prefixed payload of a PSSH box whose `data_size` field
/// starts at `off`.
fn pssh_payload(data: &[u8], off: usize) -> Option<&[u8]> {
    let size_end = off.checked_add(4)?;
    let data_size = u32::from_be_bytes(data.get(off..size_end)?.try_into().ok()?) as usize;
    data.get(size_end..size_end.checked_add(data_size)?)
}

/// Builds a full PSSH box for the given key id (used in Apple API calls).
#[expect(
    clippy::cast_possible_truncation,
    reason = "PSSH init-data payloads are tens of bytes; u32 overflow is impossible"
)]
pub fn build_pssh_box(key_id: &[u8]) -> Vec<u8> {
    let init_data = WidevineCencHeader {
        algorithm: Some(1),
        key_id: vec![key_id.to_vec()],
    }
    .encode_to_vec();

    let box_size: u32 = 32 + init_data.len() as u32;
    let mut pssh = Vec::with_capacity(box_size as usize);
    pssh.extend_from_slice(&box_size.to_be_bytes());
    pssh.extend_from_slice(b"pssh");
    pssh.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0
    pssh.extend_from_slice(&WIDEVINE_SYSTEM_ID);
    pssh.extend_from_slice(&(init_data.len() as u32).to_be_bytes());
    pssh.extend_from_slice(&init_data);
    pssh
}

// Tiny hex helper to avoid pulling in the `hex` crate for one function.
mod hex {
    use std::fmt::Write;

    pub fn encode(data: &[u8]) -> String {
        data.iter()
            .fold(String::with_capacity(data.len() * 2), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            })
    }

    #[cfg(test)]
    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        if !s.len().is_multiple_of(2) {
            return Err("odd-length hex string".into());
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect()
    }
}

pub(crate) use hex::encode as hex_encode;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pssh_box_should_have_correct_structure() {
        let kid = [0u8; 16];
        let pssh = build_pssh_box(&kid);
        assert_eq!(&pssh[4..8], b"pssh");
        assert_eq!(&pssh[12..28], &WIDEVINE_SYSTEM_ID);
        let size = u32::from_be_bytes([pssh[0], pssh[1], pssh[2], pssh[3]]);
        #[expect(
            clippy::cast_possible_truncation,
            reason = "test builds a tiny box whose length comfortably fits u32"
        )]
        let expected = pssh.len() as u32;
        assert_eq!(size, expected);
    }

    #[test]
    fn hex_roundtrip() {
        let data = b"\xde\xad\xbe\xef";
        assert_eq!(hex::encode(data), "deadbeef");
        assert_eq!(hex::decode("deadbeef").unwrap(), data);
    }

    #[test]
    fn derive_enc_key_should_be_16_bytes() {
        let key = derive_enc_key(&[0u8; 16], b"request-msg").unwrap();
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn derive_mac_key_server_should_be_32_bytes() {
        let key = derive_mac_key_server(&[0u8; 16], b"request-msg").unwrap();
        assert_eq!(
            key.len(),
            32,
            "mac_key_server is two concatenated 16-byte AES-CMAC outputs"
        );
    }

    #[test]
    fn session_key_must_be_16_bytes_for_aes_cmac_kdf() {
        let err = derive_enc_key(b"too-short", b"req").unwrap_err();
        assert!(matches!(err, DrmError::License(ref m) if m.contains("16 bytes")));
    }

    /// RFC 4493 AES-128-CMAC test vectors. Pinning these catches any
    /// regression of the CMAC primitive the Widevine KDF is built on.
    #[test]
    fn aes128_cmac_matches_rfc_4493_test_vectors() {
        let k = {
            let hex_key = "2b7e151628aed2a6abf7158809cf4f3c";
            let mut arr = [0u8; 16];
            arr.copy_from_slice(&hex::decode(hex_key).unwrap());
            arr
        };

        // Example 1: empty message.
        assert_eq!(
            aes128_cmac(&k, &[]),
            hex_slice("bb1d6929e95937287fa37d129b756746"),
            "RFC 4493 example 1 (empty)"
        );

        // Example 2: 16-byte message (one full block, no padding).
        let m16 = hex::decode("6bc1bee22e409f96e93d7e117393172a").unwrap();
        assert_eq!(
            aes128_cmac(&k, &m16),
            hex_slice("070a16b46b4d4144f79bdd9dd04a287c"),
            "RFC 4493 example 2 (16 bytes)"
        );

        // Example 3: 40-byte message (partial last block).
        let m40 = hex::decode(
            "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411",
        )
        .unwrap();
        assert_eq!(
            aes128_cmac(&k, &m40),
            hex_slice("dfa66747de9ae63030ca32611497c827"),
            "RFC 4493 example 3 (40 bytes)"
        );

        // Example 4: 64-byte message (four full blocks).
        let m64 = hex::decode(
            "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411\
             e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710",
        )
        .unwrap();
        assert_eq!(
            aes128_cmac(&k, &m64),
            hex_slice("51f0bebf7e3b9d92fc49741779363cfe"),
            "RFC 4493 example 4 (64 bytes)"
        );
    }

    fn hex_slice(s: &str) -> [u8; 16] {
        let v = hex::decode(s).unwrap();
        let mut a = [0u8; 16];
        a.copy_from_slice(&v);
        a
    }

    /// Protects the Widevine KDF context layout: counter prefix, context
    /// string + NUL separator, license-request bytes, big-endian L suffix.
    /// Pins `mac_key_server` to the AES-CMAC-derived value (not HMAC-SHA256,
    /// which is what shipped before this fix and triggered the field HMAC
    /// mismatch).
    #[test]
    fn mac_key_server_matches_widevine_aes_cmac_kdf() {
        let session_key = [0u8; 16];
        let request_msg = b"ABCD";
        let key = derive_mac_key_server(&session_key, request_msg).unwrap();

        // Reproduce the derivation independently so the test documents and
        // double-checks the exact byte layout.
        let mut input = Vec::new();
        input.push(0x01);
        input.extend_from_slice(b"AUTHENTICATION\x00");
        input.extend_from_slice(request_msg);
        input.extend_from_slice(&[0x00, 0x00, 0x02, 0x00]); // L = 512 bits
        let r1 = aes128_cmac(&session_key, &input);
        input[0] = 0x02;
        let r2 = aes128_cmac(&session_key, &input);

        assert_eq!(&key[..16], &r1, "mac_key_server first half  (counter=1)");
        assert_eq!(&key[16..], &r2, "mac_key_server second half (counter=2)");
    }

    #[test]
    fn extract_kid_should_recover_kid_from_v0_pssh_box() {
        let kid = *b"0123456789abcdef";
        let pssh = build_pssh_box(&kid);
        let recovered = extract_kid_from_init_data(&pssh).unwrap();
        assert_eq!(recovered, kid);
    }

    /// R6: a bare 16-byte blob that happens to be exactly KID-sized is NOT
    /// a PSSH box and must be rejected at the point of use — not quietly
    /// accepted only to resurface as a `KeyNotFound` much later. Callers
    /// with a bare KID are expected to wrap it via [`build_pssh_box`] first.
    #[test]
    fn extract_kid_should_reject_bare_bytes_without_pssh_header() {
        let bare = *b"abcdefghijklmnop";
        let err = extract_kid_from_init_data(&bare).unwrap_err();
        assert!(matches!(err, DrmError::InvalidWvd(_)));
    }

    #[test]
    fn open_session_should_build_challenge_from_pssh_wrapped_kid() {
        let cdm = Cdm::from_wvd(crate::DEFAULT_WVD_BYTES).unwrap();
        let kid = *b"xxxxyyyyzzzzwwww";
        let pssh = build_pssh_box(&kid);
        let session = cdm.open_session(&pssh).unwrap();
        // The session must have extracted the 16-byte KID from the PSSH box.
        assert_eq!(session.key_id, kid);
    }

    /// The `s1/e1` PSSH of an enhancedHls (spatial) playlist: a v0 box whose
    /// payload is `key_id` plus `protection_scheme = 'cbcs'`, no `algorithm`.
    const SPATIAL_PSSH_B64: &str =
        "AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgSEAAAAAAAAAAAczEvZTEgICBI88aJmwY=";

    #[test]
    fn challenge_should_carry_the_pssh_payload_verbatim() {
        let pssh = BASE64_STANDARD.decode(SPATIAL_PSSH_B64).unwrap();
        let payload = pssh[32..].to_vec();
        let cdm = Cdm::from_wvd(crate::DEFAULT_WVD_BYTES).unwrap();
        let mut session = cdm.open_session(&pssh).unwrap();
        let challenge = session.create_license_request().unwrap();

        let signed = SignedMessage::decode(challenge.as_slice()).unwrap();
        let request = LicenseRequest::decode(signed.msg.unwrap().as_slice()).unwrap();
        let sent = request
            .content_id
            .and_then(|c| c.widevine_pssh_data)
            .map(|d| d.pssh_data)
            .unwrap();
        assert_eq!(sent, vec![payload]);
    }

    #[test]
    fn spatial_pssh_should_yield_the_s1_e1_key_id() {
        let pssh = BASE64_STANDARD.decode(SPATIAL_PSSH_B64).unwrap();
        let kid = extract_kid_from_init_data(&pssh).unwrap();
        assert_eq!(&kid, b"\0\0\0\0\0\0\0\0s1/e1   ");
    }

    #[test]
    fn wrapped_bare_kid_payload_should_match_the_old_rebuilt_header() {
        let kid = *b"0123456789abcdef";
        let rebuilt = WidevineCencHeader {
            algorithm: Some(1),
            key_id: vec![kid.to_vec()],
        }
        .encode_to_vec();
        let (_, payload) = parse_init_data(&build_pssh_box(&kid)).unwrap();
        assert_eq!(payload, rebuilt);
    }

    /// End-to-end proof that the KDF + HMAC + content-key-unwrap path is
    /// correct: build a `License` and its `mac_key_server` signature using
    /// the SAME derivation the real license server runs
    /// (`derive_context(license_request_bytes)` + `mac_key_server`), hand it
    /// to `verify_and_extract` with a FIXED session key, and confirm the
    /// content key comes back intact.
    ///
    /// Regressions in any of the three KDF bugs — wrong context message,
    /// wrong `L` suffix, wrong truncation — HMAC-reject this test response.
    /// The RSA-OAEP step is covered by live use (and unchanged in this fix),
    /// so we don't re-run it in a unit test and avoid a cross-version-`rand`
    /// trip on `oaep_encrypt_with_rng`.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one end-to-end proof reads better than fragments: KDF + HMAC + unwrap + oemcrypto"
    )]
    fn verify_and_extract_round_trip_proves_hmac_path() {
        use cbc::cipher::BlockModeEncrypt;
        use rand::Rng;
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

        let cdm = Cdm::from_wvd(crate::DEFAULT_WVD_BYTES).unwrap();
        let kid = *b"\x39\x31\xc8\xcd\x00\x1d\x7f\x8f\x17\x90\x01\xed\xaa\xbb\xcc\xdd";
        let pssh = build_pssh_box(&kid);
        let mut session = cdm.open_session(&pssh).unwrap();

        // 1. Produce a real challenge so the session caches the request bytes.
        let _challenge = session.create_license_request().unwrap();
        let license_request_msg = session.license_request_msg.as_deref().unwrap().to_vec();

        // 2. Fix a plausible 16-byte Widevine session key.
        let session_key: [u8; 16] = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
            0xFF, 0x00,
        ];

        // 3. Derive the server's enc / mac_server keys using the FIXED KDF —
        //    over the license-request bytes the CDM just cached.
        let enc_key = derive_enc_key(&session_key, &license_request_msg).unwrap();
        let mac_key_server = derive_mac_key_server(&session_key, &license_request_msg).unwrap();

        // 4. Build an AES-128-CBC-wrapped content key for our KID.
        //    cbc 0.2 `encrypt_padded(buf, msg_len)` reads the plaintext from
        //    `buf[..msg_len]` and overwrites `buf` with the ciphertext in
        //    place, returning a slice of the final ciphertext.
        let mut rng = rand::rng();
        let plaintext_content_key: [u8; 16] = rng.random();
        let iv: [u8; 16] = rng.random();
        let encryptor = Aes128CbcEnc::new(&enc_key.into(), &iv.into());
        let mut buf = vec![0u8; 32]; // 16 bytes plaintext + up to 16 bytes padding
        buf[..16].copy_from_slice(&plaintext_content_key);
        let wrapped_len = encryptor
            .encrypt_padded::<cbc::cipher::block_padding::Pkcs7>(&mut buf, 16)
            .unwrap()
            .len();
        buf.truncate(wrapped_len);

        // 5. Encode the License protobuf — this is what the server would put
        //    in SignedMessage.msg and HMAC over.
        let license = License {
            key: vec![KeyContainer {
                id: Some(kid.to_vec()),
                iv: Some(iv.to_vec()),
                key: Some(buf),
                key_type: Some(KEY_TYPE_CONTENT),
            }],
        };
        let license_bytes = license.encode_to_vec();

        // 6. HMAC-SHA256 over the License bytes with mac_key_server.
        let signature = {
            let mut mac = Hmac::<Sha256>::new_from_slice(&mac_key_server).unwrap();
            mac.update(&license_bytes);
            mac.finalize().into_bytes().to_vec()
        };

        // 7. Exercise the full KDF+HMAC+unwrap path.
        let recovered = session
            .verify_and_extract(
                &license_bytes,
                Some(&signature),
                &session_key,
                &license_request_msg,
                &[],
            )
            .unwrap();
        assert_eq!(
            recovered,
            plaintext_content_key.to_vec(),
            "CDM did not recover the content key — HMAC/KDF path is broken"
        );

        // 8. A one-byte tweak of the signature MUST now be rejected by the
        //    same code path, proving the HMAC check is live (not ignored).
        let mut bad_sig = signature.clone();
        bad_sig[0] ^= 0x01;
        let err = session
            .verify_and_extract(
                &license_bytes,
                Some(&bad_sig),
                &session_key,
                &license_request_msg,
                &[],
            )
            .unwrap_err();
        match err {
            DrmError::License(msg) => assert!(msg.contains("HMAC")),
            other => panic!("expected HMAC failure, got {other:?}"),
        }

        // 9. A one-byte tweak of the KDF context (the license_request_msg)
        //    must ALSO fail HMAC, proving the server uses the challenge
        //    bytes as KDF context (not the response's License bytes).
        let mut tweaked_req = license_request_msg.clone();
        tweaked_req[0] ^= 0x01;
        let err = session
            .verify_and_extract(
                &license_bytes,
                Some(&signature),
                &session_key,
                &tweaked_req,
                &[],
            )
            .unwrap_err();
        assert!(
            matches!(err, DrmError::License(ref msg) if msg.contains("HMAC")),
            "expected HMAC failure on perturbed license_request_msg, got {err:?}"
        );

        // 10. When oemcrypto_core_message is present on the SignedMessage,
        //     the HMAC input is `core || msg`. A mismatch between what the
        //     server used and what we use must fail HMAC.
        let core_msg = b"oem-core-bytes";
        let sig_with_core = {
            let mac_key_server = derive_mac_key_server(&session_key, &license_request_msg).unwrap();
            let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&mac_key_server).unwrap();
            mac.update(core_msg);
            mac.update(&license_bytes);
            mac.finalize().into_bytes().to_vec()
        };
        let recovered = session
            .verify_and_extract(
                &license_bytes,
                Some(&sig_with_core),
                &session_key,
                &license_request_msg,
                core_msg,
            )
            .unwrap();
        assert_eq!(
            recovered,
            plaintext_content_key.to_vec(),
            "verify_and_extract must HMAC over `oemcrypto_core_message || msg` when present"
        );
    }

    /// `SignedMessage.type == ERROR_RESPONSE` must surface as
    /// [`DrmError::LicenseServerError`], not as a misleading
    /// `HMAC verification failed`. This classifies cause (A) vs (B) on a
    /// live run.
    #[test]
    fn parse_license_response_classifies_error_response() {
        let cdm = Cdm::from_wvd(crate::DEFAULT_WVD_BYTES).unwrap();
        let kid = [0u8; 16];
        let pssh = build_pssh_box(&kid);
        let mut session = cdm.open_session(&pssh).unwrap();
        let _challenge = session.create_license_request().unwrap();

        // Encode a SignedMessage with type=ERROR_RESPONSE. A body of any
        // bytes is fine — the point is the dispatch.
        let response = SignedMessage {
            msg_type: Some(MSG_TYPE_ERROR_RESPONSE),
            msg: Some(b"\xde\xad\xbe\xef".to_vec()),
            signature: None,
            session_key: None,
            oemcrypto_core_message: None,
        };
        let bytes = response.encode_to_vec();
        let err = session.parse_license_response(&bytes).unwrap_err();
        assert!(
            matches!(err, DrmError::LicenseServerError(_)),
            "expected LicenseServerError, got {err:?}"
        );
    }

    // --- Widevine Client-Identification / Device-Certificate protos ------
    //
    // Minimal subset used to verify, in the test below, that the private key
    // parsed out of the WVD actually corresponds to the public key embedded
    // in the `ClientIdentification.token.drm_certificate` that we ship to the
    // server verbatim in every license challenge.
    //
    // Field numbers come from the Widevine license protocol as reproduced in
    // pywidevine's `license_protocol.proto`. Only the fields on the path
    // from `client_id` → `public_key` are declared — every other field is
    // skipped by prost.

    #[derive(Clone, PartialEq, Message)]
    struct ClientIdentification {
        /// `SignedDrmCertificate` bytes. Declared as `bytes` so the test
        /// can decode the inner message explicitly on a well-defined boundary.
        #[prost(bytes = "vec", optional, tag = "2")]
        token: Option<Vec<u8>>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct SignedDrmCertificate {
        /// `DrmCertificate` bytes.
        #[prost(bytes = "vec", optional, tag = "1")]
        drm_certificate: Option<Vec<u8>>,
    }

    #[derive(Clone, PartialEq, Message)]
    struct DrmCertificate {
        /// DER-encoded RSA public key (`SubjectPublicKeyInfo` or PKCS#1).
        /// Format-agnostic for our purposes: both embed the modulus `n`
        /// as a big-endian `INTEGER` whose minimal bytes equal the
        /// private key's `n.to_be_bytes_trimmed_vartime()`.
        #[prost(bytes = "vec", optional, tag = "4")]
        public_key: Option<Vec<u8>>,
    }

    /// Proves that the RSA private key parsed from `default_device.wvd`
    /// corresponds to the public key inside its own
    /// `ClientIdentification.token.drm_certificate`. If these diverge the
    /// server would encrypt the Widevine session key under a public key we
    /// cannot decrypt with, yielding 16 bytes of garbage and the observed
    /// HMAC-verification failure — this test guards against a parser offset
    /// regression or a mis-assembled device blob.
    #[test]
    fn wvd_private_key_matches_client_id_certificate_public_key() {
        use rsa::traits::PublicKeyParts;

        let cdm = Cdm::from_wvd(crate::DEFAULT_WVD_BYTES).unwrap();

        // Decode the nested protos stored in the WVD's `client_id` blob.
        let client_id = ClientIdentification::decode(cdm.device.client_id.as_slice())
            .expect("parse ClientIdentification from WVD");
        let signed_cert_bytes = client_id
            .token
            .expect("ClientIdentification.token present in WVD");
        let signed_cert = SignedDrmCertificate::decode(signed_cert_bytes.as_slice())
            .expect("parse SignedDrmCertificate from ClientIdentification.token");
        let drm_cert_bytes = signed_cert
            .drm_certificate
            .expect("SignedDrmCertificate.drm_certificate present");
        let drm_cert = DrmCertificate::decode(drm_cert_bytes.as_slice())
            .expect("parse DrmCertificate from SignedDrmCertificate.drm_certificate");
        let public_key_der = drm_cert
            .public_key
            .expect("DrmCertificate.public_key present");

        // The WVD private key's modulus `n`, minimal big-endian bytes.
        let n_box = PublicKeyParts::n(&cdm.device.private_key);
        let n_bytes = n_box.to_be_bytes_trimmed_vartime();

        // Both PKCS#1 `RSAPublicKey` and SubjectPublicKeyInfo DER embed the
        // modulus verbatim as a big-endian `INTEGER` — leading 0x00 padding
        // (for the MSB-set case common to 2048-bit RSA) prefixes but does
        // not alter the 256 content bytes. So the minimal modulus must
        // appear as a contiguous window inside the cert's public_key DER.
        assert!(
            public_key_der
                .windows(n_bytes.len())
                .any(|w| w == &n_bytes[..]),
            "WVD private key modulus is NOT embedded in the ClientIdentification \
             certificate's public_key DER — WVD parser offsets are off, or the \
             bundled device blob mixes a private key and a cert from different \
             provisionings (private_key_n_len={}, public_key_der_len={})",
            n_bytes.len(),
            public_key_der.len()
        );
    }

    /// A caller that forgets to run `create_license_request` first must get a
    /// loud, specific error rather than a plausible-looking HMAC mismatch.
    #[test]
    fn parse_license_response_requires_prior_challenge() {
        let cdm = Cdm::from_wvd(crate::DEFAULT_WVD_BYTES).unwrap();
        let kid = [0u8; 16];
        let pssh = build_pssh_box(&kid);
        let session = cdm.open_session(&pssh).unwrap();

        let response = SignedMessage {
            msg_type: Some(MSG_TYPE_LICENSE),
            msg: Some(b"ignored".to_vec()),
            signature: Some(vec![0u8; 32]),
            session_key: Some(vec![0u8; 256]),
            oemcrypto_core_message: None,
        };
        let bytes = response.encode_to_vec();
        let err = session.parse_license_response(&bytes).unwrap_err();
        match err {
            DrmError::License(msg) => assert!(msg.contains("create_license_request")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
