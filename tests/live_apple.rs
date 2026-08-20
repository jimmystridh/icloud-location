use std::path::PathBuf;

use icloud_location::{AuthenticationStatus, ClientBuilder, LocateOptions};

/// Opt-in smoke test for an authorized Apple account.
///
/// Run explicitly with `cargo test --test live_apple -- --ignored` after setting
/// `ICLOUD_LIVE_USERNAME`, `ICLOUD_LIVE_PASSWORD`, and `ICLOUD_LIVE_SESSION_ROOT`.
#[tokio::test]
#[ignore = "requires an explicitly configured authorized live Apple account"]
async fn authenticates_and_reads_find_my_without_exposing_credentials() {
    let username = std::env::var("ICLOUD_LIVE_USERNAME")
        .expect("set ICLOUD_LIVE_USERNAME for the opt-in live test");
    let password = std::env::var("ICLOUD_LIVE_PASSWORD")
        .expect("set ICLOUD_LIVE_PASSWORD for the opt-in live test");
    let session_root = PathBuf::from(
        std::env::var("ICLOUD_LIVE_SESSION_ROOT")
            .expect("set ICLOUD_LIVE_SESSION_ROOT for the opt-in live test"),
    );
    let mut client = ClientBuilder::new(username)
        .password(password)
        .session_root(session_root)
        .build()
        .expect("construct live Apple client");

    let status = client
        .authenticate()
        .await
        .expect("authenticate live account");
    assert!(
        matches!(status, AuthenticationStatus::Authenticated(_)),
        "live account must already be trusted; complete 2FA with the CLI first"
    );
    client
        .locate_devices(LocateOptions::family())
        .await
        .expect("read Find My devices");
}
