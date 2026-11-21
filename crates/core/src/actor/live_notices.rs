//! Timeline notices Signal Desktop makes as things happen, which langis
//! otherwise only had from transferred history:
//!
//! - the default timer notice in a new chat (`universal-timer-notification`,
//!   `maybeSetPendingUniversalTimer` in `ts/models/conversations.preload.ts`);
//! - merged chats (`conversation-merge`, `combineConversations` in
//!   `ts/ConversationController.preload.ts`) and "number belongs to"
//!   (`phone-number-discovery`, `addPhoneNumberDiscoveryIfNeeded`), when a
//!   chat we started with someone's phone number (their PNI, all a number
//!   lookup gives) turns out to be theirs.
//!
//! Desktop makes "joined Signal" only from transferred history, and a GV1
//! migration notice only for a GV1 group it has locally (langis has none),
//! so neither is made here.

use presage::libsignal_service::protocol::SessionStore as _;

use super::*;

impl Actor {
    /// Adds a notice to `conversation` now, emitting `MessageChanged`.
    async fn add_live_notice(&self, conversation: &ConversationId, notice: &langis_store::Notice) {
        let at = now_millis();
        // Notices dedupe by time: another one in this millisecond would
        // swallow this one.
        for offset in 0..10 {
            match self.db.insert_notice(conversation, at + offset, notice).await {
                Ok(Some(id)) => {
                    return self.emit(Event::MessageChanged { conversation: conversation.clone(), message_id: id });
                }
                Ok(None) => continue,
                Err(error) => return self.error("failed to add notice", error),
            }
        }
    }

    /// Desktop's `maybeSetPendingUniversalTimer`, run when a chat is opened
    /// (Desktop runs it on `updateLastMessage`): a 1:1 chat (not the Signal
    /// chat) with no message anyone wrote, no timer and no such notice gets
    /// "The disappearing message time will be set to ... when you message
    /// them." With messages, the notice goes. Only chats that have a row
    /// get it: making one would put an empty chat in the chat list, which
    /// Desktop's doesn't show.
    pub(super) async fn maybe_set_pending_universal_timer(&self, conversation: &ConversationId) {
        if conversation.as_direct().is_none() || conversation.is_signal() || self.is_note_to_self(conversation) {
            return;
        }
        let Ok(Some(chat)) = self.db.conversation(conversation).await else { return };
        if self.db.has_user_messages(conversation).await.unwrap_or(true) {
            return self.remove_universal_timer_notices(conversation).await;
        }
        let seconds = self.universal_expire_timer().await;
        if chat.expire_timer != 0 || seconds == 0 {
            return;
        }
        if !self.db.universal_timer_notices(conversation).await.unwrap_or_default().is_empty() {
            return;
        }
        info!("adding the default timer notice to a new chat");
        self.add_live_notice(conversation, &langis_store::Notice::UniversalTimer { seconds }).await;
    }

    /// Desktop's `maybeRemoveUniversalTimer`: the chat's default timer
    /// notice goes once a message is sent or received there.
    pub(super) async fn remove_universal_timer_notices(&self, conversation: &ConversationId) {
        for id in self.db.universal_timer_notices(conversation).await.unwrap_or_default() {
            match self.db.remove_message(id).await {
                Ok(_) => self.emit(Event::MessageRemoved { conversation: conversation.clone(), message_id: id }),
                Err(error) => warn!(%error, "couldn't remove the default timer notice"),
            }
        }
    }

    /// We learned that `pni` is `aci`'s: from a PNI signature on their
    /// message (`from_signature`), or a Storage Service contact record. A
    /// chat we had with the PNI (from a phone number lookup) becomes their
    /// chat, as Desktop's `maybeMergeContacts` makes it:
    ///
    /// - When we also had a chat with their ACI and both had messages,
    ///   "Your message history with ... and their number ... has been
    ///   merged." (`combineConversations`; `phone` is the PNI chat's number).
    /// - When only the PNI chat was there and we have a session with the
    ///   PNI, "+1 555 ... belongs to ...", unless a PNI signature told us
    ///   (`addPhoneNumberDiscoveryIfNeeded`, skipped `fromPniSignature`).
    pub(super) async fn adopt_pni_chat(&self, pni: &str, aci: &str, from_signature: bool) {
        let obsolete = ConversationId::direct(pni);
        let current = ConversationId::direct(aci);
        let Ok(Some(old)) = self.db.conversation(&obsolete).await else { return };
        let current_existed = self.db.conversation(&current).await.ok().flatten().is_some();
        // Desktop's `hasSessionWith(originalPni)`, read before the merge.
        let had_session = !current_existed && !from_signature && self.has_session_with(pni).await;
        let (obsolete_had, current_had) = match self.db.merge_conversation(&obsolete, &current).await {
            Ok(Some(had)) => had,
            Ok(None) => return,
            Err(error) => return self.error("failed to merge chats", error),
        };
        info!(current_existed, "moved a phone number's chat into its account's chat");
        self.emit(Event::ConversationRemoved(obsolete));
        self.emit(Event::ConversationsChanged);
        if current_existed {
            if obsolete_had && current_had {
                self.add_live_notice(&current, &langis_store::Notice::Merged { who: aci.to_owned(), phone: old.phone.clone() }).await;
            }
        } else if had_session && let Some(phone) = old.phone.clone() {
            self.add_live_notice(&current, &langis_store::Notice::NumberIsTheirs { who: aci.to_owned(), phone }).await;
        }
    }

    /// Whether we have a session with `service_id`'s first device.
    async fn has_session_with(&self, service_id: &str) -> bool {
        let address = ProtocolAddress::new(service_id.to_owned(), *presage::libsignal_service::push_service::DEFAULT_DEVICE_ID);
        let store = presage::store::Store::aci_protocol_store(&self.store);
        matches!(store.load_session(&address).await, Ok(Some(_)))
    }
}
