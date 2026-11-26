//! The key that seals what vtteams keeps on disk, and the sealing itself.
//!
//! One random 256-bit key lives in the operating system's credential store (Windows Credential Manager, the macOS
//! Keychain, the Secret Service on Linux): a small entry, because the stores limit what one entry can hold. The stored
//! sign-in and the start-up cache are encrypted with it, so a copy of the settings or cache folder (a backup, a roaming
//! profile, a stolen disk, another account on the computer) is of no use without the key. **Nothing here protects against
//! a program running as the same user:** the credential store gives the key to any process of that user, as it gives
//! them everything else in it.
//!
//! A sealed file is `VTSE`, a version byte, a random 24-byte nonce and the XChaCha20-Poly1305 ciphertext with its tag. The
//! associated data binds the ciphertext to what the file is for (credentials of a service, the cache), the version and
//! the identity it belongs to, so a file cannot be moved to another purpose, version or account and still open.
//!
//! This is the only module that touches the credential store. It is behind the [`KeyProvider`] trait so that tests, and
//! the code that decides what to do when the store cannot be used, never reach the real one.

use chacha20poly1305::aead::{Aead, Generate, Key, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use teams_client::{CredentialStore, FileStore, StoreError};
use zeroize::Zeroizing;

/// The length of the key, in bytes.
pub const KEY_LEN: usize = 32;
const MAGIC: &[u8; 4] = b"VTSE";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
const HEADER_LEN: usize = MAGIC.len() + 1 + NONCE_LEN;

/// A 256-bit key. It is wiped from memory when dropped and never printed.
#[derive(Clone)]
pub struct SealKey(Zeroizing<[u8; KEY_LEN]>);

impl SealKey {
    /// A new random key.
    pub fn generate() -> Self {
        let key = Key::<XChaCha20Poly1305>::generate();
        let mut bytes = Zeroizing::new([0u8; KEY_LEN]);
        bytes.copy_from_slice(key.as_slice());
        Self(bytes)
    }

    /// A key from exactly [`KEY_LEN`] bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        let array: [u8; KEY_LEN] = bytes.try_into().ok()?;
        Some(Self(Zeroizing::new(array)))
    }

    fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl fmt::Debug for SealKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SealKey(..)")
    }
}

/// Why the credential store could not give or take the key. The texts name no secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// There is no usable credential store: no Secret Service on a computer with no desktop session, say.
    Unavailable(String),
    /// The store is there but refused or failed: it is locked, or access was denied.
    Failed(String),
    /// An entry is there but is not a key of ours (it is not [`KEY_LEN`] bytes).
    Malformed,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) => write!(
                f,
                "the operating system's credential store is not available: {why}"
            ),
            Self::Failed(why) => write!(
                f,
                "the operating system's credential store did not answer: {why}"
            ),
            Self::Malformed => f.write_str(
                "the vtteams entry in the operating system's credential store is not a key (delete it and sign in again)",
            ),
        }
    }
}

impl std::error::Error for KeyError {}

/// Where the sealing key is kept.
pub trait KeyProvider: Send + Sync {
    /// The key, or `None` when none has been made. Never makes one: reading what was sealed must not invent a key that
    /// can only fail to open it.
    fn get(&self) -> Result<Option<SealKey>, KeyError>;

    /// The key, making and storing a random one first when there is none.
    fn get_or_create(&self) -> Result<SealKey, KeyError>;

    /// Forget the key. Returns whether there was one. Whatever it sealed can no longer be opened.
    fn delete(&self) -> Result<bool, KeyError>;
}

/// The operating system's credential store.
pub struct OsKeys {
    entry: keyring_core::Entry,
    /// The key once the store has given it, so the store is asked once for the run and not for every file.
    held: Mutex<Option<SealKey>>,
}

impl fmt::Debug for OsKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OsKeys").finish_non_exhaustive()
    }
}

/// The name the entry has in the credential store.
const SERVICE: &str = "vtteams";
const USER: &str = "storage-key";

impl OsKeys {
    /// Connect to this platform's credential store and name the entry. Does not read or write it.
    pub fn open() -> Result<Self, KeyError> {
        install_platform_store()?;
        let entry = new_entry().map_err(from_core)?;
        Ok(Self {
            entry,
            held: Mutex::new(None),
        })
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Option<SealKey>> {
        // The guarded value is replaced whole, so it is valid whatever a panicking holder was doing.
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl KeyProvider for OsKeys {
    fn get(&self) -> Result<Option<SealKey>, KeyError> {
        let mut held = self.held();
        if let Some(key) = held.as_ref() {
            return Ok(Some(key.clone()));
        }
        match self.entry.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let key = SealKey::from_slice(&bytes).ok_or(KeyError::Malformed)?;
                *held = Some(key.clone());
                Ok(Some(key))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(from_core(e)),
        }
    }

    fn get_or_create(&self) -> Result<SealKey, KeyError> {
        if let Some(key) = self.get()? {
            return Ok(key);
        }
        let key = SealKey::generate();
        self.entry.set_secret(key.as_bytes()).map_err(from_core)?;
        // Two copies of the program starting together may both have made a key; whichever the store kept is the one
        // everything must be sealed with, so ask it rather than trusting what was just written.
        let stored = self
            .entry
            .get_secret()
            .map(Zeroizing::new)
            .map_err(from_core)?;
        let key = SealKey::from_slice(&stored).ok_or(KeyError::Malformed)?;
        *self.held() = Some(key.clone());
        Ok(key)
    }

    fn delete(&self) -> Result<bool, KeyError> {
        *self.held() = None;
        match self.entry.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring_core::Error::NoEntry) => Ok(false),
            Err(e) => Err(from_core(e)),
        }
    }
}

fn from_core(e: keyring_core::Error) -> KeyError {
    use keyring_core::Error as E;
    match e {
        E::NoDefaultStore => KeyError::Unavailable("no credential store for this platform".into()),
        E::NoStorageAccess(why) => KeyError::Failed(why.to_string()),
        E::BadDataFormat(..) | E::BadEncoding(_) => KeyError::Malformed,
        other => KeyError::Failed(other.to_string()),
    }
}

/// Make this platform's store the one `keyring_core` entries are made in, once for the process.
fn install_platform_store() -> Result<(), KeyError> {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<Result<(), KeyError>> = OnceLock::new();
    INSTALLED
        .get_or_init(|| {
            #[cfg(target_os = "windows")]
            let store = windows_native_keyring_store::Store::new();
            #[cfg(target_os = "macos")]
            let store = apple_native_keyring_store::keychain::Store::new();
            #[cfg(all(
                unix,
                not(any(target_os = "macos", target_os = "ios", target_os = "android"))
            ))]
            let store = zbus_secret_service_keyring_store::Store::new();
            #[cfg(any(
                target_os = "windows",
                target_os = "macos",
                all(
                    unix,
                    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
                )
            ))]
            {
                match store {
                    Ok(store) => {
                        keyring_core::set_default_store(store);
                        Ok(())
                    }
                    Err(e) => Err(KeyError::Unavailable(e.to_string())),
                }
            }
            #[cfg(not(any(
                target_os = "windows",
                target_os = "macos",
                all(
                    unix,
                    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
                )
            )))]
            Err(KeyError::Unavailable(
                "this platform has no credential store".into(),
            ))
        })
        .clone()
}

/// The entry. On Windows it is `Local`: kept for this computer and this user, not copied to other computers by a
/// roaming profile (the store's default is `Enterprise`, which roams, and would carry the key to where the sealed
/// sign-in is not meant to open).
fn new_entry() -> keyring_core::Result<keyring_core::Entry> {
    #[cfg(target_os = "windows")]
    {
        let modifiers = std::collections::HashMap::from([("persistence", "Local")]);
        keyring_core::Entry::new_with_modifiers(SERVICE, USER, &modifiers)
    }
    #[cfg(not(target_os = "windows"))]
    {
        keyring_core::Entry::new(SERVICE, USER)
    }
}

/// What a sealed file is for, and whom it belongs to. All of it is authenticated with the data, so changing any of it
/// makes the file refuse to open.
#[derive(Debug, Clone, Copy)]
pub struct Aad<'a> {
    /// What the file is: `credentials`, or `cache`.
    pub purpose: &'a str,
    /// The version of what is inside; a file of another version does not open.
    pub version: u8,
    /// Who it belongs to: the service for credentials, service and account for the cache.
    pub identity: &'a str,
}

impl Aad<'_> {
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"vtteams-seal\0");
        out.push(self.version);
        for part in [self.purpose, self.identity] {
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part.as_bytes());
        }
        out
    }
}

/// Why a sealed file did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// It does not begin like a sealed file.
    NotSealed,
    /// It is of another version.
    Version { found: u8, expected: u8 },
    /// It is shorter than the least a sealed file can be.
    Truncated,
    /// It is longer than the caller allows.
    TooLarge,
    /// It did not authenticate: the wrong key, another purpose or account, or the bytes were changed.
    Rejected,
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSealed => f.write_str("it is not an encrypted vtteams file"),
            Self::Version { found, expected } => write!(
                f,
                "it is version {found} and this vtteams reads version {expected}"
            ),
            Self::Truncated => f.write_str("it is cut short"),
            Self::TooLarge => f.write_str("it is larger than any file vtteams writes"),
            Self::Rejected => f.write_str(
                "it could not be decrypted: it was sealed with another key or for another account, or it was changed",
            ),
        }
    }
}

impl std::error::Error for OpenError {}

/// Encrypt `plaintext` under `key` for `aad`, with a fresh random nonce.
pub fn seal(key: &SealKey, aad: &Aad<'_>, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
    let cipher = XChaCha20Poly1305::new(&(*key.as_bytes()).into());
    let nonce = XNonce::generate();
    let body = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &aad.bytes(),
            },
        )
        .map_err(|_| VaultError::Seal)?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(MAGIC);
    out.push(aad.version);
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decrypt what [`seal`] made, refusing anything that is not exactly that for `aad`. `max_plain` bounds the answer: a
/// file larger than that is refused before any of it is decrypted.
pub fn open(
    key: &SealKey,
    aad: &Aad<'_>,
    sealed: &[u8],
    max_plain: usize,
) -> Result<Zeroizing<Vec<u8>>, OpenError> {
    if sealed.len() < MAGIC.len() || &sealed[..MAGIC.len()] != MAGIC {
        return Err(OpenError::NotSealed);
    }
    if sealed.len() < HEADER_LEN + TAG_LEN {
        return Err(OpenError::Truncated);
    }
    let found = sealed[MAGIC.len()];
    if found != aad.version {
        return Err(OpenError::Version {
            found,
            expected: aad.version,
        });
    }
    if sealed.len() - HEADER_LEN - TAG_LEN > max_plain {
        return Err(OpenError::TooLarge);
    }
    let nonce_start = MAGIC.len() + 1;
    let nonce =
        XNonce::try_from(&sealed[nonce_start..HEADER_LEN]).map_err(|_| OpenError::Truncated)?;
    let cipher = XChaCha20Poly1305::new(&(*key.as_bytes()).into());
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &sealed[HEADER_LEN..],
                aad: &aad.bytes(),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| OpenError::Rejected)
}

/// Why sealed credentials could not be read or written. It is carried as the `source` of a
/// [`StoreError::Corrupt`], so the library's callers see a typed error without the library knowing about encryption.
#[derive(Debug)]
pub enum VaultError {
    /// The credential store could not give the key.
    Key(KeyError),
    /// There is a sealed file but no key to open it: the folder came from another computer or account, or the key was
    /// deleted.
    KeyMissing,
    /// The file did not open.
    Open(OpenError),
    /// Encryption failed (not expected: the input is ours and small).
    Seal,
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(e) => write!(f, "{e}"),
            Self::KeyMissing => f.write_str(
                "the encrypted sign-in cannot be opened: the key for it is not in this computer's credential store (the settings folder may have been copied from another computer or account)",
            ),
            Self::Open(e) => write!(f, "the encrypted sign-in cannot be opened: {e}"),
            Self::Seal => f.write_str("the sign-in could not be encrypted"),
        }
    }
}

impl std::error::Error for VaultError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Key(e) => Some(e),
            Self::Open(e) => Some(e),
            Self::KeyMissing | Self::Seal => None,
        }
    }
}

/// The most a stored sign-in can be; they are a few kilobytes.
const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
/// The version of the sealed credentials.
const CREDENTIAL_VERSION: u8 = 1;

/// A [`CredentialStore`] that keeps its bytes sealed in a file. The library hands it its JSON; it never sees the key.
pub struct EncryptedStore {
    file: FileStore,
    keys: Arc<dyn KeyProvider>,
    /// Which service these credentials are for.
    identity: &'static str,
}

impl fmt::Debug for EncryptedStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedStore")
            .field("path", &self.file.path())
            .finish_non_exhaustive()
    }
}

impl EncryptedStore {
    /// Credentials for `identity` (`teams` or `graph`) in the file at `path`.
    pub fn new(
        path: impl Into<PathBuf>,
        keys: Arc<dyn KeyProvider>,
        identity: &'static str,
    ) -> Self {
        Self {
            file: FileStore::new(path),
            keys,
            identity,
        }
    }

    pub fn path(&self) -> &Path {
        self.file.path()
    }

    fn aad(&self) -> Aad<'_> {
        Aad {
            purpose: "credentials",
            version: CREDENTIAL_VERSION,
            identity: self.identity,
        }
    }
}

fn unreadable(e: VaultError) -> StoreError {
    StoreError::Corrupt {
        source: Box::new(e),
    }
}

impl CredentialStore for EncryptedStore {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreError> {
        let Some(sealed) = self.file.load()? else {
            return Ok(None);
        };
        let key = match self.keys.get() {
            Ok(Some(key)) => key,
            Ok(None) => return Err(unreadable(VaultError::KeyMissing)),
            Err(e) => return Err(unreadable(VaultError::Key(e))),
        };
        open(&key, &self.aad(), &sealed, MAX_CREDENTIAL_BYTES)
            .map(|plain| Some(plain.to_vec()))
            .map_err(|e| unreadable(VaultError::Open(e)))
    }

    fn save(&self, bytes: &[u8]) -> Result<(), StoreError> {
        let cannot_encrypt = |e: VaultError| StoreError::Io {
            action: "encrypt",
            path: self.file.path().to_path_buf(),
            source: std::io::Error::other(e),
        };
        let key = self
            .keys
            .get_or_create()
            .map_err(|e| cannot_encrypt(VaultError::Key(e)))?;
        let sealed = seal(&key, &self.aad(), bytes).map_err(cannot_encrypt)?;
        self.file.save(&sealed)
    }

    fn clear(&self) -> Result<bool, StoreError> {
        self.file.clear()
    }
}

/// Overwrite a file's bytes with zeros, then delete it. The overwrite is best effort (a journaling or copy-on-write file
/// system, or an SSD, may keep the old blocks); the deletion is what is reported.
pub fn overwrite_and_remove(path: &Path) -> std::io::Result<()> {
    let _ = overwrite_with_zeros(path);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn overwrite_with_zeros(path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let len = std::fs::metadata(path)?.len();
    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;
    let zeros = [0u8; 4096];
    let mut left = len;
    while left > 0 {
        let n = left.min(zeros.len() as u64) as usize;
        file.write_all(&zeros[..n])?;
        left -= n as u64;
    }
    file.sync_all()
}

/// A key provider that lives in memory, for tests. It can be made to fail.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemoryKeys {
    key: Mutex<Option<SealKey>>,
    /// While set, every call fails with this.
    pub broken: Mutex<Option<KeyError>>,
}

#[cfg(test)]
impl MemoryKeys {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn broken(error: KeyError) -> Arc<Self> {
        let keys = Self::default();
        *keys.broken.lock().unwrap() = Some(error);
        Arc::new(keys)
    }

    pub fn has_key(&self) -> bool {
        self.key.lock().unwrap().is_some()
    }

    fn check(&self) -> Result<(), KeyError> {
        match &*self.broken.lock().unwrap() {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
impl KeyProvider for MemoryKeys {
    fn get(&self) -> Result<Option<SealKey>, KeyError> {
        self.check()?;
        Ok(self.key.lock().unwrap().clone())
    }

    fn get_or_create(&self) -> Result<SealKey, KeyError> {
        self.check()?;
        Ok(self
            .key
            .lock()
            .unwrap()
            .get_or_insert_with(SealKey::generate)
            .clone())
    }

    fn delete(&self) -> Result<bool, KeyError> {
        self.check()?;
        Ok(self.key.lock().unwrap().take().is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdir;
    use std::error::Error as _;

    const AAD: Aad<'static> = Aad {
        purpose: "cache",
        version: 1,
        identity: "teams:ada",
    };

    fn temp(name: &str) -> testdir::TestDir {
        testdir::TestDir::new(&format!("vault-{name}"))
    }

    #[test]
    fn what_is_sealed_opens_with_the_same_key_and_aad_and_not_otherwise() {
        let key = SealKey::generate();
        let sealed = seal(&key, &AAD, b"hello world").unwrap();
        assert_eq!(&sealed[..4], b"VTSE");
        assert_eq!(sealed.len(), HEADER_LEN + 11 + TAG_LEN);
        assert_eq!(
            open(&key, &AAD, &sealed, 100).unwrap().as_slice(),
            b"hello world"
        );
        let other = SealKey::generate();
        assert_eq!(open(&other, &AAD, &sealed, 100), Err(OpenError::Rejected));
    }

    #[test]
    fn the_account_the_purpose_and_the_version_are_authenticated() {
        let key = SealKey::generate();
        let sealed = seal(&key, &AAD, b"x").unwrap();
        for wrong in [
            Aad {
                identity: "teams:grace",
                ..AAD
            },
            Aad {
                purpose: "credentials",
                ..AAD
            },
        ] {
            assert_eq!(open(&key, &wrong, &sealed, 100), Err(OpenError::Rejected));
        }
        let newer = Aad { version: 2, ..AAD };
        assert_eq!(
            open(&key, &newer, &sealed, 100),
            Err(OpenError::Version {
                found: 1,
                expected: 2
            })
        );
        // The version byte in the file is part of what is authenticated too: claiming a version it was not sealed
        // under is not accepted even when the caller expects that version.
        let mut forged = sealed.clone();
        forged[4] = 2;
        assert_eq!(open(&key, &newer, &forged, 100), Err(OpenError::Rejected));
    }

    #[test]
    fn changing_any_byte_or_cutting_the_file_short_is_refused() {
        let key = SealKey::generate();
        let sealed = seal(&key, &AAD, b"some text worth protecting").unwrap();
        for at in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[at] ^= 0x01;
            assert!(open(&key, &AAD, &bad, 1000).is_err(), "byte {at}");
        }
        for len in 0..sealed.len() {
            assert!(
                open(&key, &AAD, &sealed[..len], 1000).is_err(),
                "length {len}"
            );
        }
        let mut longer = sealed.clone();
        longer.push(0);
        assert_eq!(open(&key, &AAD, &longer, 1000), Err(OpenError::Rejected));
        assert_eq!(open(&key, &AAD, b"", 10), Err(OpenError::NotSealed));
        assert_eq!(
            open(&key, &AAD, b"plain text, not sealed at all", 100),
            Err(OpenError::NotSealed)
        );
    }

    #[test]
    fn a_file_over_the_limit_is_refused_before_it_is_decrypted() {
        let key = SealKey::generate();
        let sealed = seal(&key, &AAD, &[7u8; 100]).unwrap();
        assert_eq!(open(&key, &AAD, &sealed, 99), Err(OpenError::TooLarge));
        assert!(open(&key, &AAD, &sealed, 100).is_ok());
    }

    #[test]
    fn every_write_uses_a_new_nonce_and_so_gives_different_bytes() {
        let key = SealKey::generate();
        let nonces: std::collections::HashSet<Vec<u8>> = (0..200)
            .map(|_| seal(&key, &AAD, b"same text").unwrap()[5..HEADER_LEN].to_vec())
            .collect();
        assert_eq!(nonces.len(), 200);
        assert_ne!(
            seal(&key, &AAD, b"same text").unwrap(),
            seal(&key, &AAD, b"same text").unwrap()
        );
    }

    #[test]
    fn the_key_is_never_printed() {
        let key = SealKey::generate();
        assert_eq!(format!("{key:?}"), "SealKey(..)");
        assert!(SealKey::from_slice(&[0; 31]).is_none());
        assert!(SealKey::from_slice(&[0; 32]).is_some());
    }

    fn store(dir: &Path, keys: Arc<dyn KeyProvider>) -> EncryptedStore {
        EncryptedStore::new(dir.join("teams-session.enc"), keys, "teams")
    }

    #[test]
    fn credentials_round_trip_and_the_file_holds_none_of_them_in_the_clear() {
        let dir = temp("round-trip");
        let keys = MemoryKeys::new();
        let store = store(&dir, keys.clone());
        assert_eq!(store.load().unwrap(), None);
        let secret = br#"{"refresh_token":"Zaphod-Beeblebrox-refresh","oid":"ada-0001"}"#;
        store.save(secret).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some(&secret[..]));
        let raw = std::fs::read(store.path()).unwrap();
        for planted in ["Zaphod-Beeblebrox", "refresh_token", "ada-0001"] {
            assert!(
                !raw.windows(planted.len()).any(|w| w == planted.as_bytes()),
                "{planted} is readable in the file"
            );
        }
        assert!(keys.has_key());
        assert!(store.clear().unwrap() && !store.clear().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn credentials_that_do_not_open_are_a_typed_error_that_says_why() {
        let dir = temp("typed");
        let keys = MemoryKeys::new();
        let store = store(&dir, keys.clone());
        store.save(b"{}").unwrap();

        // The key is gone (another computer, or the entry was deleted).
        keys.delete().unwrap();
        let e = store.load().unwrap_err();
        assert!(matches!(e, StoreError::Corrupt { .. }), "{e:?}");
        let source = e.source().unwrap();
        assert!(matches!(
            source.downcast_ref::<VaultError>(),
            Some(VaultError::KeyMissing)
        ));
        assert!(
            e.to_string()
                .contains("not in this computer's credential store"),
            "{e}"
        );

        // Another key (the entry was replaced).
        let other = MemoryKeys::new();
        other.get_or_create().unwrap();
        let wrong = EncryptedStore::new(store.path(), other, "teams");
        let e = wrong.load().unwrap_err();
        assert!(e.to_string().contains("could not be decrypted"), "{e}");

        // The file is for another service.
        let keys = MemoryKeys::new();
        let a = EncryptedStore::new(dir.join("a.enc"), keys.clone(), "teams");
        a.save(b"{}").unwrap();
        std::fs::copy(a.path(), dir.join("b.enc")).unwrap();
        let b = EncryptedStore::new(dir.join("b.enc"), keys, "graph");
        assert!(
            b.load().is_err(),
            "a Teams sign-in does not open as a Graph one"
        );

        // A byte changed in the file.
        let keys = MemoryKeys::new();
        let s = EncryptedStore::new(dir.join("c.enc"), keys, "teams");
        s.save(b"{}").unwrap();
        let mut raw = std::fs::read(s.path()).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(s.path(), raw).unwrap();
        assert!(s.load().is_err());

        // The credential store itself fails.
        let broken = EncryptedStore::new(
            store.path(),
            MemoryKeys::broken(KeyError::Failed("locked".into())),
            "teams",
        );
        let e = broken.load().unwrap_err();
        assert!(e.to_string().contains("locked"), "{e}");
        let e = broken.save(b"{}").unwrap_err();
        assert!(
            matches!(
                e,
                StoreError::Io {
                    action: "encrypt",
                    ..
                }
            ) && e.to_string().contains("locked"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_with_a_broken_key_store_writes_nothing() {
        let dir = temp("nothing");
        let keys = MemoryKeys::broken(KeyError::Unavailable("no session bus".into()));
        let store = store(&dir, keys);
        assert!(store.save(b"secret").is_err());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "no file, no temp file"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overwriting_before_removing_zeroes_the_bytes_that_were_there() {
        let dir = temp("scrub");
        let path = dir.join("secret.txt");
        std::fs::write(&path, "top secret text").unwrap();
        overwrite_with_zeros(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![0u8; 15]);
        overwrite_and_remove(&path).unwrap();
        assert!(!path.exists());
        overwrite_and_remove(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
