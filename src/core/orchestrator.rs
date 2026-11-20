//! Mount lifecycle orchestration.
//!
//! Builds and executes engine commands, tracks state, and reconciles against
//! the kernel mount table. All mount-table access goes through
//! [`MountSource`], which keeps tests hermetic.

use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{expand_tilde, Config, MountOptions, Profile, ProviderKind};
use crate::core::creds;
use crate::core::install::render_command;
use crate::core::reconcile::{reconcile, MountStatus, Reconciled};
use crate::core::state::{self, MountRecord, StateRegistry};
use crate::engines::{CredentialRetention, EffectiveMount, MountSecrets, TokenKind};
use crate::error::{AppError, Result};
use crate::providers;
use crate::system::fuse::{self, MountSource};
use crate::system::locking::FileLock;
use crate::system::process::{self, CommandSpec};

const MOUNT_TIMEOUT: Duration = Duration::from_secs(5);
const UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// CLI overrides for a mount.
#[derive(Debug, Clone, Default)]
pub struct MountOptionsArg {
    /// Override the mountpoint.
    pub mountpoint: Option<PathBuf>,
    /// Override the engine id.
    pub engine: Option<String>,
    /// Override read-only mode.
    pub read_only: Option<bool>,
    /// Override `allow_other`.
    pub allow_other: Option<bool>,
    /// Run in the foreground.
    pub foreground: bool,
    /// Print the command without executing it.
    pub dry_run: bool,
    /// Ad-hoc bucket name (when `target` is a provider id).
    pub bucket: Option<String>,
    /// Ad-hoc endpoint (when `target` is a provider id).
    pub endpoint: Option<String>,
}

/// Mount a profile (or an ad-hoc provider target).
pub fn mount(
    source: &dyn MountSource,
    config_path: Option<&Path>,
    target: &str,
    opts: MountOptionsArg,
) -> Result<()> {
    let config = match config_path {
        Some(path) => Config::load(path)?,
        None => Config::load_default()?,
    };
    let (profile_name, profile) = resolve_profile(&config, target, &opts)?;

    let provider = providers::registry()
        .into_iter()
        .find(|provider| provider.id() == profile.provider.to_string())
        .ok_or_else(|| {
            AppError::Unsupported(format!(
                "no provider implementation for `{}`",
                profile.provider
            ))
        })?;
    provider.validate(&profile)?;
    let engine = select_engine(
        provider.as_ref(),
        &profile,
        config.general.engine_auto_fallback,
    )?;

    let detected = engine.detect();
    if !detected.found {
        return Err(AppError::NotFound(format!(
            "engine `{}` is not installed; run: mount-fs install {}",
            engine.id(),
            engine.id()
        )));
    }
    if let (Some(requirement), Some(version)) = (engine.min_version(), detected.version.as_ref()) {
        if !requirement.matches(version) {
            return Err(AppError::Unsupported(format!(
                "engine `{}` version {version} does not satisfy {requirement}",
                engine.id()
            )));
        }
    }

    if !profile.read_only && !engine.capabilities().read_write {
        return Err(AppError::Unsupported(format!(
            "engine `{}` is read-only; use --ro or --engine qrypt for a writable mount",
            engine.id()
        )));
    }

    // S3 `credentials_ref` is not honored yet (M4); Aliyun credentials_ref is.
    static CREDS_REF_WARNING: std::sync::Once = std::sync::Once::new();
    if engine.token_kind() == TokenKind::AwsKey && profile.credentials_ref.is_some() {
        CREDS_REF_WARNING.call_once(|| {
            tracing::warn!(
                "S3 `credentials_ref` is not honored until M4; using env/interactive credentials"
            )
        });
    }

    let secrets = resolve_secrets(engine.as_ref(), &profile)?;

    let dirs = creds::RuntimeDirs::resolve()?;

    // Hold a per-profile mount lock from before the idempotency check until the
    // state record is saved, so concurrent mounts cannot double-spawn an engine.
    let lock_key = format!(
        "{}-{}",
        creds::sanitize_profile(&profile_name),
        creds::profile_nonce(&profile_name)
    );
    let lock_path = dirs.runtime.join(format!("mount-{lock_key}.lock"));
    let _mount_lock = match FileLock::try_acquire(&lock_path) {
        Ok(lock) => lock,
        Err(err) if err.kind() == ErrorKind::WouldBlock => {
            // Another mount holds the lock: never block or double-spawn.
            let rows = list(source)?;
            if let Some(row) = rows
                .iter()
                .find(|row| row.profile.as_deref() == Some(profile_name.as_str()))
            {
                print_reconciled(row);
                println!(
                    "another mount for `{profile_name}` is in progress; reporting current state"
                );
            } else {
                println!("another mount for `{profile_name}` is already in progress");
            }
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let rows = list(source)?;

    let active: Vec<String> = rows
        .iter()
        .filter(|row| row.status == MountStatus::Healthy)
        .filter_map(|row| row.profile.clone())
        .collect();
    creds::gc(&dirs, &active)?;

    if let Some(row) = rows.iter().find(|row| {
        row.profile.as_deref() == Some(profile_name.as_str()) && row.status == MountStatus::Healthy
    }) {
        print_reconciled(row);
        println!("already mounted at {}", row.mountpoint.display());
        return Ok(());
    }

    let requested = profile.mountpoint.clone();
    if !requested.exists() {
        // Only set the mode at creation; never chmod a pre-existing directory.
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o755);
        builder.create(&requested)?;
    }
    // Canonicalize so the kernel mount target (mountinfo) matches. A missing
    // path falls back to an absolute path instead of panicking.
    let mountpoint = match fs::canonicalize(&requested) {
        Ok(path) => path,
        Err(_) => std::path::absolute(&requested).unwrap_or(requested.clone()),
    };

    if profile.allow_other {
        match fuse::user_allow_other_set() {
            Some(true) => {}
            Some(false) => tracing::warn!(
                "allow_other requested but /etc/fuse.conf does not set user_allow_other"
            ),
            None => tracing::warn!(
                "allow_other requested but /etc/fuse.conf is missing; the kernel may reject it"
            ),
        }
    }

    let effective = EffectiveMount {
        mountpoint: mountpoint.clone(),
        allow_other: profile.allow_other,
        read_only: profile.read_only,
        profile_name: profile_name.clone(),
        runtime_dir: dirs.runtime.clone(),
    };
    let plan = engine.plan_mount(&profile, &effective, &secrets)?;
    let credential_path = plan.credential_path.clone();

    let mut spec = CommandSpec::new(plan.program.clone()).args(plan.argv.clone());
    for (key, value) in &plan.env {
        spec = spec.env(key.clone(), value.clone());
    }

    if opts.dry_run {
        let mut display = vec![plan.program.display().to_string()];
        display.extend(plan.argv.iter().cloned());
        println!("Mount command: {}", render_command(&display));
        println!("Dry run: not executing; credentials are not persisted.");
        cleanup_credential(&credential_path);
        return Ok(());
    }

    if opts.foreground {
        return mount_foreground(
            &spec,
            &profile_name,
            &profile,
            engine.id(),
            &mountpoint,
            credential_path,
        );
    }

    let log_path = state::default_log_path(&profile_name)?;
    mount_background(BackgroundSpec {
        source,
        spec: &spec,
        profile_name: &profile_name,
        profile: &profile,
        engine_id: engine.id(),
        mountpoint: &mountpoint,
        log_path: &log_path,
        credential_path: &credential_path,
    })
}

struct BackgroundSpec<'a> {
    source: &'a dyn MountSource,
    spec: &'a CommandSpec,
    profile_name: &'a str,
    profile: &'a Profile,
    engine_id: &'a str,
    mountpoint: &'a Path,
    log_path: &'a Path,
    credential_path: &'a Option<PathBuf>,
}

fn mount_background(ctx: BackgroundSpec<'_>) -> Result<()> {
    let BackgroundSpec {
        source,
        spec,
        profile_name,
        profile,
        engine_id,
        mountpoint,
        log_path,
        credential_path,
    } = ctx;

    let mut child = process::spawn_background_child(spec, log_path)?;
    let pid = child.id();

    if let Err(err) = wait_for_mount(source, mountpoint, &mut child, log_path) {
        let _ = child.kill();
        let _ = child.wait();
        cleanup_credential(credential_path);
        return Err(err);
    }

    let starttime = process::pid_starttime(pid);
    let record = MountRecord {
        profile: profile_name.to_string(),
        provider: profile.provider.to_string(),
        engine: engine_id.to_string(),
        mountpoint: mountpoint.to_path_buf(),
        source: profile
            .bucket
            .clone()
            .or_else(|| profile.drive_id.clone())
            .unwrap_or_else(|| profile_name.to_string()),
        pid: Some(pid),
        starttime,
        log_path: Some(log_path.to_path_buf()),
        credential_path: credential_path.clone(),
        mounted_at: state::now_epoch_secs(),
    };
    if let Err(err) = state::update_registry(|registry| {
        registry.insert(record);
        Ok(())
    }) {
        let _ = child.kill();
        let _ = child.wait();
        cleanup_credential(credential_path);
        return Err(err);
    }

    // Keep the credential artifact: it must live as long as the mount does.
    // Detach: the engine keeps running after this process exits.
    drop(child);

    println!(
        "Mounted {} at {} (pid {pid})",
        profile_name,
        mountpoint.display()
    );
    Ok(())
}

fn mount_foreground(
    spec: &CommandSpec,
    profile_name: &str,
    profile: &Profile,
    engine_id: &str,
    mountpoint: &Path,
    credential_path: Option<PathBuf>,
) -> Result<()> {
    let interrupted = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(
        signal_hook::consts::signal::SIGINT,
        Arc::clone(&interrupted),
    )
    .map_err(|err| AppError::Unsupported(format!("cannot install SIGINT handler: {err}")))?;
    signal_hook::flag::register(
        signal_hook::consts::signal::SIGTERM,
        Arc::clone(&interrupted),
    )
    .map_err(|err| AppError::Unsupported(format!("cannot install SIGTERM handler: {err}")))?;

    let mut child = spec.to_command().spawn()?;
    let pid = child.id();
    let starttime = process::pid_starttime(pid);

    let record = MountRecord {
        profile: profile_name.to_string(),
        provider: profile.provider.to_string(),
        engine: engine_id.to_string(),
        mountpoint: mountpoint.to_path_buf(),
        source: profile
            .bucket
            .clone()
            .or_else(|| profile.drive_id.clone())
            .unwrap_or_else(|| profile_name.to_string()),
        pid: Some(pid),
        starttime,
        log_path: None,
        credential_path: credential_path.clone(),
        mounted_at: state::now_epoch_secs(),
    };
    if let Err(err) = state::update_registry(|registry| {
        registry.insert(record);
        Ok(())
    }) {
        let _ = child.kill();
        let _ = child.wait();
        cleanup_credential(&credential_path);
        return Err(err);
    }

    println!(
        "Mounted {} at {} in the foreground (pid {pid})",
        profile_name,
        mountpoint.display()
    );

    loop {
        if interrupted.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_foreground(profile_name, &credential_path);
            eprintln!("interrupted; shutting down");
            std::process::exit(130);
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                cleanup_foreground(profile_name, &credential_path);
                if !status.success() {
                    return Err(AppError::Unsupported(format!(
                        "engine exited with {status}"
                    )));
                }
                return Ok(());
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(err) => {
                let _ = child.kill();
                cleanup_foreground(profile_name, &credential_path);
                return Err(err.into());
            }
        }
    }
}

/// Remove the foreground state record and credential artifact (best effort).
fn cleanup_foreground(profile_name: &str, credential_path: &Option<PathBuf>) {
    let _ = state::update_registry(|registry| {
        registry.remove(profile_name);
        Ok(())
    });
    cleanup_credential(credential_path);
}

/// Best-effort removal of an engine credential artifact.
fn cleanup_credential(credential_path: &Option<PathBuf>) {
    if let Some(path) = credential_path {
        creds::remove_artifact(path);
    }
}

fn wait_for_mount(
    source: &dyn MountSource,
    mountpoint: &Path,
    child: &mut std::process::Child,
    log_path: &Path,
) -> Result<()> {
    let deadline = Instant::now() + MOUNT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() && mount_present(source, mountpoint) {
                return Ok(());
            }
            let tail = process::read_log_tail(log_path, 20);
            let suffix = if tail.is_empty() {
                String::new()
            } else {
                format!(":\n{tail}")
            };
            return Err(AppError::Unsupported(format!(
                "engine exited early ({status}) before the mount appeared{suffix}"
            )));
        }

        if mount_present(source, mountpoint) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AppError::Unsupported(format!(
                "timed out waiting for {} to appear",
                mountpoint.display()
            )));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn mount_present(source: &dyn MountSource, mountpoint: &Path) -> bool {
    source
        .list()
        .map(|mounts| {
            mounts.iter().any(|mount| {
                mount.target.as_path() == mountpoint && fuse::is_fuse_fstype(&mount.fstype)
            })
        })
        .unwrap_or(false)
}

/// Unmount a managed profile or mountpoint.
pub fn umount(source: &dyn MountSource, target: &str, force: bool, all: bool) -> Result<()> {
    if all {
        return umount_all(source, force);
    }

    let registry = StateRegistry::load()?;
    if let Some(record) = find_record(&registry, target) {
        unmount_record(source, &record, force)?;
        let profile = record.profile.clone();
        state::update_registry(|registry| {
            registry.remove(&profile);
            Ok(())
        })?;
        return Ok(());
    }

    // Not managed; allow unmounting an external FUSE mount by path.
    if let Ok(mounts) = source.list() {
        if let Some(mount) = mounts.iter().find(|mount| {
            mount.target.as_path() == Path::new(target) && fuse::is_fuse_fstype(&mount.fstype)
        }) {
            unmount_path(source, &mount.target, force)?;
            println!("Unmounted external mount {}", mount.target.display());
            return Ok(());
        }
    }

    println!("{target} is not mounted by mount-fs.");
    Ok(())
}

/// Attempt to unmount every managed profile, reporting per-profile results.
///
/// The registry is always persisted (successfully unmounted profiles are
/// removed) even when some unmounts fail.
fn umount_all(source: &dyn MountSource, force: bool) -> Result<()> {
    let registry = StateRegistry::load()?;
    if registry.mounts.is_empty() {
        println!("No managed mounts.");
        return Ok(());
    }

    let records: Vec<MountRecord> = registry.mounts.values().cloned().collect();
    let mut succeeded: Vec<String> = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    for record in &records {
        match unmount_record(source, record, force) {
            Ok(()) => {
                println!("{}: unmounted", record.profile);
                succeeded.push(record.profile.clone());
            }
            Err(err) => {
                println!("{}: failed: {err}", record.profile);
                failures.push(record.profile.clone());
            }
        }
    }

    state::update_registry(|registry| {
        for profile in &succeeded {
            registry.remove(profile);
        }
        Ok(())
    })?;

    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::Unsupported(format!(
            "{} of {} unmounts failed: {}",
            failures.len(),
            records.len(),
            failures.join(", ")
        )))
    }
}

fn unmount_record(source: &dyn MountSource, record: &MountRecord, force: bool) -> Result<()> {
    let rows = list(source)?;
    let status = rows
        .iter()
        .find(|row| row.profile.as_deref() == Some(record.profile.as_str()))
        .map(|row| row.status);

    match status {
        Some(MountStatus::Healthy) | Some(MountStatus::Hung) | Some(MountStatus::External) => {
            unmount_path(source, &record.mountpoint, force)?;
            println!("Unmounted {}", record.mountpoint.display());
        }
        Some(MountStatus::Stale) | Some(MountStatus::NotMounted) | None => {
            println!(
                "{} is not mounted; cleaning up state.",
                record.mountpoint.display()
            );
        }
    }

    if let Some(pid) = record.pid {
        if engine_alive(pid, record.starttime) {
            process::terminate(pid);
        }
    }
    if record.engine == "aliyundrive-fuse" {
        rotate_aliyun_token(record);
    }
    if let Some(path) = &record.credential_path {
        if engine_credential_is_temp(&record.engine) {
            creds::remove_artifact(path);
        }
    }
    Ok(())
}

/// Persist a refresh token rewritten by `aliyundrive-fuse` back to
/// `credentials_ref` before the workdir artifact is removed.
fn rotate_aliyun_token(record: &MountRecord) {
    let Some(artifact) = &record.credential_path else {
        return;
    };
    if artifact.file_name() != Some(OsStr::new("refresh_token")) {
        return;
    }
    let Ok(rotated) = fs::read_to_string(artifact) else {
        return;
    };
    let rotated = rotated.trim();
    if rotated.is_empty() {
        return;
    }
    let Ok(config) = Config::load_default() else {
        return;
    };
    let Some(profile) = config.profile(&record.profile) else {
        return;
    };
    let Some(reference) = &profile.credentials_ref else {
        return;
    };
    let stored = fs::read_to_string(reference)
        .ok()
        .map(|token| token.trim().to_string());
    if stored.as_deref() == Some(rotated) {
        return;
    }
    if let Err(err) = creds::write_private_file(reference, format!("{rotated}\n").as_bytes()) {
        tracing::warn!(path = %reference.display(), %err, "failed to persist rotated Aliyun token");
    }
}

fn engine_credential_is_temp(engine_id: &str) -> bool {
    providers::registry()
        .into_iter()
        .flat_map(|provider| provider.engines())
        .find(|engine| engine.id() == engine_id)
        .map(|engine| engine.credential_retention() == CredentialRetention::Temp)
        .unwrap_or(true)
}

fn select_engine(
    provider: &dyn crate::providers::Provider,
    profile: &Profile,
    auto_fallback: bool,
) -> Result<Box<dyn crate::engines::Engine>> {
    let Some(requested) = profile.engine.as_deref() else {
        return provider.select_engine(profile);
    };

    let engines = provider.engines();
    if !engines.iter().any(|engine| engine.id() == requested) {
        // Produces the provider's "unsupported engine" error.
        return provider.select_engine(profile);
    }

    if engines
        .iter()
        .find(|engine| engine.id() == requested)
        .map(|engine| engine.detect().found)
        .unwrap_or(false)
    {
        return Ok(engines
            .into_iter()
            .find(|engine| engine.id() == requested)
            .expect("requested engine present"));
    }

    if !auto_fallback {
        return Err(AppError::NotFound(format!(
            "engine `{requested}` is not installed; run: mount-fs install {requested}"
        )));
    }

    for engine in engines {
        if engine.id() != requested && engine.detect().found {
            tracing::warn!(
                requested,
                fallback = engine.id(),
                "requested engine is not installed; falling back (engine_auto_fallback=true)"
            );
            return Ok(engine);
        }
    }

    Err(AppError::NotFound(format!(
        "engine `{requested}` is not installed and no fallback engine is available;          run: mount-fs install {requested}"
    )))
}

fn unmount_path(source: &dyn MountSource, mountpoint: &Path, force: bool) -> Result<()> {
    let program = fuse::fusermount3_path().unwrap_or_else(|| PathBuf::from("fusermount3"));
    let mut command = Command::new(program);
    if force {
        command.arg("-uz");
    } else {
        command.arg("-u");
    }
    command.arg(mountpoint);

    let output = command.output()?;
    if !output.status.success() {
        tracing::debug!(
            status = %output.status,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "fusermount3 returned non-zero; confirming mount state"
        );
    }

    let deadline = Instant::now() + UNMOUNT_TIMEOUT;
    loop {
        let present = source
            .list()
            .map(|mounts| {
                mounts
                    .iter()
                    .any(|mount| mount.target.as_path() == mountpoint)
            })
            .unwrap_or(false);
        if !present {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let hint = if force {
                "mountpoint is still present after a lazy unmount"
            } else {
                "mountpoint is still present; retry with --force"
            };
            return Err(AppError::Unsupported(format!(
                "failed to unmount {}: {hint}",
                mountpoint.display()
            )));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn find_record(registry: &StateRegistry, target: &str) -> Option<MountRecord> {
    registry.get(target).cloned().or_else(|| {
        registry
            .mounts
            .values()
            .find(|record| record.mountpoint.as_path() == Path::new(target))
            .cloned()
    })
}

/// List managed and external FUSE mounts.
pub fn list(source: &dyn MountSource) -> Result<Vec<Reconciled>> {
    list_with_config(source, None)
}

/// List mounts, including configured-but-never-mounted profiles as
/// [`MountStatus::NotMounted`].
pub fn list_with_config(
    source: &dyn MountSource,
    config_path: Option<&Path>,
) -> Result<Vec<Reconciled>> {
    let state = StateRegistry::load()?;
    let mounts = source.list()?;
    let mut rows = reconcile(&state, &mounts, &|pid, starttime| {
        engine_alive(pid, starttime)
    });

    let config = match config_path {
        Some(path) => Config::load(path).ok(),
        None => Config::load_default().ok(),
    };
    if let Some(config) = config {
        for (name, profile) in &config.profiles {
            if state.get(name).is_some() {
                continue;
            }
            if let Some(existing) = rows
                .iter_mut()
                .find(|row| row.mountpoint == profile.mountpoint)
            {
                // An unmanaged FUSE mount occupies this profile's mountpoint:
                // attribute it to the profile so `status <profile>` works.
                if existing.profile.is_none() {
                    existing.profile = Some(name.clone());
                }
                continue;
            }
            rows.push(Reconciled {
                profile: Some(name.clone()),
                mountpoint: profile.mountpoint.clone(),
                status: MountStatus::NotMounted,
                source: None,
                fstype: None,
                pid: None,
            });
        }
    }

    Ok(rows)
}

/// Status of one profile.
pub fn status(source: &dyn MountSource, profile: &str) -> Result<Reconciled> {
    status_with_config(source, profile, None)
}

/// Status of one profile with an explicit config path.
pub fn status_with_config(
    source: &dyn MountSource,
    profile: &str,
    config_path: Option<&Path>,
) -> Result<Reconciled> {
    let rows = list_with_config(source, config_path)?;
    rows.into_iter()
        .find(|row| row.profile.as_deref() == Some(profile))
        .ok_or_else(|| {
            AppError::NotFound(format!("profile `{profile}` is not configured or managed"))
        })
}

/// Serialize reconciled rows as JSON.
pub fn json_list(rows: &[Reconciled]) -> Result<String> {
    serde_json::to_string_pretty(rows)
        .map_err(|err| AppError::Config(format!("cannot serialize status: {err}")))
}

fn print_reconciled(row: &Reconciled) {
    println!(
        "{}: {} at {}",
        row.profile.as_deref().unwrap_or("-"),
        row.status,
        row.mountpoint.display()
    );
}

/// Resolve credentials appropriate to the engine's token kind.
fn resolve_secrets(engine: &dyn crate::engines::Engine, profile: &Profile) -> Result<MountSecrets> {
    match engine.token_kind() {
        TokenKind::AwsKey => {
            let (access_key, secret_key) = creds::resolve_s3_credentials(profile)?;
            Ok(MountSecrets {
                access_key: Some(access_key),
                secret_key: Some(secret_key),
                ..MountSecrets::default()
            })
        }
        TokenKind::LegacyWeb | TokenKind::OpenApi => {
            let creds = creds::resolve_aliyun_credentials(profile)?;
            Ok(MountSecrets {
                refresh_token: Some(creds.refresh_token),
                drive_id: creds.drive_id,
                ..MountSecrets::default()
            })
        }
    }
}

fn engine_alive(pid: u32, starttime: Option<u64>) -> bool {
    match starttime {
        Some(starttime) => process::is_alive(pid, starttime),
        None => process::pid_starttime(pid).is_some(),
    }
}

fn resolve_profile(
    config: &Config,
    target: &str,
    opts: &MountOptionsArg,
) -> Result<(String, Profile)> {
    if let Some(existing) = config.profile(target) {
        let mut profile = existing.clone();
        apply_overrides(&mut profile, opts);
        return Ok((target.to_string(), profile));
    }

    if target == ProviderKind::S3.to_string() {
        let mountpoint = opts
            .mountpoint
            .as_deref()
            .map(expand_tilde)
            .ok_or_else(|| AppError::Config("ad-hoc s3 mount requires --mountpoint".to_string()))?;
        let bucket = opts
            .bucket
            .clone()
            .ok_or_else(|| AppError::Config("ad-hoc s3 mount requires --bucket".to_string()))?;
        let endpoint = opts
            .endpoint
            .clone()
            .ok_or_else(|| AppError::Config("ad-hoc s3 mount requires --endpoint".to_string()))?;

        let profile = Profile {
            provider: ProviderKind::S3,
            engine: opts.engine.clone(),
            mountpoint,
            allow_other: opts.allow_other.unwrap_or(config.general.allow_other),
            read_only: opts.read_only.unwrap_or(false),
            credentials_ref: None,
            options: MountOptions::default(),
            bucket: Some(bucket),
            endpoint: Some(endpoint),
            region: None,
            path_style: Some(true),
            drive_id: None,
        };
        return Ok((target.to_string(), profile));
    }

    Err(AppError::NotFound(format!(
        "unknown profile or provider `{target}`"
    )))
}

fn apply_overrides(profile: &mut Profile, opts: &MountOptionsArg) {
    if let Some(mountpoint) = &opts.mountpoint {
        profile.mountpoint = expand_tilde(mountpoint);
    }
    if let Some(engine) = &opts.engine {
        profile.engine = Some(engine.clone());
    }
    if let Some(read_only) = opts.read_only {
        profile.read_only = read_only;
    }
    if let Some(allow_other) = opts.allow_other {
        profile.allow_other = allow_other;
    }
}
