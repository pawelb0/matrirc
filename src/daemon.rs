use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

pub fn pid_file_path() -> Result<PathBuf> {
    let base = state_dir()?;
    Ok(base.join("daemon.pid"))
}

fn state_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("matrirc"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".local").join("state").join("matrirc"))
}

pub fn read_pid(path: &Path) -> Result<Option<u32>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s.trim().parse::<u32>().ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    // Signal 0 is a "does process exist / can I signal it" probe.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
pub fn send_sigterm(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc != 0 {
        return Err(anyhow!("kill(pid={pid}, SIGTERM) failed: errno {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn send_sigterm(_pid: u32) -> Result<()> {
    Err(anyhow!("send_sigterm only supported on unix"))
}

/// Claims the pid file for the running process. Returns a guard that deletes the
/// file on drop. Fails if another live process already owns the file.
pub fn claim() -> Result<PidGuard> {
    let path = pid_file_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    if let Some(prev) = read_pid(&path)? {
        if alive(prev) {
            return Err(anyhow!("another matrirc is running (pid {prev}); stop it first with `matrirc stop`"));
        }
    }
    let me = std::process::id();
    std::fs::write(&path, format!("{me}\n"))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(PidGuard { path })
}

pub struct PidGuard {
    path: PathBuf,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
