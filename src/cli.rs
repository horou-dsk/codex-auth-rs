use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "codex-auth-rs", about = "Codex account switcher rewritten in Rust")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    List,
    Login(LoginArgs),
    Import(ImportArgs),
    Switch(SwitchArgs),
    Remove(RemoveArgs),
    Status,
    Clean,
    Daemon(DaemonArgs),
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    #[arg(long)]
    pub device_auth: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub alias: Option<String>,
    #[arg(long)]
    pub purge: bool,
    #[arg(long)]
    pub cpa: bool,
}

#[derive(Debug, Args)]
pub struct SwitchArgs {
    pub query: Option<String>,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub query: Option<String>,
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[arg(long, conflicts_with = "once")]
    pub watch: bool,
    #[arg(long)]
    pub once: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub section: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Auto(ConfigAutoArgs),
    Api(ConfigApiArgs),
}

#[derive(Debug, Args)]
pub struct ConfigAutoArgs {
    pub action: Option<ConfigToggle>,
    #[arg(long = "5h")]
    pub threshold_5h_percent: Option<u8>,
    #[arg(long)]
    pub weekly: Option<u8>,
}

#[derive(Debug, Args)]
pub struct ConfigApiArgs {
    pub action: ConfigToggle,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum ConfigToggle {
    Enable,
    Disable,
}

pub fn parse() -> Command {
    Cli::parse().command
}
