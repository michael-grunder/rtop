#![warn(clippy::all, clippy::nursery, clippy::pedantic)]

use anyhow::Result;
use rtop::{cli, tui};

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(launch) = cli::launch_or_config()? {
        tui::run(launch).await?;
    }
    Ok(())
}
