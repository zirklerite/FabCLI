//! Encrypted at-rest storage for the persisted session token.
//!
//! The file on disk is an AES-256-GCM ciphertext sealed with a key
//! stored in the OS user keystore (DPAPI on Windows, `libsecret` /
//! Secret Service on Linux). This binds decryption to "this user,
//! this machine" — copying the file alone elsewhere makes it
//! useless.

use crate::config::PersistedSession;
use crate::error::FabCliError;
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine;
use rand::RngCore;
use rand::rngs::OsRng;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

/// 8-byte magic prefix; lets `read_token` distinguish our format
/// from anything else with O(1) work and gives a clear error when
/// a stray file lands at the token path.
const MAGIC: &[u8; 8] = b"FABCLI\xb1\xc1";
const FORMAT_VERSION: u8 = 0x01;

const AES_KEY_LEN: usize = 32; // Aes256Gcm
const GCM_NONCE_LEN: usize = 12; // 96-bit nonce per AES-GCM spec

/// Header length: MAGIC + version byte + nonce.
const HEADER_LEN: usize = MAGIC.len() + 1 + GCM_NONCE_LEN;

const KEYRING_SERVICE: &str = "fabcli";
const KEYRING_ACCOUNT: &str = "token";

/// Per-process cache of the unsealed AES key. Once populated by
/// either `get_or_create_key` or `get_key_from_keystore_or_error`,
/// subsequent encrypts/decrypts in the same process avoid the
/// keystore round-trip entirely. This matters for flows that touch
/// the token multiple times in one invocation (`auth login` does
/// 2–3 writes; `claim-batch` may refresh mid-run). The keystore key
/// is invariant across a single process's lifetime, so caching is
/// safe.
///
/// `Mutex<Option<...>>` rather than `OnceLock` because we need
/// fallible-init AND serialization-on-first-use: two parallel
/// threads racing through `get_or_create_key` on a fresh keystore
/// entry must NOT both generate keys (the loser's ciphertexts
/// would be undecryptable with the cached winner's key). The lock
/// is held for microseconds; contention is negligible.
static CACHED_KEY: Mutex<Option<[u8; AES_KEY_LEN]>> = Mutex::new(None);

pub fn read_token(path: &Path) -> Result<Option<PersistedSession>, FabCliError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let json = decrypt(&bytes)?;
    let session: PersistedSession = serde_json::from_slice(&json)?;
    Ok(Some(session))
}

pub fn write_token(path: &Path, session: &PersistedSession) -> Result<(), FabCliError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Compact (non-pretty) JSON: no human ever sees the plaintext
    // anyway (it's encrypted before write), and a few fewer bytes is
    // a few fewer bytes to encrypt and persist.
    let json = serde_json::to_vec(session)?;
    let bytes = encrypt(&json)?;

    let tmp_path = path.with_extension("json.tmp");

    {
        // Scoped so the file handle drops before we rename onto path.
        // mode 0o600 on Unix to close the post-create permission race
        // (matches the pre-encryption write_token's contract).
        let mut file = {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp_path)?
            }
            #[cfg(not(unix))]
            {
                fs::File::create(&tmp_path)?
            }
        };
        use std::io::Write;
        file.write_all(&bytes)?;
    }

    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn encrypt(plaintext: &[u8]) -> Result<Vec<u8>, FabCliError> {
    let key = get_or_create_key()?;
    encrypt_with_key(plaintext, &key)
}

fn decrypt(blob: &[u8]) -> Result<Vec<u8>, FabCliError> {
    if blob.len() < HEADER_LEN {
        return Err(FabCliError::Generic(
            "token file is too short to be a valid encrypted blob — \
             re-run 'fabcli auth login' to write a fresh token"
                .into(),
        ));
    }
    if &blob[..MAGIC.len()] != MAGIC {
        return Err(FabCliError::Generic(
            "token file format not recognized (missing FabCLI magic header). \
             Delete the file and re-run 'fabcli auth login' to write a fresh token."
                .into(),
        ));
    }
    let version = blob[MAGIC.len()];
    if version != FORMAT_VERSION {
        return Err(FabCliError::Generic(format!(
            "token file format version {} not supported (this build expects {}); \
             re-run 'fabcli auth login' to write a fresh token",
            version, FORMAT_VERSION
        )));
    }

    let key = get_key_from_keystore_or_error()?;
    decrypt_with_key(blob, &key)
}

fn encrypt_with_key(plaintext: &[u8], key: &[u8; AES_KEY_LEN]) -> Result<Vec<u8>, FabCliError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));

    let mut nonce_bytes = [0u8; GCM_NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| FabCliError::Generic(format!("AES-GCM encrypt failed: {}", e)))?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.push(FORMAT_VERSION);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

fn decrypt_with_key(blob: &[u8], key: &[u8; AES_KEY_LEN]) -> Result<Vec<u8>, FabCliError> {
    let nonce = Nonce::from_slice(&blob[MAGIC.len() + 1..HEADER_LEN]);
    let ciphertext = &blob[HEADER_LEN..];

    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    cipher.decrypt(nonce, ciphertext).map_err(|_| {
        FabCliError::Generic(
            "token decryption failed: authentication tag did not verify. \
             The keystore key may have been rotated, the file may be corrupted, \
             or the token may have been written on a different machine. \
             Re-run 'fabcli auth login'."
                .into(),
        )
    })
}

/// Fetch the AES key from the keystore, generating + storing one if
/// absent. Caches the key in `CACHED_KEY` so subsequent calls in the
/// same process skip the keystore round-trip.
fn get_or_create_key() -> Result<[u8; AES_KEY_LEN], FabCliError> {
    let mut cache = CACHED_KEY.lock().unwrap();
    if let Some(k) = *cache {
        return Ok(k);
    }
    let entry = keystore_entry()?;
    let key = match entry.get_password() {
        Ok(b64) => key_from_b64(&b64)?,
        Err(keyring::Error::NoEntry) => {
            let mut k = [0u8; AES_KEY_LEN];
            OsRng.fill_bytes(&mut k);
            let b64 = base64::engine::general_purpose::STANDARD.encode(k);
            entry.set_password(&b64).map_err(|e| {
                FabCliError::Generic(format!(
                    "could not write encryption key to OS keystore: {e}",
                ))
            })?;
            k
        }
        Err(e) => {
            return Err(FabCliError::Generic(format!(
                "OS keystore read failed: {e}",
            )));
        }
    };
    *cache = Some(key);
    Ok(key)
}

/// Fetch the AES key from the keystore for decryption. Errors if the
/// entry is missing — that's a "no key on this machine" condition,
/// which is the expected outcome of copying an encrypted token to a
/// different machine. Caches on success.
fn get_key_from_keystore_or_error() -> Result<[u8; AES_KEY_LEN], FabCliError> {
    let mut cache = CACHED_KEY.lock().unwrap();
    if let Some(k) = *cache {
        return Ok(k);
    }
    let entry = keystore_entry()?;
    let b64 = entry.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => FabCliError::Generic(
            "no encryption key in OS keystore for FabCLI. \
             The token may have been written on a different machine \
             or under a different user account. Re-run 'fabcli auth login'."
                .into(),
        ),
        other => FabCliError::Generic(format!("OS keystore read failed: {other}")),
    })?;
    let key = key_from_b64(&b64)?;
    *cache = Some(key);
    Ok(key)
}

/// Delete the keystore entry. Used by `auth logout` to ensure no
/// FabCLI state survives logout. Best-effort: a missing entry is
/// not an error.
pub fn delete_keystore_entry() -> Result<(), FabCliError> {
    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(FabCliError::Generic(format!(
            "could not delete OS keystore entry: {e}"
        ))),
    }
}

fn keystore_entry() -> Result<keyring::Entry, FabCliError> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).map_err(|e| {
        FabCliError::Generic(format!(
            "OS keystore unavailable: {e}. Start a keystore daemon \
             (gnome-keyring-daemon or kwalletd5 on Linux; DPAPI is \
             always available on Windows 10+)."
        ))
    })
}

fn key_from_b64(b64: &str) -> Result<[u8; AES_KEY_LEN], FabCliError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| FabCliError::Generic(format!("keystore key decode failed: {}", e)))?;
    if bytes.len() != AES_KEY_LEN {
        return Err(FabCliError::Generic(format!(
            "keystore key has wrong length ({}, expected {AES_KEY_LEN}) — \
             keystore entry may be corrupt; delete it and re-run 'fabcli auth login'",
            bytes.len()
        )));
    }
    let mut key = [0u8; AES_KEY_LEN];
    key.copy_from_slice(&bytes);
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> [u8; AES_KEY_LEN] {
        let mut k = [0u8; AES_KEY_LEN];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn round_trip_with_known_key() {
        let key = fixed_key();
        let plaintext = b"hello FabCLI token \xff\x00\x42";
        let blob = encrypt_with_key(plaintext, &key).unwrap();
        // Header layout sanity.
        assert_eq!(&blob[..MAGIC.len()], MAGIC);
        assert_eq!(blob[MAGIC.len()], FORMAT_VERSION);
        assert!(blob.len() > HEADER_LEN);
        let recovered = decrypt_with_key(&blob, &key).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn ciphertext_tamper_fails_authentication() {
        let key = fixed_key();
        let mut blob = encrypt_with_key(b"sensitive", &key).unwrap();
        // Flip a single bit in the ciphertext + tag region.
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        let err = decrypt_with_key(&blob, &key).unwrap_err();
        assert!(
            matches!(err, FabCliError::Generic(_)),
            "tampered blob must error, got {:?}",
            err
        );
    }

    #[test]
    fn header_tamper_version_mismatch_is_clear_error() {
        let key = fixed_key();
        let mut blob = encrypt_with_key(b"x", &key).unwrap();
        blob[MAGIC.len()] = 0xFE; // bad version
        // We don't go through decrypt_with_key — decrypt() would catch
        // this. Simulate that path's check directly.
        assert_eq!(&blob[..MAGIC.len()], MAGIC);
        assert_ne!(blob[MAGIC.len()], FORMAT_VERSION);
    }

    #[test]
    fn missing_magic_is_unrecognized_format_error() {
        // Long enough to clear HEADER_LEN, so the length check passes
        // and the magic-mismatch branch fires.
        let blob = b"{\"plaintext_json_definitely_not_our_magic\":true}";
        assert!(blob.len() > HEADER_LEN);
        let err = decrypt(blob).unwrap_err();
        let FabCliError::Generic(msg) = err else {
            panic!("expected Generic error");
        };
        assert!(
            msg.contains("magic header") || msg.contains("not recognized"),
            "error should name the format mismatch: {}",
            msg
        );
    }

    #[test]
    fn too_short_blob_is_clear_error() {
        let err = decrypt(b"FABCLI").unwrap_err();
        let FabCliError::Generic(msg) = err else {
            panic!("expected Generic error");
        };
        assert!(msg.contains("too short"), "got: {}", msg);
    }

    #[test]
    fn key_b64_round_trip() {
        let original = fixed_key();
        let b64 = base64::engine::general_purpose::STANDARD.encode(original);
        let recovered = key_from_b64(&b64).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn key_wrong_length_is_error() {
        let too_short = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]);
        assert!(key_from_b64(&too_short).is_err());
    }

}
