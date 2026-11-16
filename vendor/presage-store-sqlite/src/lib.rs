use std::str::FromStr;

use presage::{
    libsignal_service::{
        libsignal_account_keys::AccountEntropyPool, prelude::MasterKey, protocol::SenderCertificate,
    },
    store::{StateStore, Store},
};
use protocol::{IdentityType, SqliteProtocolStore};
use sqlx::{
    SqlitePool, query, query_scalar,
    sqlite::{SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};

mod content;
mod data;
mod error;
mod protocol;

pub use error::SqliteStoreError;
pub use presage::model::identity::OnNewIdentity;
pub use sqlx::sqlite::SqliteConnectOptions;

use crate::error::SqlxErrorExt;

#[derive(Debug, Clone)]
pub struct SqliteStore {
    pub(crate) db: SqlitePool,
    pub(crate) trust_new_identities: OnNewIdentity,
    pub(crate) verified: VerifiedKeys,
}

/// Langis: the identity keys the user marked verified, by protocol address
/// name (the service id string), shared by every clone of the store.
///
/// A send to a verified contact whose identity key has changed is refused
/// (`is_trusted_identity` returns false for [`Direction::Sending`]) until the
/// app clears or updates their entry, as Signal Desktop holds such a send
/// for the user's confirmation. Each refused key is kept for the app to
/// take with [`VerifiedKeys::take_refused`]. Receiving is unaffected: a
/// changed key is accepted and the app shows that it changed.
#[derive(Debug, Clone, Default)]
pub struct VerifiedKeys(std::sync::Arc<std::sync::Mutex<VerifiedState>>);

#[derive(Debug, Default)]
struct VerifiedState {
    keys: std::collections::HashMap<String, Vec<u8>>,
    refused: Vec<(String, Vec<u8>)>,
}

impl VerifiedKeys {
    fn state(&self) -> std::sync::MutexGuard<'_, VerifiedState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces every entry: address name to the serialized identity key
    /// marked verified.
    pub fn replace(&self, keys: std::collections::HashMap<String, Vec<u8>>) {
        self.state().keys = keys;
    }

    /// Whether a send to `address` may use `identity`: refused (and kept for
    /// [`Self::take_refused`]) when they're verified under another key.
    pub fn allows_sending(&self, address: &str, identity: &[u8]) -> bool {
        let mut state = self.state();
        match state.keys.get(address) {
            Some(verified) if verified.as_slice() != identity => {
                state.refused.push((address.to_owned(), identity.to_vec()));
                false
            }
            _ => true,
        }
    }

    /// The (address name, new identity key) of each send refused since the
    /// last call.
    pub fn take_refused(&self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut self.state().refused)
    }
}

impl SqliteStore {
    /// Begins a transaction that writes, taking the write lock at once. A deferred transaction
    /// that reads before it writes fails with `SQLITE_BUSY`, which no busy timeout waits out,
    /// when another connection to the database writes in between.
    pub(crate) async fn begin_write(
        &self,
    ) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, sqlx::Error> {
        self.db.begin_with("BEGIN IMMEDIATE").await
    }

    pub async fn open(
        url: &str,
        trust_new_identities: OnNewIdentity,
    ) -> Result<Self, SqliteStoreError> {
        Self::open_with_passphrase(url, None, trust_new_identities).await
    }

    pub async fn open_with_passphrase(
        url: &str,
        passphrase: Option<&str>,
        trust_new_identities: OnNewIdentity,
    ) -> Result<Self, SqliteStoreError> {
        // Escape the passphrase.
        let passphrase = passphrase.map(|p| p.replace("'", "''"));

        let options: SqliteConnectOptions = url.parse()?;
        let options = options.create_if_missing(true).foreign_keys(true);
        let options = if let Some(passphrase) = &passphrase {
            options.pragma("key", format!("'{passphrase}'"))
        } else {
            options
        };
        Self::open_with_options(options.clone(), trust_new_identities.clone()).await
    }

    pub async fn open_with_options(
        options: SqliteConnectOptions,
        trust_new_identities: OnNewIdentity,
    ) -> Result<Self, SqliteStoreError> {
        let options = options
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full);
        // Each connection carries its own page cache; sqlx's default of 10 is more than we need.
        let db = SqlitePoolOptions::new()
            .max_connections(3)
            .connect_with(options)
            .await?;

        sqlx::migrate!().run(&db).await?;
        Ok(Self {
            db,
            trust_new_identities,
            verified: VerifiedKeys::default(),
        })
    }

    /// Langis: the verified keys sends are checked against (shared by every
    /// clone of this store).
    pub fn verified_keys(&self) -> VerifiedKeys {
        self.verified.clone()
    }

    /// Closes the pool, waiting for all connections to close.
    /// Call before exit: SQLCipher's atexit cleanup races connections still closing.
    pub async fn close(&self) {
        self.db.close().await;
    }
}

impl Store for SqliteStore {
    type Error = SqliteStoreError;

    type AciStore = SqliteProtocolStore;

    type PniStore = SqliteProtocolStore;

    async fn clear(&mut self) -> Result<(), SqliteStoreError> {
        query!("DELETE FROM kv").execute(&self.db).await?;
        Ok(())
    }

    fn aci_protocol_store(&self) -> Self::AciStore {
        SqliteProtocolStore {
            store: self.clone(),
            identity: IdentityType::Aci,
        }
    }

    fn pni_protocol_store(&self) -> Self::PniStore {
        SqliteProtocolStore {
            store: self.clone(),
            identity: IdentityType::Pni,
        }
    }
}

impl StateStore for SqliteStore {
    type StateStoreError = SqliteStoreError;

    async fn load_registration_data(
        &self,
    ) -> Result<Option<presage::manager::RegistrationData>, Self::StateStoreError> {
        query_scalar!("SELECT value FROM kv WHERE key = 'registration'")
            .fetch_optional(&self.db)
            .await?
            .map(|value| serde_json::from_slice(&value))
            .transpose()
            .map_err(From::from)
    }

    async fn save_registration_data(
        &mut self,
        state: &presage::manager::RegistrationData,
    ) -> Result<(), Self::StateStoreError> {
        let value = serde_json::to_string(state)?;
        query!(
            "INSERT OR REPLACE INTO kv (key, value) VALUES ('registration', ?)",
            value
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn is_registered(&self) -> bool {
        self.load_registration_data().await.ok().flatten().is_some()
    }

    async fn clear_registration(&mut self) -> Result<(), Self::StateStoreError> {
        let mut transaction = self.begin_write().await.into_protocol_error()?;
        query!("DELETE FROM kv WHERE key = 'registration'")
            .execute(&mut *transaction)
            .await?;
        query!("DELETE FROM sessions")
            .execute(&mut *transaction)
            .await?;
        query!("DELETE FROM identities")
            .execute(&mut *transaction)
            .await?;
        query!("DELETE FROM pre_keys")
            .execute(&mut *transaction)
            .await?;
        query!("DELETE FROM signed_pre_keys")
            .execute(&mut *transaction)
            .await?;
        query!("DELETE FROM kyber_pre_keys")
            .execute(&mut *transaction)
            .await?;
        query!("DELETE FROM sender_keys")
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await.into_protocol_error()?;
        Ok(())
    }

    async fn set_aci_identity_key_pair(
        &self,
        key_pair: presage::libsignal_service::protocol::IdentityKeyPair,
    ) -> Result<(), Self::StateStoreError> {
        let key = IdentityType::Aci.identity_key_pair_key();
        let value = key_pair.serialize();
        query!(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)",
            key,
            value
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn set_pni_identity_key_pair(
        &self,
        key_pair: presage::libsignal_service::protocol::IdentityKeyPair,
    ) -> Result<(), Self::StateStoreError> {
        let key = IdentityType::Pni.identity_key_pair_key();
        let value = key_pair.serialize();
        query!(
            "INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)",
            key,
            value
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn sender_certificate(&self) -> Result<Option<SenderCertificate>, Self::StateStoreError> {
        query_scalar!("SELECT value FROM kv WHERE key = 'sender_certificate' LIMIT 1")
            .fetch_optional(&self.db)
            .await?
            .map(|value| SenderCertificate::deserialize(&value))
            .transpose()
            .map_err(From::from)
    }

    async fn save_sender_certificate(
        &self,
        certificate: &SenderCertificate,
    ) -> Result<(), Self::StateStoreError> {
        let value = certificate.serialized()?;
        query!(
            "INSERT OR REPLACE INTO kv (key, value) VALUES ('sender_certificate', ?)",
            value
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    async fn fetch_master_key(&self) -> Result<Option<MasterKey>, Self::StateStoreError> {
        query_scalar!("SELECT value FROM kv WHERE key = 'master_key' LIMIT 1")
            .fetch_optional(&self.db)
            .await?
            .map(|value| MasterKey::from_slice(&value))
            .transpose()
            .map_err(|_| SqliteStoreError::InvalidFormat)
    }

    async fn store_master_key(
        &self,
        master_key: Option<&MasterKey>,
    ) -> Result<(), Self::StateStoreError> {
        let value = master_key.map(|k| &k.inner[..]);
        if let Some(value) = value {
            query!(
                "INSERT OR REPLACE INTO kv (key, value) VALUES ('master_key', ?)",
                value
            )
            .execute(&self.db)
            .await?;
        } else {
            query!("DELETE FROM kv WHERE key = 'master_key'")
                .execute(&self.db)
                .await?;
        }
        Ok(())
    }

    async fn fetch_account_entropy_pool(
        &self,
    ) -> Result<Option<AccountEntropyPool>, Self::StateStoreError> {
        query_scalar!("SELECT value FROM kv WHERE key = 'account_entropy_pool' LIMIT 1")
            .fetch_optional(&self.db)
            .await?
            .map(|value| {
                AccountEntropyPool::from_str(
                    str::from_utf8(&value).map_err(|_| SqliteStoreError::InvalidFormat)?,
                )
                .map_err(|_| SqliteStoreError::InvalidFormat)
            })
            .transpose()
            .map_err(|_| SqliteStoreError::InvalidFormat)
    }

    async fn store_account_entropy_pool(
        &self,
        aep: Option<&AccountEntropyPool>,
    ) -> Result<(), Self::StateStoreError> {
        let value = aep.map(|k| k.to_string().into_bytes());
        if let Some(value) = value {
            query!(
                "INSERT OR REPLACE INTO kv (key, value) VALUES ('account_entropy_pool', ?)",
                value
            )
            .execute(&self.db)
            .await?;
        } else {
            query!("DELETE FROM kv WHERE key = 'account_entropy_pool'")
                .execute(&self.db)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_verified_contacts_changed_key_is_refused_for_sending() {
        let keys = VerifiedKeys::default();
        keys.replace([("alice".to_owned(), vec![1, 2, 3])].into());
        assert!(keys.allows_sending("alice", &[1, 2, 3]));
        assert!(keys.allows_sending("bob", &[9, 9]), "not verified: anything goes");
        assert!(!keys.allows_sending("alice", &[4, 5, 6]));
        assert_eq!(keys.take_refused(), vec![("alice".to_owned(), vec![4, 5, 6])]);
        assert!(keys.take_refused().is_empty());
        // Clones share the state, as the store's clones do.
        keys.clone().replace(Default::default());
        assert!(keys.allows_sending("alice", &[4, 5, 6]));
    }

    use super::*;

    #[tokio::test]
    async fn close_closes_the_pool() {
        let store = SqliteStore::open(":memory:", OnNewIdentity::Trust)
            .await
            .unwrap();
        let clone = store.clone();

        store.close().await;

        assert!(clone.db.is_closed());
    }
}
