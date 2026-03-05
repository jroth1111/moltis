//! Linux virtual display orchestration for headful browser launches.

use std::process::Child;

#[cfg(target_os = "linux")]
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use tracing::debug;
use tracing::warn;

use crate::{error::Error, types::VirtualDisplayConfig};

/// Running virtual X display process.
pub struct VirtualDisplay {
    display: String,
    child: Child,
}

impl VirtualDisplay {
    /// Start a virtual display if enabled.
    #[allow(clippy::needless_return)]
    pub fn start(config: &VirtualDisplayConfig) -> Result<Option<Self>, Error> {
        if !config.enabled {
            return Ok(None);
        }

        #[cfg(not(target_os = "linux"))]
        {
            warn!("virtual display is only supported on Linux hosts");
            return Ok(None);
        }

        #[cfg(target_os = "linux")]
        {
            if config.display_min > config.display_max {
                return Err(Error::LaunchFailed(
                    "virtual display range is invalid: display_min > display_max".to_string(),
                ));
            }

            for display_num in config.display_min..=config.display_max {
                if display_socket_path(display_num).exists() {
                    continue;
                }

                let display = format!(":{display_num}");
                let mut child = Command::new(&config.binary)
                    .arg(&display)
                    .arg("-screen")
                    .arg("0")
                    .arg(format!(
                        "{}x{}x{}",
                        config.width, config.height, config.color_depth
                    ))
                    .arg("-ac")
                    .arg("-nolisten")
                    .arg("tcp")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| {
                        Error::LaunchFailed(format!(
                            "failed to start virtual display '{}' ({}): {e}",
                            config.binary, display
                        ))
                    })?;

                let start = Instant::now();
                let timeout = Duration::from_millis(config.startup_timeout_ms.max(100));
                while start.elapsed() < timeout {
                    if display_socket_path(display_num).exists() {
                        debug!(display = %display, "virtual display ready");
                        return Ok(Some(Self { display, child }));
                    }

                    match child.try_wait() {
                        Ok(Some(status)) => {
                            warn!(
                                display = %display,
                                status = %status,
                                "virtual display process exited before readiness"
                            );
                            break;
                        },
                        Ok(None) => {},
                        Err(e) => {
                            warn!(display = %display, error = %e, "virtual display wait failed");
                            break;
                        },
                    }
                    sleep(Duration::from_millis(50));
                }

                let _ = child.kill();
                let _ = child.wait();
            }

            Err(Error::LaunchFailed(format!(
                "failed to allocate virtual display in range :{}-:{}",
                config.display_min, config.display_max
            )))
        }
    }

    /// `DISPLAY` value (for Chromium process env).
    #[must_use]
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(target_os = "linux")]
fn display_socket_path(display_num: u16) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{display_num}"))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn display_socket_path_uses_x11_socket_layout() {
        assert_eq!(display_socket_path(99), PathBuf::from("/tmp/.X11-unix/X99"));
    }
}
