//! Real client traffic must decode and re-encode to the identical bytes.

use std::{fs, path::Path};

use tolk::wbxml::{
    self, Element,
    cp::{folder_hierarchy, provision, settings},
};

fn fixture(name: &str) -> Element {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    wbxml::decode(&fs::read(&path).unwrap()).unwrap()
}

#[test]
fn all_fixtures_round_trip() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut count = 0;
    for client in fs::read_dir(&root).unwrap() {
        for file in fs::read_dir(client.unwrap().path()).unwrap() {
            let path = file.unwrap().path();
            if path.extension().is_none_or(|ext| ext != "wbxml") {
                continue;
            }
            let bytes = fs::read(&path).unwrap();
            let decoded =
                wbxml::decode(&bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(wbxml::encode(&decoded), bytes, "{}", path.display());
            count += 1;
        }
    }
    assert!(count >= 2, "expected fixtures under {}", root.display());
}

#[test]
fn gmail_initial_provision() {
    let root = fixture("gmail-android/14.1-provision-initial.req.wbxml");
    assert_eq!(root.tag, provision::Provision);

    let device = root
        .find(settings::DeviceInformation)
        .and_then(|d| d.find(settings::Set))
        .unwrap();
    assert_eq!(
        device.child_text(settings::Model),
        Some("sdk_gphone64_arm64")
    );
    assert_eq!(device.child_text(settings::OS), Some("Android 17"));
    assert!(
        device
            .child_text(settings::UserAgent)
            .unwrap()
            .starts_with("Android-Mail/")
    );

    let policy_type = root
        .find(provision::Policies)
        .and_then(|p| p.find(provision::Policy))
        .and_then(|p| p.child_text(provision::PolicyType));
    assert_eq!(policy_type, Some("MS-EAS-Provisioning-WBXML"));
}

#[test]
fn gmail_initial_folder_sync() {
    let root = fixture("gmail-android/14.1-foldersync-initial.req.wbxml");
    assert_eq!(root.tag, folder_hierarchy::FolderSync);
    assert_eq!(root.child_text(folder_hierarchy::SyncKey), Some("0"));
}
