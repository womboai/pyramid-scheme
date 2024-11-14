use std::process::Command;
use std::process;
use std::thread::{self, sleep};
use std::time::Duration;
use tracing::{info, error};
use anyhow::Result;

pub struct Updater {
    check_interval: Duration,
    git_branch: String,
}

impl Updater {
    pub fn new(check_interval: Duration, git_branch: String) -> Self {
        Self { 
            check_interval,
            git_branch,
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

    fn check_and_update(&self) -> Result<()> {
        // Single atomic pull operation
        let pull = Command::new("git")
            .args(["pull", "--ff-only", "origin", &self.git_branch])
            .output()?;

        if !pull.status.success() {
            return Err(anyhow::anyhow!("Pull failed: {}", 
                String::from_utf8_lossy(&pull.stderr)));
        }

        let stdout = String::from_utf8_lossy(&pull.stdout);
        if stdout.contains("Already up to date") {
            info!("No updates available");
            return Ok(());
        }

        info!("Update found: {}", stdout.trim());

        // Check if build succeeds before restarting
        let build = Command::new("cargo")
            .args(["build", "--release"])
            .output()?;

        if !build.status.success() {
            return Err(anyhow::anyhow!("Build failed: {}", 
                String::from_utf8_lossy(&build.stderr)));
        }

        info!("Update downloaded and built successfully. Restarting process to apply changes...");
        process::exit(0);
    }
}
