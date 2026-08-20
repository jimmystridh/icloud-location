#[cfg(feature = "waze")]
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
#[cfg(feature = "security-key")]
use icloud_location::UsbSecurityKeyAuthenticator;
use icloud_location::core::{EventSink, EventSinkError, ExternalLocationUpdate, TrackingEvent};
use icloud_location::tracking::{JsonTrackingStore, TrackingStateStore};
use icloud_location::{
    AuthenticationStatus, ChinaCoordinates, ClientBuilder, Device, DisplayMessageRequest, Error,
    FindMyProvider, ICloudClient, LocateOptions, LostModeConfirmation, LostModeRequest, Region,
    TermsAcceptanceConfirmation, TwoFactorChallenge, VerificationMethod,
    config::{AppConfig, AppleRegion, default_config_path},
    runtime::{RuntimeConfig, TrackingRuntime},
};
#[cfg(feature = "waze")]
use icloud_location::{
    routing::{RouteHistoryStore, RouteProvider, RouteRequest},
    waze::{
        RouteHistoryOrder, SqliteRouteHistoryStore, WazeClient, WazeConfig as WazeClientConfig,
        WazeRegion,
    },
};
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, env = "ICLOUD_USERNAME")]
    username: Option<String>,

    #[arg(long, env = "ICLOUD_SESSION_ROOT")]
    session_root: Option<PathBuf>,

    #[arg(long, env = "ICLOUD_STATE_FILE")]
    state_file: Option<PathBuf>,

    #[arg(long)]
    china: bool,

    #[arg(long, value_enum, default_value_t = ChinaCoordinateArg::Unchanged)]
    china_coordinates: ChinaCoordinateArg,

    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true, conflicts_with = "json")]
    ndjson: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Establish or renew an authenticated iCloud session.
    Login(LoginArgs),
    /// Check whether the saved session can access Find My.
    Status,
    /// List devices and their current status.
    Devices(DeviceQueryArgs),
    /// Print current device locations.
    Locate(LocateArgs),
    /// Remove locally saved tokens and cookies for this account.
    Logout,
    /// Validate, refresh, inspect, or reset a saved Apple session.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Play a Find My sound on a device.
    Sound(SoundArgs),
    /// Display a Find My message on a device.
    Message(MessageArgs),
    /// Enable Find My lost mode after explicit confirmation.
    LostMode(LostModeArgs),
    /// Validate standalone TOML configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect configured circular zones.
    Zones(ConfigPathArgs),
    /// Run the standalone multi-account scheduler until interrupted.
    #[command(visible_alias = "track")]
    Watch(ConfigPathArgs),
    /// Pause one device, or every persisted device when omitted.
    Pause(DeviceStateArgs),
    /// Resume one device, or every persisted device when omitted.
    Resume(DeviceStateArgs),
    /// Schedule an absolute UTC update time for a persisted device.
    Schedule(ScheduleArgs),
    /// Print the current durable, platform-neutral tracking snapshot.
    Snapshot,
    /// Print persisted typed tracking events.
    Events,
    /// Ingest one JSON update, a JSON array, or newline-delimited JSON.
    Ingest(IngestArgs),
    /// Inspect Waze routing and route history when built with the `waze` feature.
    #[cfg(feature = "waze")]
    Waze {
        #[command(subcommand)]
        command: WazeCommand,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Validate the saved token without prompting for a password.
    Validate,
    /// Validate the account password with a lightweight SRP exchange.
    ValidateCredentials,
    /// Authenticate or renew the saved session.
    Refresh,
    /// Show trust-cookie expiry and proactive reauthentication status.
    TrustStatus,
    /// Clear trust credentials but retain non-secret challenge metadata.
    Untrust,
    /// Delete all local session tokens and cookies.
    Reset,
    /// Fetch and accept current iCloud terms after explicit confirmation.
    AcceptTerms {
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Parse and validate a configuration file without contacting services.
    Validate(ConfigPathArgs),
}

#[derive(Debug, Args)]
struct ConfigPathArgs {
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SoundArgs {
    device_id: String,
    #[arg(long, default_value = "Find My iPhone Alert")]
    subject: String,
}

#[derive(Debug, Args)]
struct MessageArgs {
    device_id: String,
    #[arg(long, default_value = "iCloud Service Alert")]
    subject: String,
    #[arg(long)]
    message: String,
    #[arg(long)]
    sound: bool,
}

#[derive(Debug, Args)]
struct LostModeArgs {
    device_id: String,
    #[arg(long)]
    phone_number: String,
    #[arg(long, default_value = "This iPhone has been lost. Please call me.")]
    message: String,
    #[arg(long, default_value = "")]
    new_passcode: String,
    #[arg(long)]
    confirm: bool,
}

#[derive(Debug, Args)]
struct DeviceStateArgs {
    device_id: Option<String>,
}

#[derive(Debug, Args)]
struct ScheduleArgs {
    device_id: String,
    /// RFC 3339 UTC timestamp, for example 2026-08-19T18:30:00Z.
    #[arg(long)]
    at: String,
}

#[derive(Debug, Args)]
struct IngestArgs {
    /// Read updates from this file instead of standard input.
    #[arg(long)]
    input: Option<PathBuf>,
    /// Apply tracking and zone settings from this configuration file.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[cfg(feature = "waze")]
#[derive(Debug, Subcommand)]
enum WazeCommand {
    /// Request a route between two coordinates.
    Route(WazeRouteArgs),
    /// Inspect or maintain a route-history database.
    History {
        #[command(subcommand)]
        command: WazeHistoryCommand,
    },
}

#[cfg(feature = "waze")]
#[derive(Debug, Subcommand)]
enum WazeHistoryCommand {
    /// Print the number of stored routes.
    Stats { database: PathBuf },
    /// Remove duplicate rounded-coordinate records and compact the database.
    Maintain { database: PathBuf },
    /// List stored routes in map traversal order.
    List {
        database: PathBuf,
        #[arg(long)]
        north_south: bool,
    },
    /// Recalculate every stored route using configured zone origins.
    Recalculate {
        database: PathBuf,
        #[arg(long)]
        config: PathBuf,
    },
}

#[cfg(feature = "waze")]
#[derive(Debug, Args)]
struct WazeRouteArgs {
    from_latitude: f64,
    from_longitude: f64,
    to_latitude: f64,
    to_longitude: f64,
    #[arg(long, default_value = "eu")]
    region: String,
    #[arg(long)]
    historical: bool,
    #[arg(long, default_value_t = 0.0)]
    minimum_distance_km: f64,
    #[arg(long, default_value_t = 1_000.0)]
    maximum_distance_km: f64,
}

#[derive(Debug, Args)]
struct LoginArgs {
    /// Send the verification code by SMS to this trusted phone ID.
    #[arg(long)]
    sms_phone_id: Option<u64>,
    /// Authenticate with a connected FIDO2 USB HID security key.
    #[arg(long, conflicts_with = "sms_phone_id")]
    security_key: bool,
}

#[derive(Debug, Args)]
struct DeviceQueryArgs {
    /// Request only devices owned by this Apple account, excluding Family Sharing devices.
    #[arg(long)]
    owner_only: bool,
}

#[derive(Debug, Args)]
struct LocateArgs {
    /// Device ID, ID prefix, or case-insensitive device name. Omit to show every located device.
    selector: Option<String>,

    /// Request only devices owned by this Apple account, excluding Family Sharing devices.
    #[arg(long)]
    owner_only: bool,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum ChinaCoordinateArg {
    #[default]
    Unchanged,
    Gcj02,
    Bd09,
}

impl From<ChinaCoordinateArg> for ChinaCoordinates {
    fn from(value: ChinaCoordinateArg) -> Self {
        match value {
            ChinaCoordinateArg::Unchanged => Self::Unchanged,
            ChinaCoordinateArg::Gcj02 => Self::Gcj02,
            ChinaCoordinateArg::Bd09 => Self::Bd09,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Config { command } => run_config_command(command),
        Command::Zones(arguments) => list_zones(arguments, cli.json),
        Command::Watch(arguments) => watch(&cli, arguments).await,
        Command::Pause(arguments) => update_pause_state(&cli, arguments.device_id.as_deref(), true),
        Command::Resume(arguments) => {
            update_pause_state(&cli, arguments.device_id.as_deref(), false)
        }
        Command::Schedule(arguments) => schedule_state(&cli, arguments),
        Command::Snapshot => show_snapshot(&cli),
        Command::Events => show_events(&cli),
        Command::Ingest(arguments) => ingest_updates(&cli, arguments).await,
        #[cfg(feature = "waze")]
        Command::Waze { command } => waze_command(command, cli.json).await,
        _ => run_apple_command(&cli).await,
    }
}

async fn run_apple_command(cli: &Cli) -> Result<()> {
    let mut client = build_client(cli)?;
    match &cli.command {
        Command::Login(arguments) => login(&mut client, arguments, cli.json).await,
        Command::Status => status(&mut client, cli.json).await,
        Command::Devices(arguments) => {
            require_authenticated(&mut client).await?;
            let devices = client
                .locate_devices(locate_options(arguments.owner_only))
                .await?;
            print_devices(&devices, cli.json)
        }
        Command::Locate(arguments) => {
            require_authenticated(&mut client).await?;
            let devices = client
                .locate_devices(locate_options(arguments.owner_only))
                .await?;
            let devices = select_devices(devices, arguments.selector.as_deref())?;
            print_locations(&devices, cli.json)
        }
        Command::Logout => {
            client.clear_session()?;
            println!(
                "Removed the saved iCloud session for {}",
                required_username(cli)?
            );
            Ok(())
        }
        Command::Session { command } => session_command(&mut client, command, cli.json).await,
        Command::Sound(arguments) => {
            require_authenticated(&mut client).await?;
            client
                .play_sound(&arguments.device_id, &arguments.subject)
                .await?;
            print_action_result("sound_requested", &arguments.device_id, cli.json)
        }
        Command::Message(arguments) => {
            require_authenticated(&mut client).await?;
            client
                .display_message(&DisplayMessageRequest {
                    device_id: arguments.device_id.clone(),
                    subject: arguments.subject.clone(),
                    message: arguments.message.clone(),
                    sound: arguments.sound,
                })
                .await?;
            print_action_result("message_requested", &arguments.device_id, cli.json)
        }
        Command::LostMode(arguments) => {
            if !arguments.confirm {
                bail!(
                    "lost mode is destructive; repeat with --confirm after checking the device ID"
                );
            }
            require_authenticated(&mut client).await?;
            client
                .enable_lost_mode(
                    &LostModeRequest {
                        device_id: arguments.device_id.clone(),
                        phone_number: arguments.phone_number.clone(),
                        message: arguments.message.clone(),
                        new_passcode: arguments.new_passcode.clone(),
                    },
                    LostModeConfirmation::confirm(),
                )
                .await?;
            print_action_result("lost_mode_requested", &arguments.device_id, cli.json)
        }
        Command::Config { .. }
        | Command::Zones(_)
        | Command::Watch(_)
        | Command::Pause(_)
        | Command::Resume(_)
        | Command::Schedule(_)
        | Command::Snapshot
        | Command::Events
        | Command::Ingest(_) => unreachable!("handled before building an Apple client"),
        #[cfg(feature = "waze")]
        Command::Waze { .. } => unreachable!("handled before building an Apple client"),
    }
}

fn build_client(cli: &Cli) -> Result<ICloudClient> {
    let region = if cli.china {
        Region::China {
            coordinates: cli.china_coordinates.into(),
        }
    } else {
        Region::Global
    };
    let mut builder = ClientBuilder::new(required_username(cli)?)
        .region(region)
        .timeout(Duration::from_secs(cli.timeout_seconds));
    if let Some(root) = &cli.session_root {
        builder = builder.session_root(root);
    }
    if let Ok(password) = std::env::var("ICLOUD_PASSWORD") {
        builder = builder.password(password);
    }
    builder.build().map_err(Into::into)
}

fn required_username(cli: &Cli) -> Result<&str> {
    cli.username
        .as_deref()
        .filter(|username| !username.trim().is_empty())
        .context("--username or ICLOUD_USERNAME is required for this command")
}

async fn session_command(
    client: &mut ICloudClient,
    command: &SessionCommand,
    json: bool,
) -> Result<()> {
    match command {
        SessionCommand::Validate => {
            let status = client.validate_session().await?;
            print_authentication_status(&status, json)
        }
        SessionCommand::ValidateCredentials => {
            match client.validate_credentials().await {
                Ok(()) => {}
                Err(Error::CredentialsRequired) => {
                    let password = rpassword::prompt_password("Apple account password: ")?;
                    if password.is_empty() {
                        bail!("password cannot be empty");
                    }
                    client.set_password(password);
                    client.validate_credentials().await?;
                }
                Err(error) => return Err(error.into()),
            }
            if json {
                println!("{}", serde_json::json!({ "credentials_valid": true }));
            } else {
                println!("Apple accepted the username and password");
            }
            Ok(())
        }
        SessionCommand::Refresh => {
            let status = client.refresh_session().await?;
            print_authentication_status(&status, json)
        }
        SessionCommand::TrustStatus => {
            let status = client.trust_cookie_status(Utc::now())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if let Some(status) = status {
                println!(
                    "Trust cookie expires {} ({} days remaining, reauthentication {})",
                    status.expires_at.to_rfc3339(),
                    status.days_remaining,
                    if status.reauthentication_recommended {
                        "recommended"
                    } else {
                        "not yet needed"
                    }
                );
            } else {
                println!("No persistent Apple trust cookie is stored");
            }
            Ok(())
        }
        SessionCommand::Untrust => {
            client.untrust_session()?;
            println!("Cleared local Apple trust credentials");
            Ok(())
        }
        SessionCommand::Reset => {
            client.clear_session()?;
            println!("Deleted the local Apple session");
            Ok(())
        }
        SessionCommand::AcceptTerms { confirm } => {
            if !confirm {
                bail!("accepting Apple terms requires --confirm after reviewing them");
            }
            let status = client
                .accept_terms(TermsAcceptanceConfirmation::confirm())
                .await?;
            print_authentication_status(&status, json)
        }
    }
}

fn print_authentication_status(status: &AuthenticationStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    match status {
        AuthenticationStatus::Authenticated(account) => {
            println!("Authenticated as {}", account.username);
        }
        AuthenticationStatus::TwoFactorRequired(_) => {
            println!("Two-factor authentication required");
        }
        AuthenticationStatus::TermsOfUseRequired => {
            println!("Updated iCloud terms require acceptance");
        }
    }
    Ok(())
}

fn print_action_result(action: &str, device_id: &str, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "action": action,
                "device_id": device_id,
                "accepted": true
            }))?
        );
    } else {
        println!("Apple accepted {action} for {device_id}");
    }
    Ok(())
}

fn run_config_command(command: &ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Validate(arguments) => {
            let path = config_path(arguments)?;
            let config = AppConfig::load(&path)?;
            println!(
                "Valid configuration version {}: {} account(s), {} zone(s)",
                config.version,
                config.accounts.len(),
                config.zones.len()
            );
            Ok(())
        }
    }
}

fn list_zones(arguments: &ConfigPathArgs, json: bool) -> Result<()> {
    let config = AppConfig::load(&config_path(arguments)?)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&config.zones)?);
    } else if config.zones.is_empty() {
        println!("No zones are configured");
    } else {
        println!("ID\tLATITUDE\tLONGITUDE\tRADIUS\tPASSIVE");
        for zone in config.zones {
            println!(
                "{}\t{:.6}\t{:.6}\t{:.0} m\t{}",
                zone.id, zone.latitude, zone.longitude, zone.radius_meters, zone.passive
            );
        }
    }
    Ok(())
}

fn update_pause_state(cli: &Cli, device_id: Option<&str>, paused: bool) -> Result<()> {
    let store = Arc::new(JsonTrackingStore::new(state_path(cli, None)?));
    let mut runtime =
        TrackingRuntime::new(RuntimeConfig::default(), store, Arc::new(SilentEventSink))?;
    let matched = runtime
        .state()
        .devices
        .values()
        .filter(|device| device_id.is_none_or(|selected| device.device_id == selected))
        .count();
    if matched == 0 {
        bail!("no persisted device matches the supplied device ID");
    }
    if paused {
        runtime.pause(device_id)?;
    } else {
        runtime.resume(device_id, Utc::now())?;
    }
    println!(
        "{} {matched} device(s)",
        if paused { "Paused" } else { "Resumed" }
    );
    Ok(())
}

fn schedule_state(cli: &Cli, arguments: &ScheduleArgs) -> Result<()> {
    let at = DateTime::parse_from_rfc3339(&arguments.at)
        .context("schedule time must be RFC 3339, for example 2026-08-19T18:30:00Z")?
        .with_timezone(&Utc);
    let store = Arc::new(JsonTrackingStore::new(state_path(cli, None)?));
    let mut runtime =
        TrackingRuntime::new(RuntimeConfig::default(), store, Arc::new(SilentEventSink))?;
    runtime.schedule(&arguments.device_id, at)?;
    println!("Scheduled {} for {}", arguments.device_id, at.to_rfc3339());
    Ok(())
}

fn show_snapshot(cli: &Cli) -> Result<()> {
    let store = JsonTrackingStore::new(state_path(cli, None)?);
    let snapshot = store.load()?.snapshot(Utc::now());
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else if cli.ndjson {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else if snapshot.devices.is_empty() {
        println!("No persisted devices");
    } else {
        println!("ID\tNAME\tSTATUS\tZONE\tDIRECTION\tNEXT UPDATE");
        for device in snapshot.devices {
            println!(
                "{}\t{}\t{}\t{}\t{:?}\t{}",
                short_id(&device.device_id),
                device.name.as_deref().unwrap_or("-"),
                device
                    .availability
                    .as_ref()
                    .map_or_else(|| "-".into(), |status| format!("{status:?}")),
                device.current_zone.as_deref().unwrap_or("away"),
                device.direction,
                device
                    .next_update_at
                    .map_or_else(|| "-".into(), |timestamp| timestamp.to_rfc3339())
            );
        }
    }
    Ok(())
}

fn show_events(cli: &Cli) -> Result<()> {
    let store = JsonTrackingStore::new(state_path(cli, None)?);
    let state = store.load()?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&state.event_history)?);
    } else if cli.ndjson {
        for event in state.event_history {
            println!("{}", serde_json::to_string(&event)?);
        }
    } else if state.event_history.is_empty() {
        println!("No persisted tracking events");
    } else {
        for event in state.event_history {
            println!("{}\t{:?}", event.occurred_at.to_rfc3339(), event.event);
        }
    }
    Ok(())
}

async fn ingest_updates(cli: &Cli, arguments: &IngestArgs) -> Result<()> {
    let source = arguments.input.as_ref().map_or_else(
        || io::read_to_string(io::stdin()).context("could not read updates from standard input"),
        |path| {
            fs::read_to_string(path)
                .with_context(|| format!("could not read updates from {}", path.display()))
        },
    )?;
    let updates = parse_external_updates(&source)?;
    let config = arguments
        .config
        .as_deref()
        .map_or_else(|| Ok(AppConfig::default()), AppConfig::load)?;
    let store = Arc::new(JsonTrackingStore::new(state_path(
        cli,
        arguments.config.as_deref(),
    )?));
    let events = Arc::new(ConsoleEventSink {
        json: cli.json || cli.ndjson,
    });
    let mut runtime = TrackingRuntime::new(runtime_config(&config)?, store, events)?;
    let count = updates.len();
    for update in updates {
        let decision = runtime.ingest_external_update(update, Utc::now()).await?;
        if cli.json && !cli.ndjson {
            println!("{}", serde_json::to_string(&decision)?);
        }
    }
    if !cli.json && !cli.ndjson {
        println!("Processed {count} external update(s)");
    }
    Ok(())
}

fn parse_external_updates(source: &str) -> Result<Vec<ExternalLocationUpdate>> {
    if let Ok(updates) = serde_json::from_str::<Vec<ExternalLocationUpdate>>(source) {
        return Ok(updates);
    }
    if let Ok(update) = serde_json::from_str::<ExternalLocationUpdate>(source) {
        return Ok(vec![update]);
    }
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("invalid external update on line {}", index + 1))
        })
        .collect()
}

async fn watch(cli: &Cli, arguments: &ConfigPathArgs) -> Result<()> {
    let config_path = config_path(arguments)?;
    let config = AppConfig::load(&config_path)?;
    if config.accounts.is_empty() {
        bail!("the configuration has no Apple accounts");
    }
    let store = Arc::new(JsonTrackingStore::new(state_path(cli, Some(&config_path))?));
    let events = Arc::new(ConsoleEventSink {
        json: cli.json || cli.ndjson,
    });
    let mut runtime = TrackingRuntime::new(runtime_config(&config)?, store, events)?;
    #[cfg(feature = "waze")]
    if let Some(waze) = config.waze.as_ref() {
        let provider = Arc::new(WazeClient::new(WazeClientConfig {
            region: WazeRegion::from_icloud3_name(&waze.region),
            real_time: waze.real_time,
            minimum_distance_km: waze.minimum_distance_km,
            maximum_distance_km: waze.maximum_distance_km,
            request_timeout: Duration::from_secs(60),
        })?);
        let history = waze
            .history_database
            .as_deref()
            .map(SqliteRouteHistoryStore::open)
            .transpose()?
            .map(|history| Arc::new(history) as Arc<dyn RouteHistoryStore>);
        runtime.set_routing(provider, history);
    }
    let single_account_password = (config.accounts.len() == 1)
        .then(|| std::env::var("ICLOUD_PASSWORD").ok())
        .flatten();
    for account in config.accounts {
        let mut builder =
            ClientBuilder::new(&account.username).region(region_from_config(account.region));
        if let Some(root) = account.session_root.or_else(|| cli.session_root.clone()) {
            builder = builder.session_root(root);
        }
        if let Some(password) = &single_account_password {
            builder = builder.password(password.clone());
        }
        let provider = Arc::new(FindMyProvider::new(builder.build()?));
        runtime.register_account(
            runtime_account_id(&account.username),
            provider,
            account.device_ids,
        );
    }

    if !cli.json && !cli.ndjson {
        println!("Tracking started; press Ctrl-C to stop cleanly");
    }
    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_sender.send(true);
        }
    });
    runtime.run(shutdown_receiver).await?;
    Ok(())
}

fn runtime_account_id(username: &str) -> String {
    let digest = Sha256::digest(username.trim().to_ascii_lowercase().as_bytes());
    let mut account_id = String::from("account-");
    for byte in &digest[..12] {
        write!(account_id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    account_id
}

fn runtime_config(config: &AppConfig) -> Result<RuntimeConfig> {
    let away_time_zone_offsets = config
        .away_time_zones
        .iter()
        .flat_map(|zone| {
            zone.device_ids
                .iter()
                .map(move |device_id| (device_id.clone(), zone.offset_hours))
        })
        .collect();
    let mut tracked_from_zones = config
        .tracked_from_zones
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(base_zone_id) = &config.base_zone_id {
        tracked_from_zones.insert(base_zone_id.clone());
    }
    Ok(RuntimeConfig {
        tick_interval: Duration::from_secs(config.tracking.tick_seconds),
        prefetch_window: Duration::from_secs(config.tracking.prefetch_seconds),
        default_update_interval: Duration::from_secs(config.tracking.default_interval_seconds),
        location_quality: icloud_location::tracking::LocationQualityPolicy {
            gps_accuracy_threshold_meters: config.tracking.gps_accuracy_threshold_meters,
            ..icloud_location::tracking::LocationQualityPolicy::default()
        },
        zones: icloud_location::tracking::ZoneSet::new(config.zones.clone())?,
        base_zone_id: config.base_zone_id.clone(),
        pass_through: icloud_location::tracking::PassThroughPolicy {
            delay_seconds: config.tracking.pass_through_delay_seconds,
            tracked_from_zones,
            ..icloud_location::tracking::PassThroughPolicy::default()
        },
        stationary: icloud_location::tracking::StationaryPolicy {
            enabled: config.tracking.stationary_enabled,
            still_seconds: config.tracking.stationary_still_seconds,
            radius_meters: config.tracking.stationary_radius_meters,
            ..icloud_location::tracking::StationaryPolicy::default()
        },
        maximum_update_interval: Duration::from_secs(config.tracking.maximum_interval_seconds),
        away_time_zone_offsets,
        old_location_adjustment_seconds: config.tracking.old_location_adjustment_seconds,
        old_location_maximum_seconds: (config.tracking.old_location_maximum_seconds > 0)
            .then_some(config.tracking.old_location_maximum_seconds),
        in_zone_interval: Duration::from_secs(config.tracking.in_zone_interval_seconds),
        stationary_interval: Duration::from_secs(config.tracking.stationary_interval_seconds),
        exit_zone_interval: Duration::from_secs(config.tracking.exit_zone_interval_seconds),
        fixed_interval: (config.tracking.fixed_interval_seconds > 0)
            .then(|| Duration::from_secs(config.tracking.fixed_interval_seconds)),
        travel_time_factor: config.tracking.travel_time_factor,
    })
}

fn region_from_config(region: AppleRegion) -> Region {
    match region {
        AppleRegion::Global => Region::Global,
        AppleRegion::ChinaGcj02 => Region::China {
            coordinates: ChinaCoordinates::Gcj02,
        },
        AppleRegion::ChinaBd09 => Region::China {
            coordinates: ChinaCoordinates::Bd09,
        },
        AppleRegion::ChinaWgs84 => Region::China {
            coordinates: ChinaCoordinates::Unchanged,
        },
    }
}

fn config_path(arguments: &ConfigPathArgs) -> Result<PathBuf> {
    arguments
        .config
        .clone()
        .map_or_else(default_config_path, Ok)
        .map_err(Into::into)
}

fn state_path(cli: &Cli, config_path: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(path) = &cli.state_file {
        return Ok(path.clone());
    }
    if let Some(path) = config_path {
        return Ok(path.with_file_name("tracking-state.json"));
    }
    if let Some(root) = &cli.session_root {
        return Ok(root.join("tracking-state.json"));
    }
    let config = default_config_path()?;
    Ok(config.with_file_name("tracking-state.json"))
}

struct ConsoleEventSink {
    json: bool,
}

#[cfg(feature = "waze")]
#[allow(clippy::too_many_lines)]
async fn waze_command(command: &WazeCommand, json: bool) -> Result<()> {
    match command {
        WazeCommand::Route(arguments) => {
            let client = WazeClient::new(WazeClientConfig {
                region: WazeRegion::from_icloud3_name(&arguments.region),
                real_time: !arguments.historical,
                minimum_distance_km: arguments.minimum_distance_km,
                maximum_distance_km: arguments.maximum_distance_km,
                request_timeout: Duration::from_secs(60),
            })?;
            let estimate = client
                .route(&RouteRequest {
                    origin: icloud_location::core::Coordinates::new(
                        arguments.from_latitude,
                        arguments.from_longitude,
                    )?,
                    destination: icloud_location::core::Coordinates::new(
                        arguments.to_latitude,
                        arguments.to_longitude,
                    )?,
                    departure: Utc::now(),
                })
                .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&estimate)?);
            } else {
                let duration = estimate
                    .duration_seconds
                    .map_or_else(|| "-".into(), |seconds| format!("{seconds} s"));
                println!(
                    "{}: {:.2} km, {duration} ({:?})",
                    estimate.provider, estimate.distance_km, estimate.status
                );
            }
            Ok(())
        }
        WazeCommand::History { command } => match command {
            WazeHistoryCommand::Stats { database } => {
                let count = SqliteRouteHistoryStore::open(database)?.record_count()?;
                if json {
                    println!("{}", serde_json::json!({ "record_count": count }));
                } else {
                    println!("{count} route-history record(s)");
                }
                Ok(())
            }
            WazeHistoryCommand::Maintain { database } => {
                let result = SqliteRouteHistoryStore::open(database)?.maintain().await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!(
                        "Removed {} duplicate record(s); updated {} record(s)",
                        result.removed_records, result.updated_records
                    );
                }
                Ok(())
            }
            WazeHistoryCommand::List {
                database,
                north_south,
            } => {
                let order = if *north_south {
                    RouteHistoryOrder::NorthSouth
                } else {
                    RouteHistoryOrder::EastWest
                };
                let entries = SqliteRouteHistoryStore::open(database)?.entries(order)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&entries)?);
                } else {
                    for entry in entries {
                        println!(
                            "{}\t{:.5},{:.5}\t{:.2} km\t{} use(s)",
                            entry.zone_id,
                            entry.destination.latitude,
                            entry.destination.longitude,
                            entry.estimate.distance_km,
                            entry.use_count
                        );
                    }
                }
                Ok(())
            }
            WazeHistoryCommand::Recalculate { database, config } => {
                let config = AppConfig::load(config)?;
                let waze = config
                    .waze
                    .as_ref()
                    .context("configuration does not enable Waze")?;
                let client = WazeClient::new(WazeClientConfig {
                    region: WazeRegion::from_icloud3_name(&waze.region),
                    real_time: waze.real_time,
                    minimum_distance_km: waze.minimum_distance_km,
                    maximum_distance_km: waze.maximum_distance_km,
                    request_timeout: Duration::from_secs(60),
                })?;
                let origins: BTreeMap<_, _> = config
                    .zones
                    .iter()
                    .map(|zone| Ok((zone.id.clone(), zone.center()?)))
                    .collect::<Result<_>>()?;
                let result = SqliteRouteHistoryStore::open(database)?
                    .recalculate(&client, &origins, Utc::now())
                    .await?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!(
                        "Recalculated {} of {} route(s); {} failed",
                        result.updated_records, result.examined_records, result.failed_records
                    );
                }
                Ok(())
            }
        },
    }
}

impl EventSink for ConsoleEventSink {
    fn emit(&self, event: &TrackingEvent) -> std::result::Result<(), EventSinkError> {
        if self.json {
            let encoded = serde_json::to_string(event).map_err(|error| EventSinkError {
                message: error.to_string(),
            })?;
            println!("{encoded}");
        } else {
            println!("{event:?}");
        }
        Ok(())
    }
}

struct SilentEventSink;

impl EventSink for SilentEventSink {
    fn emit(&self, _event: &TrackingEvent) -> std::result::Result<(), EventSinkError> {
        Ok(())
    }
}

async fn login(client: &mut ICloudClient, arguments: &LoginArgs, json: bool) -> Result<()> {
    let status = match client.authenticate().await {
        Err(Error::CredentialsRequired) => {
            let password = rpassword::prompt_password("Apple account password: ")?;
            if password.is_empty() {
                bail!("password cannot be empty");
            }
            client.set_password(password);
            client.authenticate().await?
        }
        result => result?,
    };

    match status {
        AuthenticationStatus::Authenticated(account) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&account)?);
            } else {
                println!(
                    "Authenticated {}{}",
                    account.username,
                    account
                        .name
                        .as_deref()
                        .map_or_else(String::new, |name| format!(" ({name})"))
                );
            }
            Ok(())
        }
        AuthenticationStatus::TermsOfUseRequired => {
            bail!("accept the updated terms at https://icloud.com, then run login again")
        }
        AuthenticationStatus::TwoFactorRequired(challenge) => {
            complete_two_factor(client, arguments, &challenge, json).await
        }
    }
}

async fn complete_two_factor(
    client: &mut ICloudClient,
    arguments: &LoginArgs,
    challenge: &TwoFactorChallenge,
    json: bool,
) -> Result<()> {
    if arguments.security_key {
        #[cfg(feature = "security-key")]
        {
            let mut authenticator = UsbSecurityKeyAuthenticator;
            let status = client
                .authenticate_with_security_key(&mut authenticator)
                .await?;
            return print_authentication_result(status, json);
        }
        #[cfg(not(feature = "security-key"))]
        bail!("rebuild with --features security-key to use a FIDO2 USB key");
    }
    if !challenge.security_key_names.is_empty() {
        eprintln!(
            "This account has security keys configured: {}. Use login --security-key in a security-key-enabled build, or verify with a trusted-device or SMS code.",
            challenge.security_key_names.join(", ")
        );
    }
    if !challenge.trusted_phone_numbers.is_empty() {
        eprintln!("Trusted SMS phone IDs:");
        for phone in &challenge.trusted_phone_numbers {
            let description = phone
                .number_with_dial_code
                .as_deref()
                .map(ToOwned::to_owned)
                .or_else(|| {
                    phone
                        .last_two_digits
                        .as_deref()
                        .map(|digits| format!("ending in {digits}"))
                })
                .unwrap_or_else(|| "number unavailable".into());
            eprintln!("  {} ({description})", phone.id);
        }
    }
    let method = if let Some(phone_id) = arguments.sms_phone_id {
        if !challenge.trusted_phone_numbers.is_empty()
            && !challenge
                .trusted_phone_numbers
                .iter()
                .any(|phone| phone.id == phone_id)
        {
            let available = challenge
                .trusted_phone_numbers
                .iter()
                .map(|phone| phone.id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("trusted phone ID {phone_id} is unavailable; choose one of: {available}");
        }
        VerificationMethod::Sms { phone_id }
    } else {
        VerificationMethod::TrustedDevice
    };

    client.request_verification_code(method).await?;
    let prompt = match method {
        VerificationMethod::TrustedDevice => "Code shown on a trusted Apple device: ",
        VerificationMethod::Sms { .. } => "Code sent by SMS: ",
    };
    let code = rpassword::prompt_password(prompt)?;
    let status = client.verify_verification_code(method, &code).await?;

    print_authentication_result(status, json)
}

fn print_authentication_result(status: AuthenticationStatus, json: bool) -> Result<()> {
    match status {
        AuthenticationStatus::Authenticated(account) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&account)?);
            } else {
                println!("Authenticated {}", account.username);
            }
            Ok(())
        }
        AuthenticationStatus::TermsOfUseRequired => {
            bail!("accept the updated terms at https://icloud.com, then run login again")
        }
        AuthenticationStatus::TwoFactorRequired(_) => {
            bail!("Apple still requires two-factor authentication")
        }
    }
}

async fn status(client: &mut ICloudClient, json: bool) -> Result<()> {
    match client.authenticate().await {
        Ok(status) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                match status {
                    AuthenticationStatus::Authenticated(account) => {
                        println!("Authenticated as {}", account.username);
                    }
                    AuthenticationStatus::TwoFactorRequired(_) => {
                        println!("Two-factor authentication required; run 'icloud-location login'");
                    }
                    AuthenticationStatus::TermsOfUseRequired => {
                        println!("Updated iCloud terms must be accepted at https://icloud.com");
                    }
                }
            }
            Ok(())
        }
        Err(Error::CredentialsRequired) => {
            bail!("no valid saved session; run 'icloud-location login'")
        }
        Err(error) => Err(error.into()),
    }
}

async fn require_authenticated(client: &mut ICloudClient) -> Result<()> {
    match client.authenticate().await {
        Ok(AuthenticationStatus::Authenticated(_)) => Ok(()),
        Ok(AuthenticationStatus::TwoFactorRequired(_)) => {
            bail!("two-factor authentication required; run 'icloud-location login'")
        }
        Ok(AuthenticationStatus::TermsOfUseRequired) => {
            bail!("accept the updated terms at https://icloud.com, then run login again")
        }
        Err(Error::CredentialsRequired) => {
            bail!("no valid saved session; run 'icloud-location login'")
        }
        Err(error) => Err(error.into()),
    }
}

fn locate_options(owner_only: bool) -> LocateOptions {
    if owner_only {
        LocateOptions::owner()
    } else {
        LocateOptions::family()
    }
}

fn select_devices(devices: Vec<Device>, selector: Option<&str>) -> Result<Vec<Device>> {
    let Some(selector) = selector else {
        return Ok(devices);
    };
    let selector_lowercase = selector.to_lowercase();
    let exact: Vec<_> = devices
        .iter()
        .filter(|device| device.id == selector || device.name.to_lowercase() == selector_lowercase)
        .collect();
    if exact.len() == 1 {
        let id = exact[0].id.clone();
        return Ok(devices
            .into_iter()
            .filter(|device| device.id == id)
            .collect());
    }
    if exact.len() > 1 {
        bail!("device selector '{selector}' matches multiple devices");
    }

    let prefix: Vec<_> = devices
        .into_iter()
        .filter(|device| device.id.starts_with(selector))
        .collect();
    match prefix.len() {
        0 => bail!("no device matches '{selector}'"),
        1 => Ok(prefix),
        _ => bail!("device ID prefix '{selector}' matches multiple devices"),
    }
}

fn print_devices(devices: &[Device], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(devices)?);
        return Ok(());
    }

    println!("ID\tNAME\tMODEL\tSTATUS\tBATTERY\tLOCATED");
    for device in devices {
        let battery = device
            .battery
            .as_ref()
            .and_then(|battery| battery.level_percent)
            .map_or_else(|| "-".into(), |level| format!("{level}%"));
        let located = device
            .location
            .as_ref()
            .and_then(|location| location.timestamp)
            .map_or_else(|| "-".into(), |timestamp| timestamp.to_rfc3339());
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            short_id(&device.id),
            device.name,
            device.model().unwrap_or("-"),
            device.status,
            battery,
            located
        );
    }
    Ok(())
}

fn print_locations(devices: &[Device], json: bool) -> Result<()> {
    let located: Vec<_> = devices
        .iter()
        .filter(|device| device.location.is_some())
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&located)?);
        return Ok(());
    }
    if located.is_empty() {
        bail!("none of the selected devices returned a location");
    }

    println!("ID\tNAME\tLATITUDE\tLONGITUDE\tACCURACY\tTIMESTAMP\tBATTERY");
    for device in located {
        let location = device
            .location
            .as_ref()
            .context("location was filtered above")?;
        let accuracy = location
            .horizontal_accuracy_meters
            .map_or_else(|| "-".into(), |meters| format!("{meters:.0} m"));
        let timestamp = location
            .timestamp
            .map_or_else(|| "-".into(), |timestamp| timestamp.to_rfc3339());
        let battery = device
            .battery
            .as_ref()
            .and_then(|battery| battery.level_percent)
            .map_or_else(|| "-".into(), |level| format!("{level}%"));
        println!(
            "{}\t{}\t{:.6}\t{:.6}\t{}\t{}\t{}",
            short_id(&device.id),
            device.name,
            location.latitude,
            location.longitude,
            accuracy,
            timestamp,
            battery
        );
    }
    Ok(())
}

fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use icloud_location::DeviceStatus;
    use serde_json::Value;

    use super::*;

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            unique_name: name.into(),
            device_class: None,
            device_display_name: None,
            model_display_name: None,
            raw_device_model: None,
            status: DeviceStatus::Unknown,
            battery: None,
            location: None,
            family_shared: None,
            raw: Value::Null,
        }
    }

    #[test]
    fn selects_by_case_insensitive_name() {
        let devices = vec![device("abcdef123", "Jimmy's iPhone")];
        let selected = select_devices(devices, Some("JIMMY'S IPHONE")).unwrap();
        assert_eq!(selected[0].id, "abcdef123");
    }

    #[test]
    fn rejects_ambiguous_id_prefix() {
        let devices = vec![device("abcdef123", "Phone"), device("abcdef456", "Watch")];
        let error = select_devices(devices, Some("abcdef")).unwrap_err();
        assert!(error.to_string().contains("multiple"));
    }

    #[test]
    fn parses_platform_neutral_runtime_and_session_commands() {
        let snapshot = Cli::try_parse_from(["icloud-location", "snapshot"]).unwrap();
        assert!(matches!(snapshot.command, Command::Snapshot));

        let watch =
            Cli::try_parse_from(["icloud-location", "track", "--config", "test.toml"]).unwrap();
        assert!(matches!(watch.command, Command::Watch(_)));

        let credentials =
            Cli::try_parse_from(["icloud-location", "session", "validate-credentials"]).unwrap();
        assert!(matches!(
            credentials.command,
            Command::Session {
                command: SessionCommand::ValidateCredentials
            }
        ));

        let ingest =
            Cli::try_parse_from(["icloud-location", "ingest", "--input", "updates.ndjson"])
                .unwrap();
        assert!(matches!(ingest.command, Command::Ingest(_)));

        let schedule = Cli::try_parse_from([
            "icloud-location",
            "schedule",
            "device-id",
            "--at",
            "2026-08-19T18:30:00Z",
        ])
        .unwrap();
        assert!(matches!(schedule.command, Command::Schedule(_)));
    }

    #[test]
    fn parses_the_external_fixture_as_cli_input() {
        let updates = parse_external_updates(include_str!(
            "../tests/fixtures/external/location_updates.json"
        ))
        .unwrap();

        assert_eq!(updates.len(), 2);
    }

    #[test]
    fn derives_a_stable_non_identifying_runtime_account_id() {
        let account_id = runtime_account_id(" Person@Example.invalid ");

        assert_eq!(account_id, runtime_account_id("person@example.invalid"));
        assert!(account_id.starts_with("account-"));
        assert!(!account_id.contains("person"));
        assert!(!account_id.contains('@'));
    }

    #[test]
    fn lost_mode_requires_explicit_confirmation_flag_to_be_present_in_arguments() {
        let command = Cli::try_parse_from([
            "icloud-location",
            "lost-mode",
            "device-id",
            "--phone-number",
            "+46000000000",
        ])
        .unwrap();
        let Command::LostMode(arguments) = command.command else {
            panic!("expected lost-mode command");
        };
        assert!(!arguments.confirm);
    }

    #[cfg(feature = "waze")]
    #[test]
    fn parses_waze_history_recalculation_command() {
        let command = Cli::try_parse_from([
            "icloud-location",
            "waze",
            "history",
            "recalculate",
            "routes.sqlite",
            "--config",
            "config.toml",
        ])
        .unwrap();
        assert!(matches!(
            command.command,
            Command::Waze {
                command: WazeCommand::History {
                    command: WazeHistoryCommand::Recalculate { .. }
                }
            }
        ));
    }
}
