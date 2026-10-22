use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "device-simulator-mcp",
    version,
    about = "Device Simulator MCP server"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: CliCommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CliCommand {
    /// Register this MCP server in an AI client's configuration.
    InstallMcp(InstallMcpArgs),
    /// Prepare an Xcode project for Personal Team signing on an iPhone.
    PrepareIosRunner(PrepareIosRunnerArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum Client {
    ClaudeCode,
    Codex,
    Cursor,
    GeminiCli,
    #[value(name = "opencode")]
    OpenCode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum Scope {
    User,
    Project,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum DevicePlatform {
    Ios,
    Android,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum IosRunnerTransport {
    Xcode,
    Iproxy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum IosRunnerSigningMode {
    Automatic,
    Local,
}

impl DevicePlatform {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
        }
    }
}

impl IosRunnerTransport {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Xcode => "xcode",
            Self::Iproxy => "iproxy",
        }
    }
}

impl IosRunnerSigningMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Local => "local",
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct InstallMcpArgs {
    /// AI client whose MCP configuration will be updated.
    #[arg(long, value_enum)]
    pub(crate) client: Client,

    /// Write user-wide or current-project configuration (default: user).
    #[arg(long, value_enum, default_value = "user")]
    pub(crate) scope: Scope,

    /// Platform selected by DEVICE_PLATFORM (default: ios).
    #[arg(long, value_enum, default_value = "ios")]
    pub(crate) platform: DevicePlatform,

    /// Physical iOS runner transport to persist in the MCP environment.
    #[arg(long, value_enum)]
    pub(crate) ios_runner_transport: Option<IosRunnerTransport>,

    /// Physical iOS runner signing mode to persist in the MCP environment.
    #[arg(long, value_enum)]
    pub(crate) ios_runner_signing_mode: Option<IosRunnerSigningMode>,

    /// Explicit configuration path instead of the client's default.
    #[arg(long)]
    pub(crate) config_file: Option<PathBuf>,

    /// MCP executable path (default: this running executable).
    #[arg(long)]
    pub(crate) binary: Option<PathBuf>,

    /// Apply the configuration. Without this flag, print a preview only.
    #[arg(long)]
    pub(crate) apply: bool,
}

#[derive(Debug, Args)]
pub(crate) struct PrepareIosRunnerArgs {
    /// Apple Developer Team ID shown for the Personal Team in Xcode.
    #[arg(long)]
    pub(crate) team_id: String,

    /// Persistent output directory for the Xcode signing project.
    #[arg(long)]
    pub(crate) output_dir: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::{
        Cli, CliCommand, Client, DevicePlatform, IosRunnerSigningMode, IosRunnerTransport, Scope,
    };

    #[test]
    fn defaults_to_user_scope_and_ios_platform() {
        let parsed = Cli::try_parse_from([
            "device-simulator-mcp",
            "install-mcp",
            "--client",
            "opencode",
        ])
        .unwrap();

        let CliCommand::InstallMcp(arguments) = parsed.command else {
            panic!("expected install-mcp command");
        };
        assert_eq!(arguments.scope, Scope::User);
        assert_eq!(arguments.platform, DevicePlatform::Ios);
        assert!(!arguments.apply);
    }

    #[test]
    fn parses_project_scope_and_android_platform() {
        let parsed = Cli::try_parse_from([
            "device-simulator-mcp",
            "install-mcp",
            "--client",
            "codex",
            "--scope",
            "project",
            "--platform",
            "android",
            "--apply",
        ])
        .unwrap();

        let CliCommand::InstallMcp(arguments) = parsed.command else {
            panic!("expected install-mcp command");
        };
        assert_eq!(arguments.client, Client::Codex);
        assert_eq!(arguments.scope, Scope::Project);
        assert_eq!(arguments.platform, DevicePlatform::Android);
        assert_eq!(arguments.ios_runner_transport, None);
        assert_eq!(arguments.ios_runner_signing_mode, None);
        assert!(arguments.apply);
    }

    #[test]
    fn parses_physical_ios_runner_settings() {
        let parsed = Cli::try_parse_from([
            "device-simulator-mcp",
            "install-mcp",
            "--client",
            "opencode",
            "--ios-runner-transport",
            "iproxy",
            "--ios-runner-signing-mode",
            "local",
        ])
        .unwrap();

        let CliCommand::InstallMcp(arguments) = parsed.command else {
            panic!("expected install-mcp command");
        };
        assert_eq!(
            arguments.ios_runner_transport,
            Some(IosRunnerTransport::Iproxy)
        );
        assert_eq!(
            arguments.ios_runner_signing_mode,
            Some(IosRunnerSigningMode::Local)
        );
    }

    #[test]
    fn parses_personal_team_runner_preparation_arguments() {
        let parsed = Cli::try_parse_from([
            "device-simulator-mcp",
            "prepare-ios-runner",
            "--team-id",
            "ABCDEFGHIJ",
            "--output-dir",
            "./prepared-runner",
        ])
        .unwrap();

        let CliCommand::PrepareIosRunner(arguments) = parsed.command else {
            panic!("expected prepare-ios-runner command");
        };
        assert_eq!(arguments.team_id, "ABCDEFGHIJ");
        assert_eq!(
            arguments.output_dir,
            Some(PathBuf::from("./prepared-runner"))
        );
    }
}
