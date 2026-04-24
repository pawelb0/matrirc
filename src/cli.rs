use std::io::Read;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::config_path;
use crate::matrix;

const PERL_TEMPLATE: &str = include_str!("../irssi/matrirc.pl.in");

#[derive(Parser, Debug)]
#[command(version, about = "Local IRC server bridging to a Matrix homeserver")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start the IRC server (default).
    Run,
    /// Install the irssi autorun script into ~/.irssi/scripts/autorun/.
    InstallIrssi {
        /// Overwrite existing matrirc.pl if present.
        #[arg(long)]
        force: bool,
        /// Print the rendered script to stdout instead of writing it.
        #[arg(long)]
        dry_run: bool,
        /// Embed this absolute path in the script instead of PATH lookup.
        /// Useful for dev builds (`target/debug/matrirc`) that aren't on PATH.
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// Log in: password + SAS emoji verify. `--token` for access-token mode.
    Login {
        /// Your Matrix user ID, e.g. @you:matrix.org
        mxid: String,
        /// Override homeserver URL (skip .well-known resolution).
        #[arg(long)]
        homeserver: Option<String>,
        /// Read access token from MATRIRC_TOKEN / stdin instead of password prompt.
        #[arg(long)]
        token: bool,
        /// Skip SAS device verification after login.
        #[arg(long)]
        skip_verify: bool,
    },
    /// Import cross-signing + backup key via SSS recovery key (from
    /// MATRIRC_RECOVERY_KEY or stdin). Not persisted.
    BootstrapE2ee,
    /// Wipe local state (config, crypto store, names). Does not sign out the
    /// device on the homeserver — do that in Element → Sessions.
    Reset {
        #[arg(long)]
        force: bool,
    },
    /// Re-run SAS emoji verification against another device.
    Verify,
    /// Report whether the daemon is running.
    Status,
    /// SIGTERM the running daemon.
    Stop,
}

pub async fn login(
    mxid: &str,
    homeserver_override: Option<&str>,
    use_token: bool,
    skip_verify: bool,
) -> Result<()> {
    let server_name = matrix::server_name_from_mxid(mxid)?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("matrirc/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let homeserver_url = match homeserver_override {
        Some(h) => h.trim_end_matches('/').to_string(),
        None => matrix::discover_homeserver(&http, server_name).await?,
    };

    let (cfg, client) = if use_token {
        let token = read_secret("MATRIRC_TOKEN", "token")?;
        matrix::login_with_token(&homeserver_url, mxid, &token).await?
    } else {
        let password = read_password_for_mxid(mxid)?;
        matrix::login_with_password(&homeserver_url, mxid, &password).await?
    };

    let path = config_path()?;
    cfg.save(&path)?;
    println!("✓ {} (device {}) on {}", cfg.mxid, cfg.device_id, cfg.homeserver_url);
    println!("  config: {}", path.display());

    if skip_verify {
        println!("\n--skip-verify: E2EE rooms won't decrypt until `matrirc verify` or bootstrap-e2ee.");
        return Ok(());
    }

    println!();
    match matrix::run_sas_bootstrap(&client).await {
        Ok(true) => println!("✓ device verified. run `matrirc run`."),
        Ok(false) => println!("device already trusted. run `matrirc run`."),
        Err(e) => {
            println!("× SAS didn't finish: {e}");
            println!("  retry with `matrirc verify` or `matrirc bootstrap-e2ee`.");
        }
    }
    Ok(())
}

fn read_password_for_mxid(mxid: &str) -> Result<String> {
    if let Ok(p) = std::env::var("MATRIRC_PASSWORD") {
        let p = p.trim().to_string();
        if !p.is_empty() {
            return Ok(p);
        }
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("read stdin")?;
        let p = buf.trim().to_string();
        if p.is_empty() {
            return Err(anyhow!("empty password on stdin"));
        }
        return Ok(p);
    }
    let prompt = format!("password for {mxid}: ");
    rpassword::prompt_password(prompt).context("prompt password")
}

pub fn read_recovery_key() -> Result<String> {
    read_secret("MATRIRC_RECOVERY_KEY", "recovery key")
}

fn read_secret(env: &str, label: &str) -> Result<String> {
    if let Ok(t) = std::env::var(env) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Err(anyhow!(
            "no {label}: set {env} or pipe the {label} on stdin"
        ));
    }
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).context("read stdin")?;
    let t = buf.trim().to_string();
    if t.is_empty() {
        return Err(anyhow!("empty {label} on stdin"));
    }
    Ok(t)
}

pub async fn verify() -> Result<()> {
    let cfg_path = config_path()?;
    let cfg = crate::config::Config::load(&cfg_path)
        .with_context(|| format!("load config {}", cfg_path.display()))?;
    let client = matrix::build_client_restored(&cfg).await?;

    let sas_outcome = matrix::run_sas_bootstrap(&client).await;
    match &sas_outcome {
        Ok(true) => println!("✓ device verified."),
        Ok(false) => println!("device already trusted."),
        Err(e) => println!("× SAS verification failed: {e}"),
    }

    println!();
    println!("checking what we have ...");
    matrix::report_encryption_state(&client).await;

    if sas_outcome.is_err() {
        return sas_outcome.map(|_| ());
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let pid_path = crate::daemon::pid_file_path()?;
    match crate::daemon::read_pid(&pid_path)? {
        Some(pid) if crate::daemon::alive(pid) => println!("running (pid {pid})"),
        Some(pid) => println!("stale pid file (pid {pid} not running) — {}", pid_path.display()),
        None => println!("not running"),
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let pid_path = crate::daemon::pid_file_path()?;
    let Some(pid) = crate::daemon::read_pid(&pid_path)? else {
        println!("no pid file");
        return Ok(());
    };
    if !crate::daemon::alive(pid) {
        let _ = std::fs::remove_file(&pid_path);
        println!("pid {pid} not running; stale pid file removed");
        return Ok(());
    }
    crate::daemon::send_sigterm(pid)?;
    println!("SIGTERM → pid {pid}");
    Ok(())
}

pub fn reset(force: bool) -> Result<()> {
    use std::io::Write;
    let paths = [
        crate::config::config_path()?,
        crate::matrix::store_path()?,
        crate::names::default_store_path()?,
    ];
    eprintln!("will remove:");
    for p in &paths {
        eprintln!("  {}", p.display());
    }
    if !force {
        eprint!("proceed? [y/N] ");
        std::io::stderr().flush().ok();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).context("read stdin")?;
        if !buf.trim().eq_ignore_ascii_case("y") {
            eprintln!("aborted");
            return Ok(());
        }
    }
    for p in &paths {
        if p.is_dir() {
            std::fs::remove_dir_all(p).with_context(|| p.display().to_string())?;
            println!("removed {}", p.display());
        } else if p.exists() {
            std::fs::remove_file(p).with_context(|| p.display().to_string())?;
            println!("removed {}", p.display());
        }
    }
    println!("next: Element → Sessions → sign out the old matrirc device, then `matrirc login`.");
    Ok(())
}

pub fn install_irssi(force: bool, dry_run: bool, bin: Option<PathBuf>) -> Result<()> {
    // Bare `matrirc` resolves via $PATH at runtime, so the script survives
    // brew upgrades or the binary moving.
    let bin_str = match bin {
        Some(p) => p.to_str().ok_or_else(|| anyhow!("non-utf8 path: {}", p.display()))?.to_owned(),
        None => "matrirc".to_owned(),
    };
    let script = render_from_str(&bin_str)?;

    if dry_run {
        print!("{script}");
        return Ok(());
    }

    let (script_path, autorun_link) = irssi_paths()?;
    if script_path.exists() && !force {
        return Err(anyhow!("{} exists; use --force", script_path.display()));
    }
    if let Some(d) = script_path.parent() {
        std::fs::create_dir_all(d).context("create scripts dir")?;
    }
    std::fs::write(&script_path, script).context("write script")?;
    println!("installed {}", script_path.display());

    if let Some(d) = autorun_link.parent() {
        std::fs::create_dir_all(d).context("create autorun dir")?;
    }
    if autorun_link.symlink_metadata().is_ok() {
        if !force {
            return Err(anyhow!("{} exists; use --force", autorun_link.display()));
        }
        std::fs::remove_file(&autorun_link).context("remove autorun symlink")?;
    }
    std::os::unix::fs::symlink("../matrirc.pl", &autorun_link).context("symlink autorun")?;
    println!("symlinked {} → ../matrirc.pl", autorun_link.display());
    println!("now: /script load matrirc");
    Ok(())
}

fn render_from_str(bin: &str) -> Result<String> {
    if bin.contains('\'') {
        return Err(anyhow!("binary path contains a single quote, refusing to embed: {bin}"));
    }
    Ok(PERL_TEMPLATE.replace("__MATRIRC_BIN__", bin))
}

fn irssi_paths() -> Result<(PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    let scripts = PathBuf::from(home).join(".irssi").join("scripts");
    let script = scripts.join("matrirc.pl");
    let autorun = scripts.join("autorun").join("matrirc.pl");
    Ok((script, autorun))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_substitutes_path() {
        let s = render_from_str("/opt/matrirc/bin/matrirc").unwrap();
        assert!(s.contains("'/opt/matrirc/bin/matrirc'"));
        assert!(!s.contains("__MATRIRC_BIN__"));
    }

    #[test]
    fn render_substitutes_bare_command() {
        let s = render_from_str("matrirc").unwrap();
        assert!(s.contains("'matrirc'"));
    }

    #[test]
    fn render_rejects_quote_injection() {
        assert!(render_from_str("/tmp/'; rm -rf /; echo '").is_err());
    }

    #[test]
    fn template_has_placeholder_and_required_keys() {
        assert!(PERL_TEMPLATE.contains("__MATRIRC_BIN__"));
        assert!(PERL_TEMPLATE.contains("Irssi::timeout_add"));
        assert!(PERL_TEMPLATE.contains("connect matrirc"));
    }
}
