//! CLI subcommands for emergency stop (E-STOP).

use std::path::Path;

use clap::Subcommand;

#[derive(Subcommand)]
pub enum EstopAction {
    /// Enable emergency stop.
    Enable,
    /// Disable emergency stop.
    Disable,
    /// Show emergency stop status.
    Status,
}

pub fn handle_estop(action: EstopAction, data_dir: &Path) -> anyhow::Result<()> {
    let estop_path = data_dir.join("estop");

    match action {
        EstopAction::Enable => {
            std::fs::write(&estop_path, b"enabled")?;
            println!("E-STOP enabled");
        },
        EstopAction::Disable => {
            if estop_path.exists() {
                std::fs::remove_file(&estop_path)?;
            }
            println!("E-STOP disabled");
        },
        EstopAction::Status => {
            let enabled = estop_path.exists();
            println!(
                "E-STOP {}",
                if enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        },
    }

    Ok(())
}
