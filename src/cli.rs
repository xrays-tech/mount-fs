//! Command-line interface definition.
//!
//! The full command surface is declared here; M2a wires `providers`, `install`,
//! `uninstall`, and `config`, while mount lifecycle commands remain stubs.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Mount cloud drives by orchestrating existing FUSE tools.
#[derive(Debug, Parser)]
#[command(
    name = "mount-fs",
    version,
    about = "Mount cloud drives (Aliyun Drive, S3-compatible object storage)",
    long_about = None
)]
pub struct Cli {
    /// Increase logging verbosity (repeatable: -v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Suppress all non-error output.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable colored output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Path to the configuration file.
    #[arg(long, value_name = "PATH", global = true)]
    pub config: Option<PathBuf>,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// List known providers and their support status.
    Providers,

    /// Check the system for required dependencies.
    Doctor {
        /// Apply safe, minimal fixes (currently only `/etc/fuse.conf`).
        #[arg(long)]
        fix: bool,
    },

    /// Install a mount engine.
    Install {
        /// Engine identifier (e.g. s3fs-fuse).
        engine: String,
        /// Installation method.
        #[arg(long, value_enum, default_value_t = InstallMethodArg::Auto)]
        method: InstallMethodArg,
        /// Print commands without executing them.
        #[arg(long)]
        dry_run: bool,
        /// Allow executing the source-build recipe.
        #[arg(long)]
        yes: bool,
        /// Do not install runtime dependencies after the engine.
        #[arg(long)]
        no_deps: bool,
    },

    /// Uninstall a mount engine.
    Uninstall {
        /// Engine identifier (e.g. s3fs-fuse).
        engine: String,
    },

    /// Manage configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Mount a configured profile (or ad-hoc provider target).
    Mount {
        /// Profile name, or a provider id such as `s3`.
        target: String,
        /// Override the mountpoint.
        #[arg(long)]
        mountpoint: Option<PathBuf>,
        /// Bucket name (ad-hoc provider mounts).
        #[arg(long)]
        bucket: Option<String>,
        /// Endpoint URL (ad-hoc provider mounts).
        #[arg(long)]
        endpoint: Option<String>,
        /// Override the engine.
        #[arg(long)]
        engine: Option<String>,
        /// Mount read-only.
        #[arg(long, conflicts_with = "rw")]
        ro: bool,
        /// Mount read-write.
        #[arg(long)]
        rw: bool,
        /// Permit access by other users (`allow_other`).
        #[arg(long)]
        allow_other: bool,
        /// Run in the foreground.
        #[arg(long)]
        foreground: bool,
        /// Print the command without executing it.
        #[arg(long)]
        dry_run: bool,
        /// Install missing engines/dependencies before mounting.
        #[arg(long)]
        auto_install: bool,
    },

    /// Unmount a mounted profile or mountpoint.
    Umount {
        /// Profile name or mountpoint.
        target: String,
        /// Lazy/forced unmount.
        #[arg(long)]
        force: bool,
        /// Unmount all managed mounts.
        #[arg(long)]
        all: bool,
    },

    /// List configured profiles and their mount state.
    List {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show the status of a profile.
    Status {
        /// Profile name.
        profile: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Manage systemd user services for persistent mounts.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Show logs for a profile.
    Logs {
        /// Profile name.
        profile: String,
        /// Follow the log.
        #[arg(long)]
        follow: bool,
    },

    /// Show or install runtime dependencies.
    Deps {
        #[command(subcommand)]
        action: Option<DepsAction>,
        /// Engine id.
        #[arg(long)]
        engine: Option<String>,
        /// Profile name.
        #[arg(long)]
        profile: Option<String>,
        /// Emit JSON (status only).
        #[arg(long)]
        json: bool,
    },

    /// Print the version.
    Version,
}

/// Actions for `deps`.
#[derive(Debug, Subcommand)]
pub enum DepsAction {
    /// Install missing dependencies via the package manager.
    Install {
        /// Engine id.
        #[arg(long)]
        engine: Option<String>,
        /// Profile name.
        #[arg(long)]
        profile: Option<String>,
        /// Print the command without executing it.
        #[arg(long)]
        dry_run: bool,
        /// Confirm installing system packages.
        #[arg(long)]
        yes: bool,
    },
}

/// Actions for `service`.
#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Generate a systemd user unit for a profile.
    Generate {
        /// Profile name.
        profile: String,
        /// Path to the mount-fs binary (defaults to the current executable).
        #[arg(long)]
        exec: Option<PathBuf>,
        /// Overwrite an existing unit.
        #[arg(long)]
        force: bool,
        /// Render the unit without writing it.
        #[arg(long)]
        dry_run: bool,
    },

    /// List generated units.
    List {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },

    /// Remove a generated unit.
    Remove {
        /// Profile name.
        profile: String,
    },
}

/// Installation method selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstallMethodArg {
    /// Use the engine's declared method.
    Auto,
    /// Require a package-manager installation.
    Pkg,
    /// Require a pinned release archive download.
    Release,
    /// Print or run the source-build recipe.
    Source,
}

/// Actions for the `config` subcommand.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Create a new configuration file.
    Init,

    /// List configuration keys.
    List,

    /// Read a configuration value.
    Get {
        /// Configuration key.
        key: String,
    },

    /// Write a configuration value.
    Set {
        /// Configuration key.
        key: String,
        /// Configuration value.
        value: String,
    },

    /// Manage profiles.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },

    /// Show the effective configuration with secrets redacted.
    Show {
        /// Profile name (omit for all profiles).
        name: Option<String>,
    },

    /// Validate profiles without mounting them.
    Validate {
        /// Profile name (omit for all profiles).
        name: Option<String>,
    },

    /// Store credentials for a provider (reads secret values from stdin).
    Login {
        /// Provider (`s3` or `aliyun`).
        provider: Option<String>,
        /// Profile name.
        #[arg(long)]
        profile: Option<String>,
        /// Aliyun OpenAPI drive id (for qrypt/webdav).
        #[arg(long)]
        drive_id: Option<String>,
        /// Use the interactive QR-code flow (Aliyun only).
        #[arg(long)]
        qr: bool,
    },

    /// Remove stored credentials for a provider.
    Logout {
        /// Provider (`s3` or `aliyun`).
        provider: Option<String>,
        /// Profile name.
        #[arg(long)]
        profile: Option<String>,
    },
}

/// Provider selector for `config profile add`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProviderArg {
    /// S3-compatible object storage.
    S3,
    /// Aliyun Drive.
    Aliyun,
}

/// Actions for `config profile`.
#[derive(Debug, Subcommand)]
pub enum ProfileAction {
    /// Add a new profile.
    Add {
        /// Profile name.
        name: String,
        /// Cloud provider.
        #[arg(long, value_enum)]
        provider: ProviderArg,
        /// Preferred engine.
        #[arg(long)]
        engine: Option<String>,
        /// Local mountpoint.
        #[arg(long)]
        mountpoint: PathBuf,
        /// S3 bucket.
        #[arg(long)]
        bucket: Option<String>,
        /// S3 endpoint.
        #[arg(long)]
        endpoint: Option<String>,
        /// S3 region.
        #[arg(long)]
        region: Option<String>,
        /// Use path-style S3 requests.
        #[arg(long)]
        path_style: bool,
        /// Aliyun OpenAPI drive id.
        #[arg(long)]
        drive_id: Option<String>,
        /// Aliyun drive type (aliyundrive-webdav): resource | backup | default.
        #[arg(long)]
        drive_type: Option<String>,
        /// Permit access by other users.
        #[arg(long)]
        allow_other: bool,
        /// Mount read-only.
        #[arg(long)]
        read_only: bool,
        /// Overwrite an existing profile.
        #[arg(long)]
        force: bool,
    },

    /// Remove a profile.
    Remove {
        /// Profile name.
        name: String,
    },

    /// Set non-secret profile fields (`KEY=VALUE`).
    Set {
        /// Profile name.
        name: String,
        /// Assignments, e.g. `region=us-east-2`.
        #[arg(value_name = "KEY=VALUE")]
        assignments: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_global_flags_and_subcommand() {
        let cli = Cli::parse_from(["mount-fs", "-vv", "--no-color", "doctor"]);
        assert_eq!(cli.verbose, 2);
        assert!(cli.no_color);
        assert!(matches!(cli.command, Some(Commands::Doctor { fix: false })));
    }

    #[test]
    fn parses_install_flags() {
        let cli = Cli::parse_from([
            "mount-fs",
            "install",
            "s3fs-fuse",
            "--method",
            "source",
            "--dry-run",
            "--yes",
        ]);
        match cli.command {
            Some(Commands::Install {
                engine,
                method,
                dry_run,
                yes,
                no_deps,
            }) => {
                assert_eq!(engine, "s3fs-fuse");
                assert_eq!(method, InstallMethodArg::Source);
                assert!(dry_run);
                assert!(yes);
                assert!(!no_deps);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_deps_install() {
        let cli = Cli::parse_from([
            "mount-fs",
            "deps",
            "install",
            "--engine",
            "aliyundrive-webdav",
            "--dry-run",
        ]);
        match cli.command {
            Some(Commands::Deps {
                action:
                    Some(DepsAction::Install {
                        engine, dry_run, ..
                    }),
                ..
            }) => {
                assert_eq!(engine.as_deref(), Some("aliyundrive-webdav"));
                assert!(dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_mount_auto_install_and_install_no_deps() {
        let cli = Cli::parse_from(["mount-fs", "mount", "p", "--auto-install"]);
        match cli.command {
            Some(Commands::Mount { auto_install, .. }) => assert!(auto_install),
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from(["mount-fs", "install", "s3fs-fuse", "--no-deps"]);
        match cli.command {
            Some(Commands::Install { no_deps, .. }) => assert!(no_deps),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
