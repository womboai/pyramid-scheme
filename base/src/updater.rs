use std::process::Command;
use std::thread::{self, sleep};
use std::time::Duration;
use tracing::{info, error};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum UpdaterError {
    #[error("Git pull failed: {0}")]
    PullFailed(String),
    
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    
    #[error(transparent)]
    Utf8Error(#[from] std::string::FromUtf8Error)
}

pub struct Updater {
    check_interval: Duration,
}

impl Updater {
    pub fn new(check_interval: Duration) -> Self {
        Self { 
            check_interval,
        }
    }

    pub fn spawn(self) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            loop {
                if let Err(e) = self.check_and_update() {
                    error!("Update check failed: {}", e);
                }
                sleep(self.check_interval);
            }
        })
    }

    fn check_and_update(&self) -> Result<(), UpdaterError> {
        let pull = Command::new("git")
            .args(["pull", "--ff-only"])
            .output()?;

        if !pull.status.success() {
            return Err(UpdaterError::PullFailed(
                String::from_utf8_lossy(&pull.stderr).to_string()
            ));
        }

        let stdout = String::from_utf8_lossy(&pull.stdout);
        if stdout.contains("Already up to date") {
            info!("No updates available");
            return Ok(());
        }

        info!("Update found: {}", stdout.trim());
        info!("Update downloaded successfully. Please rebuild and restart the process to apply changes.");
        Ok(())
    }
}
