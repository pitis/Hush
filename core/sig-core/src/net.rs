//! An authenticated `PushService` from presage's registration, shared by the history fetcher
//! and the calling-relays fetch.

use presage::libsignal_service::prelude::PushService;

/// Authenticated with this device's credentials. presage keeps the account password private,
/// but `RegistrationData` serializes it — that is how presage itself persists it.
pub(crate) fn push_service(data: &presage::manager::RegistrationData) -> Result<PushService, String> {
    use presage::libsignal_service::configuration::{ServiceCredentials, SignalServers};
    use presage::libsignal_service::prelude::phonenumber;
    use presage::libsignal_service::protocol::E164;
    use std::str::FromStr as _;
    let v = serde_json::to_value(data).map_err(|e| e.to_string())?;
    let password = v["password"].as_str().ok_or("no account password")?.to_owned();
    let e164 = data.phone_number.format().mode(phonenumber::Mode::E164).to_string();
    let credentials = ServiceCredentials {
        aci: Some(data.service_ids.aci),
        pni: Some(data.service_ids.pni),
        phonenumber: E164::from_str(&e164).map_err(|e| format!("phone number: {e}"))?,
        password: Some(password),
        device_id: data.device_id.and_then(|d| d.try_into().ok()),
    };
    Ok(PushService::new(
        SignalServers::Production,
        Some(credentials),
        "Semaphore",
    ))
}
