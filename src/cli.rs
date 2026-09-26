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

impl DevicePlatform {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ios => "ios",
            Self::Android => "android",
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, CliCommand, Client, DevicePlatform, Scope};

    #[test]
    fn defaults_to_user_scope_and_ios_platform() {
        let parsed = Cli::try_parse_from([
            "device-simulator-mcp",
            "install-mcp",
            "--client",
            "opencode",
        ])
        .unwrap();

        let CliCommand::InstallMcp(arguments) = parsed.command;
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

        let CliCommand::InstallMcp(arguments) = parsed.command;
        assert_eq!(arguments.client, Client::Codex);
        assert_eq!(arguments.scope, Scope::Project);
        assert_eq!(arguments.platform, DevicePlatform::Android);
        assert!(arguments.apply);
    }
}
