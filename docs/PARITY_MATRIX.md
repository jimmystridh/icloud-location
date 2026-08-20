# Non-Home-Assistant parity matrix

This ledger covers externally meaningful behavior from iCloud3 revision
`3e367c54abdbb2f1ff2cb1a69f16f810e30ef35b`.

Statuses are `complete` for ported behavior, `adapted` for a platform-neutral
replacement, `divergent` for an intentional documented difference, and
`excluded` for Home Assistant presentation or registration. Intentional
differences are detailed in [`PARITY_EXCEPTIONS.md`](PARITY_EXCEPTIONS.md).

## Apple account and authentication

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| GSA/SRP `s2k` and `s2k_fo` sign-in | port | complete | `srp::tests::matches_py_srp_apple_gsa_vector` |
| Saved session-token login | port | complete | saved-token and password-free validation protocol tests |
| `/validate` token validation | port | complete | valid, invalid, and 2FA-required validation mocks |
| Trusted-device push request and verification | port | complete | trusted-device request/rejection state-machine tests |
| SMS phone discovery, request, and verification | port | complete | multi-phone parse plus selected-phone wire-flow test |
| FIDO2/WebAuthn USB security key | optional feature | complete | mocked ceremony plus ignored physical HID probe |
| Challenge metadata caching | safe port | complete | phone IDs and key names persist; phone display text does not |
| Trust session | port | complete | trusted-device, SMS, and security-key trust flows |
| Untrust, snapshot, and restore trusted session | port | complete | `snapshots_untrusts_and_restores_a_trusted_session` |
| Trust-cookie expiry inspection | port | complete | expiration and reauthentication-window cookie test |
| Proactive reauthentication | port | complete | injected-time near-expiry SRP test |
| Automatic recovery and idempotent read replay | port | complete | 421, 450, and 500 replay-once tests |
| Mutation replay prevention | safety rule | divergent | `does_not_replay_a_state_changing_find_my_action` |
| Configurable request timeout | port | complete | local delayed-server timeout test |
| IPv4 Apple transport | port | complete | all local protocol servers bind IPv4 loopback |
| Global and China endpoints | port | complete | endpoint selection plus GCJ-02/BD-09 vectors |
| Updated terms detection | port | complete | terms-required account fixture |
| Terms acceptance | explicit user action | divergent | confirmed getTerms/repairDone mock and rejection without confirmation |
| Account-lock state | port | complete | locked-account response test |
| Username/password validation helper | port | complete | lightweight SRP valid/invalid mock |
| Persist encoded password | never port | divergent | session files are inspected for password absence |
| Per-account cookies and tokens | port | complete | hashed account path, atomic round-trip, and `0700`/`0600` tests |
| Multiple accounts | standalone manager | adapted | coalescing and independent-failure runtime scenarios |

## Find My device protocol

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| Family-wide refresh | port | complete | refresh request captures `fmly=true` |
| Owner-only refresh | port | complete | locate option and request fixture with `fmly=false` |
| Selected-device refresh | port | complete | selected-device request capture |
| Device discovery and stable ID | port | complete | sanitized Find My response fixture |
| Device name cleanup | port | complete | curly apostrophe and non-breaking-space normalization |
| Model cleanup and accessory classification | port | complete | iPhone, Watch, AirPods, and AirTag model tests |
| Duplicate device names | port | complete | period-suffix uniqueness test |
| Family-shared versus owner classification | port | complete | mixed `fmlyShare` fixture coverage |
| Device status mapping | port | complete | known, unknown, offline, and other status cases |
| Battery normalization and labels | port | complete | charged, charging, low, missing, and low-power cases |
| Optional location and timestamp | port | complete | located and offline/missing-location fixtures |
| China coordinate conversion | port | complete | GCJ-02 and BD-09 vectors |
| Complete raw Apple response | preserve more data | divergent | provider raw-response round-trip |
| Find My unavailable state | port | complete | HTTP 501 conversion test |
| Account-wide refresh coalescing | port | complete | two due devices cause one provider call |
| Cached state across refreshes | port | complete | previous-location restart round-trip |
| Play sound | port | complete | exact endpoint/payload plus no-replay test |
| Display message | port | complete | exact endpoint/payload plus required-field validation |
| Lost mode | confirmed action | divergent | exact payload, library token, and CLI confirmation tests |
| Malformed successful response handling | hardening | complete | malformed Find My protocol response test |

## Device state and tracking policy

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| Durable previous location and metadata | port | complete | versioned restart preserves family classification, previous/current location, battery source, and raw metadata |
| Location age and dynamic old threshold | port | complete | old-threshold branch and boundary tests |
| GPS accuracy classification | port | complete | quality threshold fixture |
| Old/poor GPS grace and rejection | port | complete | two-bad-update grace and rejection tests |
| Distance moved | port | complete | Haversine vector and accepted-location state test |
| Nearby-device distance | port | complete | multi-device eligibility/group fixture |
| Direction and history | port | complete | towards, away, stationary, far-away, and override tests |
| Track-from/base-zone calculations | typed configuration | adapted | validated base/tracked IDs and multi-zone runtime scenario |
| Per-device HA policy overrides | runtime-wide profile | divergent | documented separate-process/profile boundary |
| Dynamic distance intervals | port | complete | Python-validated interval fixture and Rust differential test |
| Route/travel-time adjustment | provider interface | adapted | fake-provider arrival/direction/interval scenario |
| In-zone, fixed, maximum, offline, and exit intervals | port | complete | priority and boundary policy tests |
| Location/authentication retry backoff | port | complete | exact count table fixture plus runtime account retry state |
| Pause and resume | port | complete | durable state transition test and CLI commands |
| Immediate and delayed locate | API/CLI | adapted | `locate_now`, `schedule`, and injected-time state tests |
| Previous-result reuse | port | complete | history lookup and nearby route-sharing scenario |
| Battery state arbitration | port | complete | newer battery timestamp wins |
| Missing location representation | Rust model | divergent | optional location tests; no sentinel coordinate/date |

## Zones and stationary behavior

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| Typed circular zones | adapt HA zones | adapted | TOML validation and geometry tests |
| Closest-zone calculation | port | complete | explicit closest-zone and fixture tests |
| GPS allowance for in-zone selection | port | complete | half-accuracy boundary fixture |
| Smallest overlapping zone wins | port | complete | overlapping-zone fixture |
| Same/overlapping center detection | port | complete | two-meter boundary test |
| Zone enter/exit events | typed events | adapted | ordered runtime transition scenario |
| Pass-through delay | port | complete | tracked-immediate and delayed-entry virtual-time tests |
| Stationary-zone lifecycle | persisted manager | adapted | create, move, exit, remove, and reuse scenario |
| Zone device counts | snapshot | adapted | multi-device occupancy snapshot test |
| Nearby grouping, shared routes, and exit refresh | port | complete | eligibility, one-route/two-device, and two-minute exit scheduling scenarios |
| Away-time-zone offsets | platform-neutral snapshot | adapted | offset boundary, validation, and snapshot tests |

## Routing and Waze

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| Provider-neutral routing | adaptation boundary | adapted | tracking uses fake `RouteProvider` only |
| Straight-line fallback | port | complete | Haversine route estimate test |
| Waze isolation | optional adapter | adapted | independent feature/crate; tracking has no Waze dependency |
| Waze regions | port | complete | America, Israel, and rest-of-world mapping test |
| Real-time/historical segment time | port | complete | recorded camel/snake-case fixture aggregation |
| Waze min/max status | port | complete | out-of-range test avoids server request |
| Three-attempt retry | port | complete | deterministic local transport failures and query capture |
| Escalating pause | port | complete | 10/20/30/40+ error-count scenario |
| SQLite route history | port through interface | complete | CRUD, reopen, private permission, and version test |
| Proximity lookup and use counts | port | complete | lookup boundary and count increment test |
| Compression and maintenance | port | complete | duplicate-coordinate maintenance test |
| Directional ordering and recalculation | port | complete | north/south, east/west, and provider recalculation test |
| Live Waze calls in CI | recorded fixtures only | divergent | no live endpoint required by the suite |

## External location sources

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| Generic external update | adapt mobile app | adapted | JSON fixture, API, and CLI parser |
| Apple/external freshness arbitration | port | complete | conflicting fixture plus older-Apple-after-external regression scenario |
| Accuracy arbitration | port | complete | conflicting accuracy fixture |
| External zone triggers | typed trigger | adapted | enter/exit runtime path |
| Source health and throttling | port | complete | injected-time health, persistence, and snapshot scenarios |
| Outbound location request | callback interface | adapted | fake requester/throttle test |
| Webhook or app origin | source interface | adapted | `ExternalLocationSource` feeds the same public ingestion pipeline regardless of transport |

## Runtime, persistence, events, and CLI

| Capability | Disposition | Status | Repository evidence |
| --- | --- | --- | --- |
| Five-second scheduler tick | standalone Tokio | adapted | default validation and deterministic `tick(now)` scenarios |
| Fifteen-second Apple prefetch | port | complete | 16/15-second boundary runtime test |
| Multi-account orchestration | standalone manager | adapted | failure isolation and per-account coalescing tests |
| Concurrency bound | sequential accounts | divergent | documented bounded standalone scheduling choice |
| Typed snapshots replacing sensors | neutral schema | adapted | serialization, raw data, route, zone, battery, nearby, and away-time tests |
| Typed events replacing event log | neutral sink | adapted | ordered sink plus persisted capped history |
| Versioned tracking store | neutral JSON | adapted | v0-v3 migration, account/device restart state, future rejection, atomic partial-write, and permissions tests |
| Typed versioned configuration | TOML | adapted | defaults, v0 migration, validation, round-trip, atomicity, and permissions tests |
| Graceful shutdown | standalone runtime | adapted | immediate shutdown persists paused state and events |
| Basic login/status/devices/locate/logout CLI | command surface | adapted | Clap parser and release smoke tests |
| Watch/track CLI | standalone daemon | adapted | parser plus runtime controlled-shutdown scenario |
| Pause/resume/schedule CLI | public runtime API | adapted | parser plus durable state and typed-event command tests |
| Zone/config CLI | typed TOML commands | adapted | parser and config/zone validation tests |
| Find My action CLI | confirmed commands | adapted | parser, confirmation, and protocol payload tests |
| Session maintenance CLI | public client API | adapted | validate, credential validate, refresh, trust, reset, and terms parser/protocol tests |
| Waze inspection/maintenance CLI | optional commands | adapted | route/history parser plus store tests |
| Human, JSON, and NDJSON output | standalone rendering | adapted | distinct snapshot/event rendering paths and schema serialization |
| Atomic operation-time persistence | standalone state store | adapted | external ingest/request, commands, tick, and shutdown save paths |
| Opt-in live Apple validation | never CI | divergent | ignored `tests/live_apple.rs` smoke test |

## Explicit Home Assistant exclusions

| Capability | Status |
| --- | --- |
| Entity and device registries | excluded |
| Sensor creation and state writes | excluded; typed snapshots replace them |
| Lovelace dashboard generation | excluded |
| Home Assistant config flow and forms | excluded; typed TOML replaces them |
| Home Assistant service registration | excluded; library/CLI methods replace it |
| Home Assistant notifications | excluded; typed events replace them |
| Home Assistant mobile-app entity discovery | excluded; neutral ingestion remains included |

## Completion audit

Every non-excluded row is `complete`, `adapted`, or a documented `divergent`
decision. Validation requires the Python fixture harness, formatting, Clippy with
denied warnings, all-feature workspace tests and doctests, and an all-feature
release build. Physical FIDO2 and live Apple tests remain explicit opt-in checks.
