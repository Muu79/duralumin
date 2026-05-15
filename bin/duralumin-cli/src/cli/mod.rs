pub mod check;
pub mod download;
pub mod episode;
pub mod feed;
pub mod helpers;
pub mod purge;
pub mod quarantine;
pub mod rules;
pub mod start;
pub mod status;
pub mod sync;

// Top-level re-exports so `run()` in main.rs can call `cli::cmd_*` directly.
pub use check::cmd_check;
pub use download::cmd_download;
pub use purge::cmd_purge;
pub use quarantine::cmd_quarantine_list;
pub use start::cmd_start;
pub use status::cmd_status;
pub use sync::cmd_sync;

// ---- Single-instance daemon lock -------------------------------------------

use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct RunLockGuard {
    path: PathBuf,
}

impl Drop for RunLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write a PID file next to the DB. Returns a guard that removes it on drop.
/// Errors if another `dura start` process appears to be running.
pub fn acquire_run_lock(db_path: &std::path::Path) -> Result<RunLockGuard> {
    let pid_path = db_path.with_file_name("dura.pid");

    if let Ok(content) = std::fs::read_to_string(&pid_path)
        && let Ok(pid) = content.trim().parse::<u32>()
        && pid != std::process::id()
        && is_running(pid)
    {
        anyhow::bail!(
            "`dura start` is already running (PID {pid}).\n  \
                     Stop it first, or remove {:?} if the file is stale.",
            pid_path
        );
    }

    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())
        .with_context(|| format!("failed to write PID file {:?}", pid_path))?;

    Ok(RunLockGuard { path: pid_path })
}

fn is_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        let _ = pid;
        false // conservative: stale PID files won't block restart on Windows
    }
}
