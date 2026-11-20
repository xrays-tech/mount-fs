//! `mount-fs` — a Linux CLI that mounts cloud drives by orchestrating existing
//! FUSE tools (s3fs-fuse, aliyundrive-fuse, qrypt).
//!
//! This is the thin CLI wrapper; most logic lives in the `mount_fs` library.

use std::fs;
use std::io::{IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use clap::Parser;
use dialoguer::{Input, Password};
use tracing_subscriber::EnvFilter;

use mount_fs::cli::{
    Cli, Commands, ConfigAction, InstallMethodArg, ProfileAction, ProviderArg, ServiceAction,
};
use mount_fs::config::{Config, ProviderKind};
use mount_fs::core::config_ux;
use mount_fs::core::creds;
use mount_fs::core::install::{self, InstallRequest};
use mount_fs::core::orchestrator::{self, MountOptionsArg};
use mount_fs::core::state::{self, StateRegistry};
use mount_fs::error::{AppError, Result};
use mount_fs::providers::registry;
use mount_fs::system::fuse::RealMountSource;
use mount_fs::system::{distro, pkg, systemd};

fn main() {
    let cli = Cli::parse();

    init_logging(&cli);

    if let Err(err) = run(cli.command, cli.config) {
        // Wrap in `anyhow` to print the full error chain for the user.
        let err = anyhow::Error::new(err);
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

/// Initialize `tracing` based on CLI verbosity and the environment.
///
/// `RUST_LOG` takes precedence when set; otherwise the level is derived from
/// `--verbose`/`--quiet`.
fn init_logging(cli: &Cli) {
    let level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let mut builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    if cli.no_color {
        builder = builder.with_ansi(false);
    }

    builder.init();
}

/// Dispatch a parsed command to its implementation.
fn run(command: Option<Commands>, config_path: Option<PathBuf>) -> Result<()> {
    match command {
        Some(Commands::Version) => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Commands::Doctor { fix }) => mount_fs::system::doctor(fix),
        Some(Commands::Providers) => cmd_providers(),
        Some(Commands::Install {
            engine,
            method,
            dry_run,
            yes,
        }) => cmd_install(&engine, method, dry_run, yes),
        Some(Commands::Uninstall { engine }) => cmd_uninstall(&engine),
        Some(Commands::Config { action }) => cmd_config(action, config_path.as_deref()),
        Some(Commands::Mount {
            target,
            mountpoint,
            bucket,
            endpoint,
            engine,
            ro,
            rw,
            allow_other,
            foreground,
            dry_run,
        }) => cmd_mount(
            &target,
            config_path.as_deref(),
            MountOptionsArg {
                mountpoint,
                engine,
                read_only: if ro {
                    Some(true)
                } else if rw {
                    Some(false)
                } else {
                    None
                },
                allow_other: allow_other.then_some(true),
                foreground,
                dry_run,
                bucket,
                endpoint,
            },
        ),
        Some(Commands::Umount { target, force, all }) => cmd_umount(&target, force, all),
        Some(Commands::List { json }) => cmd_list(json, config_path.as_deref()),
        Some(Commands::Status { profile, json }) => {
            cmd_status(&profile, config_path.as_deref(), json)
        }
        Some(Commands::Service { action }) => cmd_service(action, config_path.as_deref()),
        Some(Commands::Logs { profile, follow }) => cmd_logs(&profile, follow),
        None => {
            println!(
                "{} {} — run `mount-fs --help` for usage",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
    }
}

fn cmd_mount(target: &str, config_path: Option<&Path>, opts: MountOptionsArg) -> Result<()> {
    orchestrator::mount(&RealMountSource, config_path, target, opts)
}

fn cmd_umount(target: &str, force: bool, all: bool) -> Result<()> {
    orchestrator::umount(&RealMountSource, target, force, all)
}

fn cmd_list(json: bool, config_path: Option<&Path>) -> Result<()> {
    let rows = orchestrator::list_with_config(&RealMountSource, config_path)?;

    if json {
        println!("{}", orchestrator::json_list(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No managed or external FUSE mounts.");
        return Ok(());
    }

    let registry = StateRegistry::load().unwrap_or_default();
    println!(
        "{:<20} {:<10} {:<12} {:<32} STATUS",
        "PROFILE", "PROVIDER", "ENGINE", "MOUNTPOINT"
    );
    for row in &rows {
        let (provider, engine) = row
            .profile
            .as_deref()
            .and_then(|profile| registry.get(profile))
            .map(|record| (record.provider.clone(), record.engine.clone()))
            .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
        println!(
            "{:<20} {:<10} {:<12} {:<32} {}",
            row.profile.as_deref().unwrap_or("-"),
            provider,
            engine,
            row.mountpoint.display(),
            row.status
        );
    }
    Ok(())
}

fn cmd_status(profile: &str, config_path: Option<&Path>, json: bool) -> Result<()> {
    let row = orchestrator::status_with_config(&RealMountSource, profile, config_path)?;
    if json {
        println!("{}", orchestrator::json_status(&row)?);
        return Ok(());
    }
    println!("profile:    {}", row.profile.as_deref().unwrap_or("-"));
    println!("mountpoint: {}", row.mountpoint.display());
    println!("status:     {}", row.status);
    println!("source:     {}", row.source.as_deref().unwrap_or("-"));
    println!("fstype:     {}", row.fstype.as_deref().unwrap_or("-"));
    if let Some(pid) = row.pid {
        println!("pid:        {pid}");
    }
    Ok(())
}

fn cmd_logs(profile: &str, follow: bool) -> Result<()> {
    let path = StateRegistry::load()
        .unwrap_or_default()
        .get(profile)
        .and_then(|record| record.log_path.clone())
        .map(Ok)
        .unwrap_or_else(|| state::default_log_path(profile))?;

    if !path.exists() {
        if !follow {
            println!(
                "No log file for `{profile}` yet (expected {}).",
                path.display()
            );
            return Ok(());
        }
        println!("Waiting for log file {} ...", path.display());
        while !path.exists() {
            thread::sleep(Duration::from_millis(250));
        }
    }

    let mut file = fs::File::open(&path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    print!("{contents}");

    if !follow {
        return Ok(());
    }

    let mut position = contents.len() as u64;
    loop {
        thread::sleep(Duration::from_millis(250));
        let length = file.metadata()?.len();
        if length > position {
            file.seek(SeekFrom::Start(position))?;
            let mut chunk = String::new();
            file.read_to_string(&mut chunk)?;
            print!("{chunk}");
            std::io::stdout().flush()?;
            position = length;
        }
    }
}

fn cmd_service(action: ServiceAction, config_path: Option<&Path>) -> Result<()> {
    match action {
        ServiceAction::Generate {
            profile,
            exec,
            force,
            dry_run,
        } => {
            let config = match config_path {
                Some(path) => Config::load(path)?,
                None => Config::load_default()?,
            };
            let configured = config
                .profile(&profile)
                .ok_or_else(|| AppError::NotFound(format!("profile `{profile}`")))?;
            let mountpoint = configured.mountpoint.clone();

            let exec = match exec {
                Some(path) => absolute_exec(&path)?,
                None => std::env::current_exe()?,
            };

            systemd::generate(&profile, &mountpoint, &exec, force, dry_run)?;
            println!(
                "Hint: systemctl --user daemon-reload && systemctl --user enable --now mount-fs-{profile}.service"
            );

            match &configured.credentials_ref {
                Some(path) if path.exists() => {}
                Some(path) => println!(
                    "[WARN] credentials_ref {} does not exist; the service will fail to mount until it is created",
                    path.display()
                ),
                None => println!(
                    "[WARN] profile `{profile}` has no credentials_ref; systemd services do not inherit shell env. Run `mount-fs config login <provider> --profile {profile}` first."
                ),
            }
            Ok(())
        }
        ServiceAction::List { json } => {
            let units = systemd::list_units()?;
            if json {
                let text = serde_json::to_string_pretty(&units)
                    .map_err(|err| AppError::Config(format!("cannot serialize services: {err}")))?;
                println!("{text}");
            } else if units.is_empty() {
                println!("No generated services.");
            } else {
                for unit in units {
                    println!("{}  {}", unit.profile, unit.path.display());
                }
            }
            Ok(())
        }
        ServiceAction::Remove { profile } => {
            let path = systemd::remove(&profile)?;
            println!("Removed {}", path.display());
            Ok(())
        }
    }
}

fn absolute_exec(path: &Path) -> Result<PathBuf> {
    if !path.exists() {
        return Err(AppError::NotFound(format!("executable {}", path.display())));
    }
    Ok(fs::canonicalize(path)?)
}

fn cmd_providers() -> Result<()> {
    let providers = registry();
    if providers.is_empty() {
        println!("No providers registered.");
        return Ok(());
    }

    for provider in providers {
        println!("Provider: {}", provider.id());
        for engine in provider.engines() {
            let detected = engine.detect();
            if detected.found {
                let version = detected
                    .version
                    .as_ref()
                    .map(|version| format!(" v{version}"))
                    .unwrap_or_default();
                let path = detected
                    .path
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "unknown path".to_string());
                println!("  [ OK ] {}{} ({})", engine.id(), version, path);
            } else {
                println!("  [WARN] {} not found on PATH", engine.id());
            }

            let capabilities = engine.capabilities();
            println!(
                "         capabilities: read_write={} allow_other={} foreground={}",
                capabilities.read_write, capabilities.allow_other, capabilities.foreground
            );
            println!(
                "         token_kind={} credential={}",
                engine.token_kind(),
                engine.credential_retention()
            );
        }
    }
    Ok(())
}

fn cmd_install(engine_id: &str, method: InstallMethodArg, dry_run: bool, yes: bool) -> Result<()> {
    let request = match method {
        InstallMethodArg::Auto => InstallRequest::Auto,
        InstallMethodArg::Pkg => InstallRequest::Pkg,
        InstallMethodArg::Release => InstallRequest::Release,
        InstallMethodArg::Source => InstallRequest::Source,
    };
    let detected_distro = distro::detect();
    let manager = pkg::PackageManager::detect();

    for provider in registry() {
        for engine in provider.engines() {
            if engine.id() == engine_id {
                return install::install_engine(
                    engine.as_ref(),
                    &detected_distro,
                    manager,
                    request,
                    dry_run,
                    yes,
                );
            }
        }
    }

    Err(AppError::NotFound(format!(
        "engine `{engine_id}` is not registered"
    )))
}

fn cmd_uninstall(engine_id: &str) -> Result<()> {
    println!("{engine_id}: package-manager-managed engines are not removed by mount-fs.");
    match pkg::PackageManager::detect() {
        pkg::PackageManager::Unknown => {
            println!("No supported package manager detected; remove the engine manually.");
        }
        manager => match remove_argv(manager, engine_id) {
            Some(argv) => println!("To remove it, run: {}", argv.join(" ")),
            None => println!("Remove it manually."),
        },
    }
    Ok(())
}

fn remove_argv(manager: pkg::PackageManager, package: &str) -> Option<Vec<String>> {
    let argv = match manager {
        pkg::PackageManager::Dnf => vec!["dnf", "remove", "-y", package],
        pkg::PackageManager::Apt => vec!["apt", "remove", "-y", package],
        pkg::PackageManager::Pacman => vec!["pacman", "-Rns", "--noconfirm", package],
        pkg::PackageManager::Zypper => vec!["zypper", "-n", "remove", package],
        pkg::PackageManager::Apk => vec!["apk", "del", package],
        pkg::PackageManager::Unknown => return None,
    };
    Some(argv.into_iter().map(str::to_string).collect())
}

fn cmd_config(action: ConfigAction, path: Option<&Path>) -> Result<()> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => mount_fs::config::default_config_path()?,
    };

    match action {
        ConfigAction::Init => cmd_config_init(&path),
        ConfigAction::List => cmd_config_list(&path),
        ConfigAction::Get { key } => cmd_config_get(&path, &key),
        ConfigAction::Set { key, value } => cmd_config_set(&path, &key, &value),
        ConfigAction::Profile { action } => cmd_config_profile(&path, action),
        ConfigAction::Show { name } => {
            let config = Config::load(&path)?;
            print!("{}", config_ux::render_config(&config, name.as_deref())?);
            Ok(())
        }
        ConfigAction::Validate { name } => cmd_config_validate(&path, name.as_deref()),
        ConfigAction::Login {
            provider,
            profile,
            drive_id,
        } => cmd_config_login(
            &path,
            provider.as_deref(),
            profile.as_deref(),
            drive_id.as_deref(),
        ),
        ConfigAction::Logout { provider, profile } => {
            cmd_config_logout(&path, provider.as_deref(), profile.as_deref())
        }
    }
}

fn cmd_config_profile(path: &Path, action: ProfileAction) -> Result<()> {
    match action {
        ProfileAction::Add {
            name,
            provider,
            engine,
            mountpoint,
            bucket,
            endpoint,
            region,
            path_style,
            drive_id,
            allow_other,
            read_only,
            force,
        } => {
            let provider = match provider {
                ProviderArg::S3 => ProviderKind::S3,
                ProviderArg::Aliyun => ProviderKind::Aliyun,
            };
            config_ux::add_profile(
                path,
                &name,
                config_ux::NewProfile {
                    provider,
                    engine,
                    mountpoint,
                    bucket,
                    endpoint,
                    region,
                    path_style,
                    drive_id,
                    allow_other,
                    read_only,
                },
                force,
            )
        }
        ProfileAction::Remove { name } => config_ux::remove_profile(path, &name),
        ProfileAction::Set { name, assignments } => {
            config_ux::set_profile_fields(path, &name, &assignments)
        }
    }
}

fn cmd_config_login(
    config_path: &Path,
    provider: Option<&str>,
    profile_name: Option<&str>,
    drive_id: Option<&str>,
) -> Result<()> {
    let profile_name = profile_name
        .ok_or_else(|| AppError::Config("config login requires --profile <name>".to_string()))?;

    match provider {
        Some("s3") => {
            let (access, secret) = read_s3_credentials_interactive()?;
            let path = config_ux::login_s3(config_path, profile_name, &access, &secret)?;
            println!(
                "Stored S3 credentials for `{profile_name}` at {}",
                path.display()
            );
            Ok(())
        }
        Some("aliyun") => {
            let token = read_secret_line("Aliyun refresh token")?;
            if token.is_empty() {
                return Err(AppError::Credential(
                    "refusing to store an empty token".to_string(),
                ));
            }
            let token_path = creds::store_aliyun_token(profile_name, &token)?;
            println!("Stored token at {}", token_path.display());

            let mut config = Config::load(config_path)?;
            if let Some(profile) = config.profiles.get_mut(profile_name) {
                profile.credentials_ref = Some(token_path.clone());
                if let Some(drive_id) = drive_id {
                    profile.drive_id = Some(drive_id.to_string());
                }
                config.save_atomic(config_path)?;
                println!("Updated credentials_ref for profile `{profile_name}`");
            } else {
                println!(
                    "Profile `{profile_name}` is not in {}; add `credentials_ref = \"{}\"` to use it.",
                    config_path.display(),
                    token_path.display()
                );
            }
            Ok(())
        }
        other => Err(AppError::Unsupported(format!(
            "config login supports `s3` and `aliyun` (got {})",
            other.unwrap_or("none")
        ))),
    }
}

fn cmd_config_logout(
    config_path: &Path,
    provider: Option<&str>,
    profile_name: Option<&str>,
) -> Result<()> {
    let provider = provider.ok_or_else(|| {
        AppError::Config("config logout requires a provider (`s3` or `aliyun`)".to_string())
    })?;
    let profile_name = profile_name
        .ok_or_else(|| AppError::Config("config logout requires --profile <name>".to_string()))?;

    let removed = config_ux::logout(config_path, provider, profile_name)?;
    if removed.is_empty() {
        println!("No stored credential file found for `{profile_name}`");
    } else {
        for path in removed {
            println!("Removed {}", path.display());
        }
    }
    println!("Cleared credentials_ref for `{profile_name}`");
    Ok(())
}

fn cmd_config_validate(path: &Path, name: Option<&str>) -> Result<()> {
    let report = config_ux::validate_report(path, name)?;
    if report.is_empty() {
        println!("No profiles configured.");
        return Ok(());
    }

    let mut failures = 0;
    for (profile, error) in &report {
        match error {
            None => println!("[ OK ] {profile}"),
            Some(error) => {
                failures += 1;
                println!("[FAIL] {profile}: {error}");
            }
        }
    }

    if failures == 0 {
        Ok(())
    } else {
        Err(AppError::Unsupported(format!(
            "{failures} of {} profiles failed validation",
            report.len()
        )))
    }
}

fn read_secret_line(prompt: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        return Password::new()
            .with_prompt(prompt)
            .interact()
            .map_err(|err| AppError::Credential(err.to_string()));
    }
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn read_plain_line(prompt: &str) -> Result<String> {
    if std::io::stdin().is_terminal() {
        return Input::<String>::new()
            .with_prompt(prompt)
            .interact_text()
            .map_err(|err| AppError::Credential(err.to_string()));
    }
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn read_s3_credentials_interactive() -> Result<(String, String)> {
    let access = read_plain_line("AWS access key id")?;
    let secret = read_secret_line("AWS secret access key")?;
    if access.is_empty() || secret.is_empty() {
        return Err(AppError::Credential(
            "AWS access key id and secret access key must not be empty".to_string(),
        ));
    }
    Ok((access, secret))
}

fn cmd_config_init(path: &Path) -> Result<()> {
    if path.exists() {
        println!("Config already exists: {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(path, mount_fs::config::CONFIG_TEMPLATE)?;
    println!("Created {}", path.display());
    Ok(())
}

fn cmd_config_list(path: &Path) -> Result<()> {
    let config = Config::load(path)?;
    if config.profiles.is_empty() {
        println!("No profiles configured in {}", path.display());
        return Ok(());
    }

    println!("Profiles in {}:", path.display());
    for (name, profile) in &config.profiles {
        println!(
            "  {name}: provider={} mountpoint={}",
            profile.provider,
            profile.mountpoint.display()
        );
    }
    Ok(())
}

fn cmd_config_get(path: &Path, key: &str) -> Result<()> {
    let config = Config::load(path)?;
    println!("{}", config_ux::get_value(&config, key)?);
    Ok(())
}

fn cmd_config_set(path: &Path, key: &str, value: &str) -> Result<()> {
    if config_ux::is_secret_key(key) {
        return Err(AppError::Config(format!(
            "refusing to set `{key}` from argv; use `config login` and a credentials file"
        )));
    }

    let mut config = Config::load(path)?;
    match key {
        "general.mount_root" => config.general.mount_root = PathBuf::from(value),
        "general.allow_other" => config.general.allow_other = parse_bool(value)?,
        "general.engine_auto_fallback" => config.general.engine_auto_fallback = parse_bool(value)?,
        "general.log_level" => {
            config.general.log_level = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
        }
        other => {
            return Err(AppError::NotFound(format!(
                "config key `{other}` is not settable in M4"
            )))
        }
    }

    config.save_atomic(path)?;
    println!("Set {key}");
    Ok(())
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(AppError::Config(format!(
            "expected a boolean value, got `{value}`"
        ))),
    }
}
