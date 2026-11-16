//! The database key (`Config::db_key`): where it's kept between runs.
//!
//! Both databases are encrypted with one 32-byte key. Whoever has the key
//! and the data directory can read every message, so where the key lives
//! decides who can. Three choices, picked by the frontend:
//!
//! - [`KeySource::File`] (the default): random bytes in `database.key` in the
//!   data directory, readable only by its owner (mode 0600). It's what
//!   Signal Desktop does when there's no keyring. Anything running as the
//!   same user can read it; full-disk encryption protects it at rest.
//! - [`KeySource::Passphrase`]: derived from a passphrase with Argon2id each
//!   start, so nothing on disk unlocks the data. The user types it every
//!   launch, and a forgotten passphrase loses the local history (the phone
//!   keeps its own).
//! - [`KeySource::Keyring`] (cargo feature `keyring`): random bytes kept by
//!   the Secret Service (GNOME Keyring, KWallet, KeePassXC, oo7), as Signal
//!   Desktop does when it finds one. [`KeySource::Auto`] uses it when one
//!   answers and falls back to the file.
//!
//! The choice is made once, when the data directory is created: an existing
//! directory's key has to come from where it was first kept.
//! Call [`load_or_create`] before [`crate::Backend::start`], from a thread
//! outside any async runtime (the keyring client blocks).

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use rand::RngCore as _;
use sha2::Digest as _;

/// Where the database key comes from. See the module docs.
#[derive(Clone)]
pub enum KeySource {
    /// `database.key` in the data directory, mode 0600.
    File,
    /// Derived from this passphrase (Argon2id, with a random salt kept in
    /// `database.salt`).
    Passphrase(String),
    /// The Secret Service, under this service name (one entry per data
    /// directory). Needs the `keyring` feature.
    #[cfg(feature = "keyring")]
    Keyring { service: String },
    /// The keyring when one answers, else the file. A data directory that
    /// already has a key file keeps using it.
    #[cfg(feature = "keyring")]
    Auto { service: String },
}

impl std::fmt::Debug for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the passphrase.
        f.write_str(match self {
            Self::File => "File",
            Self::Passphrase(_) => "Passphrase",
            #[cfg(feature = "keyring")]
            Self::Keyring { .. } => "Keyring",
            #[cfg(feature = "keyring")]
            Self::Auto { .. } => "Auto",
        })
    }
}

/// How the key was found, for the frontend to show or log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptIn {
    File,
    Passphrase,
    Keyring,
}

const KEY_FILE: &str = "database.key";
const SALT_FILE: &str = "database.salt";
/// A check value for the passphrase's key, so a wrong passphrase is told
/// apart from a damaged database.
const CHECK_FILE: &str = "database.check";

/// The data directory's database key, made (and kept per `source`) the
/// first time. Fails rather than make a new key for databases that exist:
/// a new key couldn't open them.
pub fn load_or_create(data_dir: &Path, source: &KeySource) -> anyhow::Result<([u8; 32], KeptIn)> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let existing = data_dir.join("app.db").exists() || data_dir.join("signal.db").exists();
    match source {
        KeySource::File => file_key(data_dir, existing).map(|k| (k, KeptIn::File)),
        KeySource::Passphrase(passphrase) => passphrase_key(data_dir, passphrase, existing).map(|k| (k, KeptIn::Passphrase)),
        #[cfg(feature = "keyring")]
        KeySource::Keyring { service } => keyring::key(service, data_dir, existing).map(|k| (k, KeptIn::Keyring)),
        #[cfg(feature = "keyring")]
        KeySource::Auto { service } => {
            if data_dir.join(KEY_FILE).exists() {
                return file_key(data_dir, existing).map(|k| (k, KeptIn::File));
            }
            match keyring::key(service, data_dir, existing) {
                Ok(key) => Ok((key, KeptIn::Keyring)),
                Err(error) if !existing => {
                    tracing::warn!(%error, "no keyring; keeping the database key in a file");
                    file_key(data_dir, existing).map(|k| (k, KeptIn::File))
                }
                Err(error) => Err(error),
            }
        }
    }
}

fn random_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    key
}

/// Writes `bytes` to a new file only its owner can read; fails if it exists.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn decode_key(text: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(text.trim()).context("the key isn't hex")?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok().context("the key isn't 32 bytes")
}

fn file_key(data_dir: &Path, existing: bool) -> anyhow::Result<[u8; 32]> {
    let path = data_dir.join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(text) => decode_key(&text).with_context(|| format!("reading {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if existing {
                bail!("{} is missing, and the databases need the key it held", path.display());
            }
            let key = random_key();
            write_private(&path, hex::encode(key).as_bytes())?;
            Ok(key)
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn check_value(key: &[u8; 32]) -> String {
    let mut hash = sha2::Sha256::new();
    hash.update(b"langis database key check");
    hash.update(key);
    hex::encode(&hash.finalize()[..16])
}

/// Argon2id with OWASP's first recommended setting (19 MiB, two passes,
/// one lane) raised to 64 MiB and three passes: a fraction of a second per
/// start, and every guess costs an attacker the same.
fn derive(passphrase: &str, salt: &[u8]) -> anyhow::Result<[u8; 32]> {
    let params = argon2::Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    argon.hash_password_into(passphrase.as_bytes(), salt, &mut key).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(key)
}

fn passphrase_key(data_dir: &Path, passphrase: &str, existing: bool) -> anyhow::Result<[u8; 32]> {
    if passphrase.is_empty() {
        bail!("the passphrase is empty");
    }
    let (salt_path, check_path) = (data_dir.join(SALT_FILE), data_dir.join(CHECK_FILE));
    match std::fs::read(&salt_path) {
        Ok(salt) => {
            let key = derive(passphrase, &salt)?;
            let check = std::fs::read_to_string(&check_path).with_context(|| format!("reading {}", check_path.display()))?;
            if check.trim() != check_value(&key) {
                bail!("wrong passphrase");
            }
            Ok(key)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if existing {
                bail!("{} is missing: these databases weren't set up with a passphrase", salt_path.display());
            }
            let mut salt = [0u8; 16];
            rand::rng().fill_bytes(&mut salt);
            let key = derive(passphrase, &salt)?;
            write_private(&salt_path, &salt)?;
            write_private(&check_path, check_value(&key).as_bytes())?;
            Ok(key)
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", salt_path.display())),
    }
}

#[cfg(feature = "keyring")]
mod keyring {
    use std::path::Path;

    use anyhow::{Context as _, bail};

    /// One entry per data directory, under the frontend's service name.
    fn entry(service: &str, data_dir: &Path) -> anyhow::Result<::keyring::Entry> {
        let account = data_dir.canonicalize().unwrap_or_else(|_| data_dir.to_owned());
        ::keyring::Entry::new(service, &account.to_string_lossy()).context("opening the keyring")
    }

    pub(super) fn key(service: &str, data_dir: &Path, existing: bool) -> anyhow::Result<[u8; 32]> {
        let entry = entry(service, data_dir)?;
        match entry.get_password() {
            Ok(text) => super::decode_key(&text).context("the keyring's database key"),
            Err(::keyring::Error::NoEntry) => {
                if existing {
                    bail!("the keyring has no database key for {}", data_dir.display());
                }
                let key = super::random_key();
                entry.set_password(&hex::encode(key)).context("saving the database key in the keyring")?;
                Ok(key)
            }
            Err(error) => Err(error).context("reading the keyring"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_file_key_is_made_once_private_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (key, kept) = load_or_create(dir.path(), &KeySource::File).unwrap();
        assert_eq!(kept, KeptIn::File);
        let mode = std::fs::metadata(dir.path().join(KEY_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(load_or_create(dir.path(), &KeySource::File).unwrap().0, key);
        assert_ne!(key, [0; 32]);
    }

    #[test]
    fn no_new_key_for_databases_that_exist() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.db"), b"x").unwrap();
        assert!(load_or_create(dir.path(), &KeySource::File).is_err());
        assert!(load_or_create(dir.path(), &KeySource::Passphrase("pw".into())).is_err());
    }

    #[test]
    fn a_passphrase_gives_the_same_key_and_a_wrong_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let source = KeySource::Passphrase("correct horse".into());
        let (key, kept) = load_or_create(dir.path(), &source).unwrap();
        assert_eq!(kept, KeptIn::Passphrase);
        assert_eq!(load_or_create(dir.path(), &source).unwrap().0, key);
        let wrong = load_or_create(dir.path(), &KeySource::Passphrase("wrong horse".into()));
        assert!(wrong.unwrap_err().to_string().contains("wrong passphrase"));
        // Nothing on disk is the key itself.
        for file in [SALT_FILE, CHECK_FILE] {
            let bytes = std::fs::read(dir.path().join(file)).unwrap();
            assert!(!bytes.windows(32).any(|w| w == key));
            assert!(!String::from_utf8_lossy(&bytes).contains(&hex::encode(key)));
        }
        assert!(!dir.path().join(KEY_FILE).exists());
        assert!(load_or_create(dir.path(), &KeySource::Passphrase(String::new())).is_err());
    }

    #[test]
    fn the_passphrase_is_never_printed() {
        assert_eq!(format!("{:?}", KeySource::Passphrase("secret".into())), "Passphrase");
    }
}
