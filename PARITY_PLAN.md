# iCloud3 Functional Parity Plan

> Implementation status (2026-08-19): all phases have been implemented and
> reconciled in [`docs/PARITY_MATRIX.md`](docs/PARITY_MATRIX.md). Intentional
> safety and standalone adaptation differences are recorded in
> [`docs/PARITY_EXCEPTIONS.md`](docs/PARITY_EXCEPTIONS.md). The ignored physical
> FIDO2 and live Apple tests remain opt-in validation boundaries, not CI inputs.

## Objective

Bring `icloud-location` close to behavioral parity with iCloud3 outside Home Assistant.

Given the same Apple responses, device history, zones, time, and configuration, the Rust implementation should make the same tracking decisions and produce equivalent outputs. The target is behavioral parity rather than a line-for-line port or API-compatible rewrite of the Python implementation.

## Scope

### Included

- Apple authentication, sessions, retries, and security keys.
- Device discovery, location, status, battery, and Find My actions.
- Dynamic scheduling and multi-account coordination.
- Location quality and source arbitration.
- Zones, stationary zones, pass-through zones, and nearby devices.
- Direction of travel and interval calculation.
- Waze routing and route-history caching.
- Pause, resume, manual locate, and operational commands.
- Durable tracking state, configuration, and event history.

### Adapted to platform-neutral interfaces

| Original dependency | Rust replacement |
| --- | --- |
| Home Assistant zones | Typed zone configuration |
| Home Assistant sensors and entities | `TrackingSnapshot` and typed events |
| Home Assistant mobile-app entities | Generic external-location ingestion API |
| Home Assistant services | Library methods and CLI commands |
| Home Assistant notifications and event log | `EventSink`, structured tracing, and JSON/NDJSON output |
| Home Assistant restore state | Versioned persistent state store |

### Excluded

- Home Assistant entity and device registries.
- Lovelace dashboards.
- Home Assistant config flows and forms.
- Home Assistant service registration.
- Home Assistant-specific notifications and sensor publishing.

## Implemented architecture

The approved workspace layout is:

```text
icloud-location             Existing public facade, CLI, and runtime
├── icloud-location-core    Shared models, traits, events, and configuration
├── icloud-findmy           Apple authentication, sessions, and Find My protocol
├── icloud-tracking         Pure tracking and zone state machine
├── icloud-routing          RouteProvider and RouteHistoryStore interfaces
└── icloud-waze             Optional Waze and SQLite history implementation
```

The existing `icloud_location` public API remains available through the facade to avoid unnecessary breakage.

### Core interfaces

- `LocationProvider`: returns device snapshots; Apple Find My is one implementation.
- `ExternalLocationSource`: supplies mobile-app or other GPS updates without knowing about Home Assistant.
- `RouteProvider`: calculates route distance and travel time; Waze is optional.
- `RouteHistoryStore`: caches route observations independently from any route provider.
- `TrackingStore`: persists device, zone, scheduler, and historical state.
- `EventSink`: receives authentication, zone, device, scheduling, action, and error events.
- `Clock`: supplies time so tracking and scheduler tests remain deterministic.
- `CredentialProvider`: supplies passwords or credentials from an interactive prompt, environment, or optional OS keychain.

## Implementation phases

### 1. Establish the parity specification

- Inventory every non-Home-Assistant public behavior in iCloud3.
- Classify each behavior as `port`, `adapt`, `exclude`, or `deliberate divergence`.
- Create sanitized Apple, Waze, device, and zone fixtures.
- Capture golden interval, zone-selection, GPS-quality, route-history, and device-state results from Python.
- Track implementation and test status in a parity matrix.

Exit criterion: every original capability has an explicit disposition and at least one representative test case.

### 2. Complete Apple authentication parity

- Add `/validate` token validation.
- Add automatic trusted-session refresh and safe recovery for HTTP 421, 450, and 500 responses.
- Automatically replay only idempotent reads.
- Track trust-cookie expiration and support proactive reauthentication.
- Cache trusted phone IDs and registered security-key names.
- Add optional USB FIDO2/WebAuthn support behind a `security-key` feature.
- Support trusted-session snapshot, untrust, and restore operations.
- Preserve the current SRP, trusted-device, and SMS authentication flows.
- Keep all authentication errors and diagnostics credential-safe.

Exit criterion: mocked state-machine tests cover fresh login, saved token, token validation, push, SMS, security key, expired session, retry, and terms-required flows.

### 3. Complete Find My protocol parity

- Preserve family-wide, owner-only, and selected-device refreshes.
- Add Find My service-availability and Apple-account-lock state.
- Add play-sound, display-message, and lost-mode operations.
- Require explicit confirmation for lost mode.
- Never automatically retry state-changing device actions.
- Coalesce account-wide refreshes so multiple due devices cause one Apple request.
- Preserve both normalized device fields and the raw Apple response.

Exit criterion: mocked request and response fixtures match the Python implementation for every supported Find My endpoint.

### 4. Build the durable device model

- Preserve the current typed device snapshot and raw Apple response.
- Add previous location, update age, offline duration, GPS-quality state, battery normalization, and source metadata.
- Port model-name cleanup, AirPods classification, duplicate-name handling, and family/owner classification.
- Model missing locations as optional data rather than sentinel coordinates or timestamps.
- Persist tracking state separately from authentication secrets.
- Version persisted state and provide migrations.

Exit criterion: restarting the process restores the same device state and produces the same next tracking decision.

### 5. Port the tracking engine as pure policy

- Port distance and movement calculations.
- Port old-location and poor-GPS rejection.
- Port direction-of-travel history.
- Port exact retry counters and interval tables.
- Port fixed, maximum, offline, exit-zone, and in-zone interval behavior.
- Port arrival-time and track-from-zone calculations.
- Keep network, filesystem, wall-clock, logging, and CLI concerns outside policy calculations.

Exit criterion: differential tests match the Python tracking decisions for the same fixtures and injected time.

### 6. Port zone behavior

- Load typed circular zones from configuration.
- Port track-from-zone and base-zone selection.
- Port overlapping-zone and closest-zone resolution.
- Port enter/exit detection and pass-through delays.
- Port stationary-zone creation, movement, reuse, and removal.
- Port nearby-device grouping and shared-location decisions.
- Port away-time-zone offsets where they remain useful outside Home Assistant.
- Emit typed events for every zone transition.

Exit criterion: scenario tests cover approaching, entering, leaving, passing through, remaining stationary, overlapping zones, and nearby-device movement.

### 7. Decouple routing and port Waze

- Define `RouteProvider` without Waze-specific types.
- Implement straight-line routing as a dependency-free fallback.
- Port Waze regions, real-time mode, distance limits, status, and retry behavior.
- Port SQLite route history, compression, recalculation, direction tracking, and maintenance.
- Store route history through `RouteHistoryStore` rather than accessing SQLite from tracking policy.
- Keep Waze in a separate optional crate because its endpoint is unofficial.
- Use sanitized recorded fixtures in tests; do not depend on live Waze calls in CI.

Exit criterion: disabling Waze requires no changes to `icloud-tracking`, and fixture-backed Waze tests reproduce the Python results.

### 8. Preserve mobile-app arbitration without Home Assistant

- Define an external update containing device ID, coordinates, accuracy, timestamp, battery, and optional zone trigger.
- Port iCloud-versus-mobile-app freshness and quality selection.
- Port source-health, old-data, and request-throttling rules that do not depend on Home Assistant.
- Accept updates through the Rust API and an optional CLI JSON/NDJSON input.
- Keep outbound location requests behind a callback trait implemented by the embedding application.

Exit criterion: tracking decisions are identical whether external updates originate from a webhook, another application, or test fixtures.

### 9. Build the standalone runtime

- Implement a Tokio scheduler with a five-second decision tick and dynamic request intervals.
- Port the fifteen-second prefetch behavior.
- Coordinate multiple Apple accounts.
- Coalesce account-wide requests and enforce concurrency limits.
- Support pause, resume, immediate locate, and scheduled locate.
- Add typed, versioned TOML configuration with validation and migration.
- Persist state atomically during operation and graceful shutdown.
- Avoid duplicate requests after restart or concurrent due-device events.
- Expose structured events independently from the terminal UI.

Exit criterion: restart, network loss, expired sessions, and concurrent due devices neither lose state nor produce duplicate requests.

### 10. Expand the library and CLI surface

- Add `watch` or `daemon` operation.
- Add `track`, `pause`, `resume`, and `schedule` commands.
- Add `zones` and `config validate` commands.
- Add `sound`, `message`, and confirmed `lost-mode` commands.
- Add `session validate`, `session refresh`, and `session reset` commands.
- Add Waze route and route-history inspection and maintenance commands.
- Support human-readable, JSON, and NDJSON snapshots and event output.
- Ensure every CLI operation uses the public Rust library rather than private implementation details.

Exit criterion: all non-Home-Assistant operational actions are accessible through both the library and CLI.

### 11. Parity and release validation

- Run differential Python-versus-Rust golden tests.
- Run mocked Apple and Waze protocol servers.
- Run deterministic scheduler tests with virtual time.
- Test restart, migration, and persistence behavior.
- Test secret redaction and filesystem permissions.
- Inject timeouts, malformed responses, partial writes, session expiry, unavailable services, and database errors.
- Maintain an opt-in live Apple-account test suite that is never run in CI.
- Run formatting, clippy with denied warnings, unit tests, doctests, and release builds.
- Document every remaining parity exception.

Exit criterion: the parity matrix is complete, all required checks pass, and remaining differences are intentional and documented.

## Delivery milestones

### Milestone 1: Reliable Apple client

Phases 1 through 3. The CLI supports resilient authentication, automatic read recovery, security keys, complete device discovery, and Find My actions.

### Milestone 2: Deterministic tracking core

Phases 4 through 6. The Rust state machine reproduces iCloud3's device, interval, GPS-quality, and zone decisions without external services.

### Milestone 3: Decoupled routing and external sources

Phases 7 and 8. Waze and external mobile-app-style updates are optional adapters around the tracking core.

### Milestone 4: Standalone parity runtime

Phases 9 through 11. The daemon, CLI, persistence, events, and failure recovery support sustained multi-account operation.

## Testing strategy

### Differential tests

Run equivalent inputs through extracted Python behavior and Rust policy functions, then compare structured results. Use `uv` for the Python harness and freeze the current Python source revision used to produce each fixture.

### Protocol tests

Use local mock servers to model Apple's authentication and Find My state machines, including header capture, cookies, 2FA, expired tokens, retries, malformed responses, and service errors.

### Time-dependent tests

Use an injected clock and Tokio virtual time. Tests must not sleep according to wall-clock intervals.

### Live tests

Keep live Apple and Waze tests opt-in, ignored by default, credential-safe, and outside CI. Never record live tokens, cookies, phone numbers, device IDs, or precise locations in fixtures.

## Deliberate safety differences

These behaviors should remain different even under a one-to-one parity goal:

- Never persist an obfuscated password. Use an optional OS-keychain-backed credential provider.
- Never accept Apple terms silently. Require an explicit command and confirmation or direct the user to iCloud.com.
- Never automatically retry state-changing device actions.
- Require confirmation for lost mode.
- Redact tokens, cookies, phone numbers, device identifiers, precise locations, and credentials from errors and tracing where appropriate.
- Keep Waze optional and identify it as an unofficial interface.
- Use private file permissions and atomic writes for all persisted authentication material.

## Implementation order used

The work started with the behavior-parity matrix and sanitized fixture harness,
followed by authentication and session resilience. This established evidence for
subsequent parity work before the standalone tracking runtime was built.
