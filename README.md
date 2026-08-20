# icloud-location

[![CI](https://github.com/jimmystridh/icloud-location/actions/workflows/ci.yml/badge.svg)](https://github.com/jimmystridh/icloud-location/actions/workflows/ci.yml)

`icloud-location` is a standalone Rust library and CLI that ports iCloud3's
non-Home-Assistant behavior. It combines Apple Find My access with durable
multi-account tracking, dynamic intervals, zones, stationary and nearby-device
policy, generic external GPS ingestion, and optional Waze routing.

Apple and Waze do not publish or support the web interfaces used here. They can
change without notice. Use this software only with accounts and devices you are
authorized to access.

## Workspace

- `icloud-findmy`: Apple SRP, trusted sessions, 2FA, security keys, devices, and
  Find My actions.
- `icloud-tracking`: pure location-quality, interval, zone, stationary, nearby,
  and source-arbitration policy.
- `icloud-routing`: provider-neutral route and route-history interfaces plus a
  straight-line fallback.
- `icloud-waze`: optional Waze provider and private `SQLite` route history.
- `icloud-location`: public facade, standalone scheduler, configuration, and
  CLI.

Typed snapshots, events, TOML, and Rust interfaces keep protocol access,
tracking policy, routing, and presentation concerns separate.

## Build

```console
cargo build --release
cargo build --release --features waze,security-key
```

The binary is `target/release/icloud-location`. Waze and USB FIDO2 support are
independent opt-in features; tracking does not depend on either.

Install the CLI directly from GitHub:

```console
cargo install --git https://github.com/jimmystridh/icloud-location --locked
```

Add `--features waze,security-key` to include both optional integrations.

## Apple session and Find My

```console
export ICLOUD_USERNAME='name@example.com'
icloud-location login
icloud-location login --sms-phone-id 2
```

The CLI reads `ICLOUD_PASSWORD` or prompts without echo. It never persists the
password. Tokens and cookies are stored per account using a hashed directory
name, atomic replacement, and owner-only Unix permissions. Override the root
with `--session-root` or `ICLOUD_SESSION_ROOT`.

Server applications can keep the same authentication state outside the local
filesystem by using a portable in-memory session:

```rust,no_run
use icloud_location::{ClientBuilder, PortableSession};

# fn example(encrypted_storage_payload: Vec<u8>) -> icloud_location::Result<()> {
# let decrypted_archive = encrypted_storage_payload;
let session = PortableSession::from_bytes(decrypted_archive)?;
let client = ClientBuilder::new("name@example.com")
    .portable_session(session)
    .build()?;
let updated_archive = client.export_portable_session()?;
# let _bytes_to_encrypt = updated_archive.as_bytes();
# Ok(())
# }
```

`ClientBuilder::in_memory()` starts without an existing archive. Portable
archives are bounded and bound to the normalized account username, include the
complete cookie store, and never include the configured password. The account
binding prevents accidental cross-account reuse; it does not authenticate the
archive. Archives contain sensitive tokens in plaintext, so the embedding
application must protect them with authenticated encryption before durable
storage and must not log their bytes. The type's `Debug` output is redacted and
its owned byte buffer is zeroized when dropped.

When built with `security-key`, a connected FIDO2 USB HID authenticator can be
used with `login --security-key`.

Useful session operations are:

```console
icloud-location session validate
icloud-location session validate-credentials
icloud-location session refresh
icloud-location session trust-status
icloud-location session untrust
icloud-location session reset
icloud-location session accept-terms --confirm
```

Device reads and actions are available without the tracking daemon:

```console
icloud-location devices
icloud-location locate
icloud-location locate "Jimmy's iPhone"
icloud-location sound DEVICE_ID
icloud-location message DEVICE_ID --message 'Please call me' --sound
icloud-location lost-mode DEVICE_ID --phone-number '+46000000000' --confirm
```

Family-wide reads are the default. Add `--owner-only` to `devices` or `locate`,
or select one device by full ID, unambiguous ID prefix, or case-insensitive
name. Lost mode always requires explicit confirmation, and state-changing Find
My actions are never automatically replayed.

For mainland China accounts, pass `--china` and optionally
`--china-coordinates gcj02` or `bd09`.

## Standalone tracking

A complete starter configuration is available at
[`examples/config.toml`](examples/config.toml). Its essential shape is:

```toml
version = 1
base_zone_id = "home"
tracked_from_zones = ["home", "work"]

[[accounts]]
username = "name@example.com"
region = "global"
device_ids = []

[[zones]]
id = "home"
latitude = 59.3293
longitude = 18.0686
radius_meters = 100.0
passive = false

[[zones]]
id = "work"
latitude = 59.3320
longitude = 18.0640
radius_meters = 125.0
passive = false

[tracking]
tick_seconds = 5
prefetch_seconds = 15
default_interval_seconds = 60
maximum_interval_seconds = 7200
in_zone_interval_seconds = 120
stationary_interval_seconds = 300
exit_zone_interval_seconds = 30
fixed_interval_seconds = 0
gps_accuracy_threshold_meters = 100.0
old_location_adjustment_seconds = 0
old_location_maximum_seconds = 0
pass_through_delay_seconds = 60
stationary_enabled = true
stationary_still_seconds = 1800
stationary_radius_meters = 100.0
travel_time_factor = 0.5
```

Validate and run it with:

```console
cp examples/config.toml config.toml
icloud-location config validate --config config.toml
icloud-location zones --config config.toml
icloud-location watch --config config.toml
# `track` is an alias for `watch`
```

The runtime uses a five-second decision tick, fifteen-second prefetch, one
account-wide Apple refresh for co-due devices, independent account failure
handling, and atomic state persistence during operation and shutdown. Existing
trusted sessions are sufficient; for a single account, `ICLOUD_PASSWORD` can
also supply credentials for automatic renewal.

Operational commands work directly on durable state:

```console
icloud-location snapshot
icloud-location --json snapshot
icloud-location --ndjson events
icloud-location pause DEVICE_ID
icloud-location resume DEVICE_ID
icloud-location schedule DEVICE_ID --at 2026-08-19T18:30:00Z
```

Set `--state-file` or `ICLOUD_STATE_FILE` to choose the state path.

## External location sources

External GPS integrations use a generic typed update. `ingest` accepts one JSON
object, an array, or NDJSON from a file or standard input:

```console
icloud-location ingest --input updates.ndjson --config config.toml
```

[`tests/fixtures/external/location_updates.json`](tests/fixtures/external/location_updates.json)
is a sanitized input array using the public wire schema.

The library arbitrates Apple and external samples by timestamp and GPS quality,
tracks source health, applies typed zone triggers, pulls optional adapters
through `ExternalLocationSource`, and exposes outbound location requests through
`ExternalLocationRequester` for the embedding application.

## Optional Waze routing

Build with `--features waze`, then add a Waze section if desired:

```toml
[waze]
region = "eu"
real_time = true
minimum_distance_km = 1.0
maximum_distance_km = 100.0
history_database = "routes.sqlite3"
```

Waze remains a provider adapter outside tracking policy. Route history is
private `SQLite` storage behind `RouteHistoryStore` and supports lookup, reuse,
compression, ordered inspection, maintenance, and recalculation.

```console
icloud-location waze route 59.32 18.06 59.40 18.10
icloud-location waze history stats routes.sqlite3
icloud-location waze history list routes.sqlite3 --north-south
icloud-location waze history maintain routes.sqlite3
icloud-location waze history recalculate routes.sqlite3 --config config.toml
```

## Library

```rust,no_run
use icloud_location::{AuthenticationStatus, ICloudClient, LocateOptions};

# async fn example() -> icloud_location::Result<()> {
let mut client = ICloudClient::builder("name@example.com").build()?;

match client.authenticate().await? {
    AuthenticationStatus::Authenticated(_) => {
        for device in client.locate_devices(LocateOptions::family()).await? {
            if let Some(location) = device.location {
                println!("{}: {}, {}", device.name, location.latitude, location.longitude);
            }
        }
    }
    AuthenticationStatus::TwoFactorRequired(challenge) => {
        println!("verification required: {challenge:?}");
    }
    AuthenticationStatus::TermsOfUseRequired => {
        println!("explicit terms acceptance is required");
    }
}
# Ok(())
# }
```

The facade re-exports `core`, `findmy`, `routing`, `tracking`, and, when enabled,
`waze`. Public traits include `LocationProvider`, `ExternalLocationSource`,
`ExternalLocationRequester`, `RouteProvider`, `RouteHistoryStore`,
`TrackingStateStore`, `EventSink`, `Clock`, and `CredentialProvider`.

## Validation and parity

The parity target is iCloud3 revision
`3e367c54abdbb2f1ff2cb1a69f16f810e30ef35b`. See
[`PARITY_PLAN.md`](PARITY_PLAN.md), [`docs/PARITY_MATRIX.md`](docs/PARITY_MATRIX.md),
and [`docs/PARITY_EXCEPTIONS.md`](docs/PARITY_EXCEPTIONS.md).

```console
uv run --python 3.12 parity/python/validate_fixtures.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo build --workspace --release --all-features
```

The physical security-key probe and live Apple-account smoke test are ignored by
default and must be invoked explicitly with authorized hardware and credentials.

## License and attribution

MIT licensed. This port derives behavior and protocol handling from Gary Cobb's
MIT-licensed iCloud3 project, which in turn credits the Py iCloud community. See
the [license](https://github.com/jimmystridh/icloud-location/blob/main/LICENSE).
