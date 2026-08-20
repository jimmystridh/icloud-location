use std::path::Path;

use icloud_location::core::{ExternalLocationUpdate, LocationSourceKind};
use serde_json::Value;

fn fixture(relative_path: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(relative_path);
    let contents = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(&contents).unwrap()
}

#[test]
fn find_my_fixture_contains_owner_and_family_devices() {
    let fixture = fixture("apple/find_my_refresh.json");
    let devices = fixture["content"].as_array().unwrap();

    assert_eq!(devices.len(), 2);
    assert!(devices.iter().any(|device| device["fmlyShare"] == false));
    assert!(devices.iter().any(|device| device["fmlyShare"] == true));
    assert!(devices.iter().any(|device| device["location"].is_null()));
}

#[test]
fn interval_fixture_covers_short_and_long_polling() {
    let fixture = fixture("tracking/interval_cases.json");
    let seconds: Vec<_> = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|case| case["expected_seconds"].as_u64().unwrap())
        .collect();

    assert!(seconds.contains(&15));
    assert!(seconds.contains(&3600));
}

#[test]
fn waze_fixture_has_realtime_and_historical_expectations() {
    let fixture = fixture("waze/route_response.json");

    assert_eq!(fixture["expected"]["realtime_time_minutes"], 3.5);
    assert!(
        fixture["expected"]["historical_time_minutes"]
            .as_f64()
            .unwrap()
            > 4.0
    );
}

#[test]
fn external_fixture_is_directly_accepted_by_the_public_input_schema() {
    let fixture = fixture("external/location_updates.json");
    let updates: Vec<ExternalLocationUpdate> = serde_json::from_value(fixture).unwrap();

    assert_eq!(updates.len(), 2);
    assert!(matches!(
        updates[0].sample.source,
        LocationSourceKind::External(ref source) if source == "example_phone_bridge"
    ));
}

#[test]
fn fixtures_do_not_contain_known_private_account_data() {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pending = vec![fixture_root];
    while let Some(path) = pending.pop() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let contents = std::fs::read_to_string(path).unwrap().to_lowercase();
                assert!(!contents.contains("@stridh"));
                assert!(!contents.contains("jimmy-apple"));
                assert!(!contents.contains("x-apple-session-token"));
            }
        }
    }
}
