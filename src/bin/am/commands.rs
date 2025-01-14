use anyhow::Result;
use autometrics_am::config::AmConfig;
use clap::{Parser, Subcommand};
use indicatif::MultiProgress;
use std::path::PathBuf;
use tracing::info;

mod explore;
mod init;
mod list;
mod proxy;
pub mod start;
pub mod system;
pub mod update;

#[derive(Parser)]
#[command(author, version, about, long_about = None, bin_name = "am")]
pub struct Application {
    #[command(subcommand)]
    pub command: SubCommands,

    /// Enable verbose logging. By enabling this you are also able to use
    /// RUST_LOG environment variable to change the log levels of other
    /// modules.
    ///
    /// By default, we will only log INFO level messages of all modules. If this
    /// flag is enabled, then we will log the message from `am` with DEBUG
    /// level, other modules still use the INFO level.
    #[clap(long, short)]
    pub verbose: bool,

    /// Use the following file to define defaults for am.
    #[clap(long, env)]
    pub config_file: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum SubCommands {
    /// Start scraping the specified endpoint(s), while also providing a web
    /// interface to inspect the autometrics data.
    Start(start::CliArguments),

    /// Manage am related system settings. Such as cleaning up downloaded
    /// Prometheus, Pushgateway installs.
    System(system::Arguments),

    /// Open up the existing Explorer
    #[clap(alias = "explorer")]
    Explore(explore::Arguments),

    /// Use am as a proxy to another prometheus instance
    Proxy(proxy::CliArguments),

    /// Create a new `am.toml` file interactively with sensible defaults
    Init(init::Arguments),

    /// Open the Fiberplane discord to receive help, send suggestions or
    /// discuss various things related to Autometrics and the `am` CLI
    Discord,

    /// Run the updater
    Update(update::Arguments),

    /// List the functions in a project
    List(list::Arguments),

    #[clap(hide = true)]
    MarkdownHelp,
}

pub async fn handle_command(app: Application, config: AmConfig, mp: MultiProgress) -> Result<()> {
    match app.command {
        SubCommands::Start(args) => start::handle_command(args, config, mp).await,
        SubCommands::System(args) => system::handle_command(args, mp).await,
        SubCommands::Explore(args) => explore::handle_command(args).await,
        SubCommands::Proxy(args) => proxy::handle_command(args).await,
        SubCommands::Init(args) => init::handle_command(args).await,
        SubCommands::Discord => {
            const URL: &str = "https://discord.gg/kHtwcH8As9";

            if open::that(URL).is_err() {
                info!("Unable to open browser, open the following URL in your browser: {URL}");
            }

            Ok(())
        }
        SubCommands::Update(args) => update::handle_command(args, mp).await,
        SubCommands::List(args) => list::handle_command(args),
        SubCommands::MarkdownHelp => {
            let disable_toc = true;
            clap_markdown::print_help_markdown::<Application>(Some(disable_toc));
            Ok(())
        }
    }
}
