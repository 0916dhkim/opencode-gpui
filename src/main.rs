mod gpui_ui;

use clap::Parser;

#[derive(Clone, Debug, Parser)]
#[command(version, about)]
pub struct Args {
    /// OpenCode v2 server URL.
    #[arg(long, env = "OPENCODE_SERVER_URL")]
    pub server: Option<String>,
    #[arg(long, env = "OPENCODE_SERVER_USERNAME")]
    pub username: Option<String>,
    #[arg(long, env = "OPENCODE_SERVER_PASSWORD")]
    pub password: Option<String>,
    #[arg(long, env = "OPENCODE_CF_ACCESS_CLIENT_ID")]
    pub cf_access_client_id: Option<String>,
    #[arg(long, env = "OPENCODE_CF_ACCESS_CLIENT_SECRET")]
    pub cf_access_client_secret: Option<String>,
    /// Show deterministic offline data for screenshot comparison.
    #[arg(long)]
    pub preview: bool,
    /// Exercise the real command/event UI with a deterministic in-process v2 fixture.
    #[arg(long)]
    pub preview_api: bool,
    /// Initial overlay to show in preview mode.
    #[arg(long)]
    pub drawer: Option<String>,
}

fn main() {
    env_logger::init();
    let args = Args::parse();
    gpui_ui::run(args);
}
