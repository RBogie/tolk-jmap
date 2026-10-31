//! Provision (MS-ASPROV): the two-phase policy handshake.
//!
//! Phase 1: the device asks for the policy and gets a temporary key plus the
//! policy document. Phase 2: it acknowledges with that key and receives the
//! final key, which it then sends as `X-MS-PolicyKey` on every command.
//! The policy is fully permissive: we demand nothing of the device. A wipe
//! requested by the administrator is handed out here.

use super::{CommandError, CommandResult, Request, settings};
use crate::wbxml::{
    Element, Tag,
    cp::{provision, settings as settings_cp},
};

const POLICY_TYPE: &str = "MS-EAS-Provisioning-WBXML";

/// MS-ASPROV policy status values.
mod policy_status {
    pub const OK: &str = "1";
    pub const UNKNOWN_POLICY_TYPE: &str = "3";
    pub const WRONG_POLICY_KEY: &str = "5";
}

/// Command status telling the client to provision (again).
pub const DEVICE_NOT_PROVISIONED: &str = "142";
/// Command status telling the client to provision for a remote wipe.
pub const REMOTE_WIPE_REQUESTED: &str = "140";

const POLICY: &[(Tag, &str)] = &[
    (provision::DevicePasswordEnabled, "0"),
    (provision::AlphanumericDevicePasswordRequired, "0"),
    (provision::PasswordRecoveryEnabled, "0"),
    (provision::RequireStorageCardEncryption, "0"),
    (provision::RequireDeviceEncryption, "0"),
    (provision::AttachmentsEnabled, "1"),
    (provision::AllowSimpleDevicePassword, "1"),
    (provision::AllowStorageCard, "1"),
    (provision::AllowCamera, "1"),
    (provision::AllowWiFi, "1"),
    (provision::AllowTextMessaging, "1"),
    (provision::AllowPOPIMAPEmail, "1"),
    (provision::AllowBluetooth, "2"),
    (provision::AllowIrDA, "1"),
    (provision::RequireManualSyncWhenRoaming, "0"),
    (provision::AllowDesktopSync, "1"),
    (provision::AllowHTMLEmail, "1"),
    (provision::AllowSMIMESoftCerts, "1"),
    (provision::AllowBrowser, "1"),
    (provision::AllowConsumerEmail, "1"),
    (provision::AllowRemoteDesktop, "1"),
    (provision::AllowInternetSharing, "1"),
    (provision::AllowUnsignedApplications, "1"),
    (provision::AllowUnsignedInstallationPackages, "1"),
];

pub fn handle(req: &Request) -> CommandResult {
    let body = req.body_with_root(provision::Provision)?;
    let mut response = Element::new(provision::Provision);

    if let Some(info) = body.find(settings_cp::DeviceInformation) {
        settings::store_device_info(req, info)?;
        response.push(
            Element::new(settings_cp::DeviceInformation)
                .child(Element::with_text(settings_cp::Status, "1")),
        );
    }
    response.push(Element::with_text(provision::Status, "1"));

    // The device confirms it wiped itself (or the account): it is done.
    if body.find(provision::RemoteWipe).is_some()
        || body.find(provision::AccountOnlyRemoteWipe).is_some()
    {
        req.store
            .set_device_status(req.user, req.device_id, "wiped")?;
        tracing::warn!(
            user = req.user,
            device = req.device_id,
            "device confirmed the remote wipe"
        );
        return Ok(response);
    }
    // A wipe requested with `tolk devices wipe`.
    let wipe = match req.store.device_status(req.user, req.device_id)?.as_str() {
        "wipe" => Some(provision::RemoteWipe),
        "account-wipe" => Some(provision::AccountOnlyRemoteWipe),
        _ => None,
    };
    if let Some(tag) = wipe {
        tracing::warn!(user = req.user, device = req.device_id, %tag, "sending a remote wipe");
        response.push(Element::new(tag));
        return Ok(response);
    }

    let policy = body
        .find(provision::Policies)
        .and_then(|p| p.find(provision::Policy))
        .ok_or_else(|| CommandError::bad("Provision without Policy"))?;
    let policy_type = policy.child_text(provision::PolicyType).unwrap_or_default();
    let mut out = Element::new(provision::Policy)
        .child(Element::with_text(provision::PolicyType, policy_type));

    if policy_type != POLICY_TYPE {
        out.push(Element::with_text(
            provision::Status,
            policy_status::UNKNOWN_POLICY_TYPE,
        ));
    } else if let Some(acked_key) = policy.child_text(provision::PolicyKey) {
        let pending = req
            .store
            .device_policy(req.user, req.device_id)?
            .and_then(|p| p.pending_policy_key);
        if pending.is_some_and(|key| acked_key.parse() == Ok(key)) {
            let key = new_policy_key();
            req.store.confirm_policy_key(req.user, req.device_id, key)?;
            tracing::info!(
                user = req.user,
                device = req.device_id,
                "device provisioned"
            );
            out.push(Element::with_text(provision::Status, policy_status::OK));
            out.push(Element::with_text(provision::PolicyKey, key.to_string()));
        } else {
            out.push(Element::with_text(
                provision::Status,
                policy_status::WRONG_POLICY_KEY,
            ));
        }
    } else {
        let key = new_policy_key();
        req.store
            .set_pending_policy_key(req.user, req.device_id, key)?;
        let document = POLICY.iter().fold(
            Element::new(provision::EASProvisionDoc),
            |doc, &(tag, value)| doc.child(Element::with_text(tag, value)),
        );
        out.push(Element::with_text(provision::Status, policy_status::OK));
        out.push(Element::with_text(provision::PolicyKey, key.to_string()));
        out.push(Element::new(provision::Data).child(document));
    }

    response.push(Element::new(provision::Policies).child(out));
    Ok(response)
}

/// Returns the "not provisioned" response for a command when the device did
/// not send its current policy key, or `None` when it may proceed.
pub fn check(req: &Request, root: Tag, status: Tag) -> Result<Option<Element>, CommandError> {
    let expected = req
        .store
        .device_policy(req.user, req.device_id)?
        .and_then(|p| p.policy_key);
    if expected.is_some() && expected == req.policy_key {
        return Ok(None);
    }
    Ok(Some(
        Element::new(root).child(Element::with_text(status, DEVICE_NOT_PROVISIONED)),
    ))
}

/// A random policy key (it only has to differ between handshakes): the
/// standard library's randomly seeded hasher over the current time.
fn new_policy_key() -> u32 {
    use std::hash::BuildHasher;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (std::collections::hash_map::RandomState::new().hash_one(now) as u32).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        store::Store,
        wbxml::{self, cp::folder_hierarchy},
    };

    fn fixture() -> Element {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/gmail-android/14.1-provision-initial.req.wbxml"
        ))
        .unwrap();
        wbxml::decode(&bytes).unwrap()
    }

    fn ack(key: &str) -> Element {
        Element::new(provision::Provision).child(
            Element::new(provision::Policies).child(
                Element::new(provision::Policy)
                    .child(Element::with_text(provision::PolicyType, POLICY_TYPE))
                    .child(Element::with_text(provision::PolicyKey, key))
                    .child(Element::with_text(provision::Status, "1")),
            ),
        )
    }

    fn policy(response: &Element) -> &Element {
        response
            .find(provision::Policies)
            .unwrap()
            .find(provision::Policy)
            .unwrap()
    }

    #[test]
    fn two_phase_handshake() {
        let store = Store::open_in_memory().unwrap();
        store.touch_device("rob", "d1", None).unwrap();
        let mut req = Request::for_tests(&store, Some(fixture()));

        // Not provisioned yet: commands are refused.
        let denied = check(&req, folder_hierarchy::FolderSync, folder_hierarchy::Status).unwrap();
        assert_eq!(
            denied.unwrap().child_text(folder_hierarchy::Status),
            Some(DEVICE_NOT_PROVISIONED)
        );

        // Phase 1 returns a temporary key and the policy document.
        let first = handle(&req).unwrap();
        assert_eq!(first.child_text(provision::Status), Some("1"));
        assert!(first.find(settings_cp::DeviceInformation).is_some());
        let temp_key = policy(&first)
            .child_text(provision::PolicyKey)
            .unwrap()
            .to_owned();
        assert!(policy(&first).find(provision::Data).is_some());

        // A wrong acknowledgement is rejected.
        req.body = Some(ack("12345"));
        let wrong = handle(&req).unwrap();
        assert_eq!(policy(&wrong).child_text(provision::Status), Some("5"));

        // Phase 2 returns the final key, which unlocks commands.
        req.body = Some(ack(&temp_key));
        let second = handle(&req).unwrap();
        assert_eq!(policy(&second).child_text(provision::Status), Some("1"));
        assert!(policy(&second).find(provision::Data).is_none());
        let key: u32 = policy(&second)
            .child_text(provision::PolicyKey)
            .unwrap()
            .parse()
            .unwrap();

        req.policy_key = Some(key);
        assert!(
            check(&req, folder_hierarchy::FolderSync, folder_hierarchy::Status)
                .unwrap()
                .is_none()
        );
        req.policy_key = Some(key.wrapping_add(1));
        assert!(
            check(&req, folder_hierarchy::FolderSync, folder_hierarchy::Status)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn unknown_policy_type() {
        let store = Store::open_in_memory().unwrap();
        store.touch_device("rob", "d1", None).unwrap();
        let body = Element::new(provision::Provision).child(
            Element::new(provision::Policies).child(Element::new(provision::Policy).child(
                Element::with_text(provision::PolicyType, "MS-WAP-Provisioning-XML"),
            )),
        );
        let req = Request::for_tests(&store, Some(body));
        let response = handle(&req).unwrap();
        assert_eq!(policy(&response).child_text(provision::Status), Some("3"));
    }
}
