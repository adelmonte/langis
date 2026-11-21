//! Why linking failed, sorted the way Signal Desktop's install screen sorts
//! it (`ts/state/ducks/installer.preload`, `ts/types/InstallScreen.std`).

use presage::libsignal_service::prelude::ServiceError;
use presage::libsignal_service::provisioning::ProvisioningError;

/// The kind of a link failure, for `Event::LinkFailed`. Desktop shows the
/// first three in the QR code's box with a way to retry, and the rest on a
/// full error screen ("Something went wrong!").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkError {
    /// No QR code came in time (Desktop's `InstallScreenQRCodeError.Timeout`:
    /// "The QR code couldn't load. Check your connection and try again.").
    Timeout,
    /// The code went unscanned until the server closed it
    /// (`InstallScreenQRCodeError.MaxRotations`: only "Refresh code").
    Expired,
    /// Anything else after the code showed (`InstallScreenQRCodeError.Unknown`).
    Unknown,
    /// The server couldn't be reached for a code
    /// (`InstallScreenError.ConnectionFailed`: "Failed to connect to server.").
    ConnectionFailed,
    /// The account has as many linked devices as it may (HTTP 411,
    /// `InstallScreenError.TooManyDevices`).
    TooManyDevices,
    /// The server wants a newer client (HTTP 409, a capability this client
    /// lacks; `InstallScreenError.TooOld`).
    TooOld,
}

/// Sorts a failed `Manager::link_secondary_device`. `had_url` says whether
/// a QR code had been shown: before it, a network error means the server
/// couldn't be reached; after it, Desktop calls anything but 409 and 411
/// unknown.
pub(crate) fn classify<S: std::error::Error>(error: &presage::Error<S>, had_url: bool) -> LinkError {
    let service = match error {
        presage::Error::ServiceError(e) | presage::Error::ProvisioningError(ProvisioningError::ServiceError(e)) => Some(e),
        _ => None,
    };
    match service {
        Some(ServiceError::DeviceLimitReached { .. }) => return LinkError::TooManyDevices,
        Some(ServiceError::DeviceCapabilityDowngrade | ServiceError::DeprecatedVersion) => return LinkError::TooOld,
        _ => {}
    }
    if !had_url {
        return match service {
            Some(ServiceError::Timeout { .. }) => LinkError::Timeout,
            Some(_) => LinkError::ConnectionFailed,
            None => match error {
                presage::Error::ProvisioningError(ProvisioningError::WsError { .. } | ProvisioningError::WsClosing | ProvisioningError::MissingUrl) => LinkError::ConnectionFailed,
                _ => LinkError::Unknown,
            },
        };
    }
    match error {
        // The provisioning socket closed with no scan: the code expired.
        presage::Error::NoProvisioningMessageReceived
        | presage::Error::ProvisioningError(ProvisioningError::MissingMessage | ProvisioningError::WsClosing) => LinkError::Expired,
        _ => LinkError::Unknown,
    }
}

/// What a new link means for the data here, by whose it is (Note to
/// Self's service id in the app database) and the linked account's ACI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relink {
    /// No account's data here: a clean start (Desktop's `isCleanStart`).
    Fresh,
    /// The same account again (`isRelinkingToSameAccount`): Desktop keeps
    /// the data and erases only the configuration.
    SameAccount,
    /// Another account: Desktop asks before deleting all data.
    OtherAccount,
}

pub(crate) fn relink(previous: Option<&str>, new_aci: &str) -> Relink {
    match previous {
        None => Relink::Fresh,
        Some(previous) if previous == new_aci => Relink::SameAccount,
        Some(_) => Relink::OtherAccount,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relink_keeps_the_data_only_for_the_same_account() {
        assert_eq!(relink(None, "a"), Relink::Fresh);
        assert_eq!(relink(Some("a"), "a"), Relink::SameAccount);
        assert_eq!(relink(Some("a"), "b"), Relink::OtherAccount);
    }

    type E = presage::Error<std::io::Error>;

    fn service(e: ServiceError) -> E {
        presage::Error::ProvisioningError(ProvisioningError::ServiceError(e))
    }

    #[test]
    fn sorts_as_desktop() {
        assert_eq!(classify(&service(ServiceError::DeviceLimitReached { current: 5, max: 5 }), true), LinkError::TooManyDevices);
        assert_eq!(classify(&service(ServiceError::DeviceCapabilityDowngrade), true), LinkError::TooOld);
        assert_eq!(classify(&service(ServiceError::Timeout { reason: "x" }), false), LinkError::Timeout);
        assert_eq!(classify(&service(ServiceError::SendError { reason: "x".into() }), false), LinkError::ConnectionFailed);
        assert_eq!(classify(&E::NoProvisioningMessageReceived, true), LinkError::Expired);
        assert_eq!(classify(&E::ProvisioningError(ProvisioningError::MissingMessage), true), LinkError::Expired);
        // After the code showed, a network error is Desktop's unknown one.
        assert_eq!(classify(&service(ServiceError::SendError { reason: "x".into() }), true), LinkError::Unknown);
        assert_eq!(classify(&E::ProvisioningError(ProvisioningError::MismatchedMac), true), LinkError::Unknown);
    }
}
