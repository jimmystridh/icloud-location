# Intentional parity exceptions

The reference behavior is iCloud3 revision
`3e367c54abdbb2f1ff2cb1a69f16f810e30ef35b`. These differences are intentional;
they are not unfinished Home Assistant replacements.

## Safety differences

- Passwords are accepted from a prompt, environment, or caller-provided
  `CredentialProvider`; they are never obfuscated and persisted. Only Apple
  tokens and cookies are saved.
- Apple terms are never accepted silently. The library requires a confirmation
  token and the CLI requires `session accept-terms --confirm`.
- Sound, message, and lost-mode requests are sent once and never automatically
  replayed after an authentication or transport error.
- Lost mode requires explicit confirmation in both the library and CLI.
- Session directories use hashed account keys. Authentication, configuration,
  tracking state, and Waze history files written by the library use owner-only
  Unix permissions where supported.

## Standalone adaptation differences

- Home Assistant zones are typed circular TOML zones. `base_zone_id` selects the
  route/direction reference and `tracked_from_zones` bypass pass-through delay.
- Interval, stationary, quality, base-zone, and tracked-zone settings form one
  runtime profile. iCloud3 can attach some of those overrides to individual HA
  devices; standalone deployments needing different profiles run separate
  configurations and state files.
- The base zone is the only track-from zone sent to a configured
  `RouteProvider`. Other tracked zones retain independent straight-line
  distance, direction, history, and interval state; the earliest interval wins.
  This avoids multiplying calls to an optional unofficial route service.
- HA sensor/entity output is replaced by versioned `TrackingSnapshot` data and
  typed `TrackingEvent` values. Human, JSON, and NDJSON rendering belongs to the
  CLI.
- HA mobile-app discovery and service calls are replaced by
  `ExternalLocationUpdate` ingestion and an `ExternalLocationRequester`
  callback. The freshness, accuracy, trigger, health, and throttle decisions
  remain in pure tracking policy.
- HA restore state is replaced by a versioned JSON state store. Scheduling time
  is injected into `tick`, policy functions, and scenario tests instead of read
  from HA.
- Account retry state is keyed by a stable one-way account label in the CLI so
  usernames are not copied into tracking state or event history.
- Account refreshes are currently bounded by processing accounts sequentially.
  Failures are isolated and co-due devices still share one family-wide request.
  This is observably equivalent for tracking decisions but not an attempt to
  preserve HA task scheduling internals.
- Away-time-zone offsets are declarative configuration and remain configured
  while a device is home. The snapshot exposes the configured local time at all
  times; the runtime does not rewrite TOML on arrival.

## Deliberate data and adapter differences

- Typed core values use UTC timestamps, meters, kilometers, and seconds. HA
  locale-specific units and 12/24-hour sensor formatting remain presentation
  concerns rather than tracking policy.
- The complete raw Apple device object is retained alongside normalized fields;
  iCloud3 filters more of it internally.
- Missing locations use `Option` rather than sentinel coordinates or dates.
- Waze is an independent optional crate behind provider-neutral routing and
  history interfaces. Disabling it does not alter `icloud-tracking`.
- Straight-line routing remains available without Waze. It has distance but no
  invented travel duration.
- Tiny floating-point differences are allowed at serialization precision;
  fixture comparisons use explicit tolerances where the Python implementation
  also performs geographic arithmetic.

## Validation boundaries

- Apple and Waze web APIs are unofficial. CI uses local protocol servers and
  sanitized recorded fixtures, not live services.
- The USB FIDO2 discovery test is ignored unless explicitly run with a physical
  authenticator.
- The live Apple smoke test is ignored unless explicitly run with an authorized
  account and a caller-selected private session directory.
