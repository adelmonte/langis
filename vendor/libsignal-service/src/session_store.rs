use async_trait::async_trait;
use libsignal_core::DeviceId;
use libsignal_protocol::{
    ProtocolAddress, ServiceId, SessionStore, SignalProtocolError,
};

use crate::{content::ServiceError, push_service::DEFAULT_DEVICE_ID};

/// Archives the active session with `address` (langis), as Signal Desktop
/// does for a device the server calls stale (410) or extra (409)
/// (`handle409Response`, `handle410Response`): the record and its previous
/// states stay, so messages already on their way from that device still
/// decrypt, and the next send starts a new session. Deleting the record
/// lost those.
pub async fn archive_device_session<S: SessionStore + ?Sized>(
    store: &mut S,
    address: &ProtocolAddress,
) -> Result<(), SignalProtocolError> {
    if let Some(mut record) = store.load_session(address).await? {
        record.archive_current_state()?;
        store.store_session(address, &record).await?;
    }
    Ok(())
}

/// This is additional functions required to handle
/// session deletion. It might be a candidate for inclusion into
/// the bigger `SessionStore` trait.
#[async_trait(?Send)]
pub trait SessionStoreExt: SessionStore {
    /// Get the IDs of all known sub devices with active sessions for a recipient.
    ///
    /// This should return every device except for the main device [DEFAULT_DEVICE_ID].
    async fn get_sub_device_sessions(
        &self,
        name: &ServiceId,
    ) -> Result<Vec<DeviceId>, SignalProtocolError>;

    /// Remove a session record for a recipient ID + device ID tuple.
    async fn delete_session(
        &self,
        address: &ProtocolAddress,
    ) -> Result<(), SignalProtocolError>;

    /// Remove the session records corresponding to all devices of a recipient
    /// ID.
    ///
    /// Returns the number of deleted sessions.
    async fn delete_all_sessions(
        &self,
        address: &ServiceId,
    ) -> Result<usize, SignalProtocolError>;

    /// Remove a session record for a recipient ID + device ID tuple.
    async fn delete_service_addr_device_session(
        &self,
        address: &ProtocolAddress,
    ) -> Result<usize, SignalProtocolError> {
        let mut count = 0;
        match self.delete_session(address).await {
            Ok(()) => {
                count += 1;
            },
            Err(SignalProtocolError::SessionNotFound(_)) => (),
            Err(e) => return Err(e),
        }

        Ok(count)
    }

    async fn compute_safety_number(
        &self,
        local_address: &ServiceId,
        address: &ServiceId,
    ) -> Result<String, ServiceError>
    where
        Self: Sized + libsignal_protocol::IdentityKeyStore,
    {
        let addr = crate::cipher::get_preferred_protocol_address(
            self,
            address,
            *DEFAULT_DEVICE_ID,
        )
        .await?;
        let ident = self
            .get_identity(&addr)
            .await?
            .ok_or(SignalProtocolError::UntrustedIdentity(addr))?;
        let local = self
            .get_identity_key_pair()
            .await
            .expect("valid local identity");
        let fp = libsignal_protocol::Fingerprint::new(
            2,
            5200,
            local_address.raw_uuid().as_bytes(),
            local.identity_key(),
            address.raw_uuid().as_bytes(),
            &ident,
        )?;
        Ok(fp.display_string()?)
    }
}

#[cfg(test)]
mod tests {
    use libsignal_protocol::{
        kem, process_prekey_bundle, DeviceId, IdentityKeyPair,
        InMemSignalProtocolStore, KeyPair, KyberPreKeyId, PreKeyBundle,
        ProtocolAddress, SessionStore, SessionUsabilityRequirements,
        SignedPreKeyId,
    };
    use std::time::SystemTime;

    use super::archive_device_session;

    /// Archiving keeps the record (and so the states messages already on
    /// their way decrypt with) but leaves no session to send on.
    #[tokio::test]
    async fn archiving_keeps_the_record_without_a_usable_session() {
        let mut rng = rand::rng();
        let alice = IdentityKeyPair::generate(&mut rng);
        let mut store = InMemSignalProtocolStore::new(alice, 1).unwrap();
        let bob = IdentityKeyPair::generate(&mut rng);
        let signed = KeyPair::generate(&mut rng);
        let kyber = kem::KeyPair::generate(kem::KeyType::Kyber1024, &mut rng);
        let bundle = PreKeyBundle::new(
            2,
            DeviceId::new(1).unwrap(),
            None,
            SignedPreKeyId::from(1),
            signed.public_key,
            bob.private_key()
                .calculate_signature(&signed.public_key.serialize(), &mut rng)
                .unwrap()
                .to_vec(),
            KyberPreKeyId::from(1),
            kyber.public_key.clone(),
            bob.private_key()
                .calculate_signature(&kyber.public_key.serialize(), &mut rng)
                .unwrap()
                .to_vec(),
            *bob.identity_key(),
        )
        .unwrap();
        let address = ProtocolAddress::new(
            "22222222-2222-4222-8222-222222222222".into(),
            DeviceId::new(1).unwrap(),
        );
        let local = ProtocolAddress::new(
            "11111111-1111-4111-8111-111111111111".into(),
            DeviceId::new(1).unwrap(),
        );
        process_prekey_bundle(
            &address,
            &local,
            &mut store.session_store,
            &mut store.identity_store,
            &bundle,
            SystemTime::now(),
            &mut rng,
        )
        .await
        .unwrap();
        let usable = |record: &libsignal_protocol::SessionRecord| {
            record
                .has_usable_sender_chain(
                    SystemTime::now(),
                    SessionUsabilityRequirements::NotStale,
                )
                .unwrap()
        };
        assert!(usable(&store.load_session(&address).await.unwrap().unwrap()));

        archive_device_session(&mut store, &address).await.unwrap();
        let record = store.load_session(&address).await.unwrap().expect("the record stays");
        assert!(!usable(&record));
        assert!(!record.serialize().unwrap().is_empty(), "the archived state stays");

        // No record at all: nothing to do, and no error.
        let other = ProtocolAddress::new(
            "33333333-3333-4333-8333-333333333333".into(),
            DeviceId::new(1).unwrap(),
        );
        archive_device_session(&mut store, &other).await.unwrap();
        assert!(store.load_session(&other).await.unwrap().is_none());
    }
}
