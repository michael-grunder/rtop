#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use anyhow::Result;
use rtop::{cli, tui};

#[tokio::main]
async fn main() -> Result<()> {
    let launch = cli::build_launch_config()?;
    tui::run(launch).await
}
