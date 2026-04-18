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
    },
    /// Log in to a Matrix homeserver. By default prompts for your password
    /// (m.login.password) and then walks you through SAS emoji verification
    /// from another device to enable E2EE rooms. Pass --token to use an
    /// existing access token instead (read from MATRIRC_TOKEN or stdin);
    /// --skip-verify to delay E2EE setup until `matrirc bootstrap-e2ee`.
    Login {
        /// Your Matrix user ID, e.g. @you:matrix.org
        mxid: String,
        /// Override the homeserver URL instead of resolving via .well-known.
        #[arg(long)]
        homeserver: Option<String>,
        /// Use an access token instead of password login.
        #[arg(long)]
        token: bool,
        /// Skip SAS device verification after login.
        #[arg(long)]
        skip_verify: bool,
    },
    /// One-shot import of cross-signing + message backup key using a Secure Secret
    /// Storage recovery key. Required once per fresh crypto store so E2EE rooms decrypt.
    /// Key is read from MATRIRC_RECOVERY_KEY env var, or piped on stdin. The key is
    /// not persisted by matrirc anywhere.
    BootstrapE2ee,
    /// Wipe local matrirc state (config, crypto store, name store) so the next
    /// login creates a fresh device. Does NOT sign the device out on the
    /// homeserver — do that in Element → Settings → Sessions.
    Reset {
        /// Skip the confirmation prompt.
        #[arg(long)]
        force: bool,
    },
    /// Report whether a matrirc daemon is currently running.
    Status,
    /// Stop the running daemon (SIGTERM).
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
    println!(
        "✓ logged in as {} (device {}) on {}",
        cfg.mxid, cfg.device_id, cfg.homeserver_url
    );
    println!("  config written to {}", path.display());

    if skip_verify {
        println!();
        println!("--skip-verify set. Encrypted rooms won't decrypt until you run");
        println!("  matrirc bootstrap-e2ee   (recovery-key path)   OR");
        println!("  matrirc login --skip-verify=false  (to re-run SAS verification).");
        return Ok(());
    }

    println!();
    match matrix::run_sas_bootstrap(&client).await {
        Ok(true) => {
            println!("✓ device verified. E2EE ready. Start the daemon with `matrirc run`.");
        }
        Ok(false) => {
            println!("device is already trusted. E2EE ready. Start the daemon with `matrirc run`.");
        }
        Err(e) => {
            println!("× SAS verification didn't finish: {e}");
            println!("  Encrypted rooms won't decrypt until you retry. Options:");
            println!("    matrirc login    # retry SAS verification");
            println!("    matrirc bootstrap-e2ee    # paste your recovery key instead");
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

pub fn status() -> Result<()> {
    let pid_path = crate::daemon::pid_file_path()?;
    match crate::daemon::read_pid(&pid_path)? {
        Some(pid) if crate::daemon::alive(pid) => {
            println!("daemon running (pid {pid}) — {}", pid_path.display());
        }
        Some(pid) => {
            println!("stale pid file (pid {pid} not running) — {}", pid_path.display());
        }
        None => println!("daemon not running"),
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    let pid_path = crate::daemon::pid_file_path()?;
    let Some(pid) = crate::daemon::read_pid(&pid_path)? else {
        println!("no pid file — daemon probably not running");
        return Ok(());
    };
    if !crate::daemon::alive(pid) {
        println!("pid {pid} not running; removing stale pid file");
        let _ = std::fs::remove_file(&pid_path);
        return Ok(());
    }
    crate::daemon::send_sigterm(pid)?;
    println!("sent SIGTERM to pid {pid}");
    Ok(())
}

pub fn reset(force: bool) -> Result<()> {
    use std::io::Write;
    let paths = [
        crate::config::config_path()?,
        crate::matrix::store_path()?,
        crate::names::default_store_path()?,
    ];

    eprintln!("matrirc reset will remove:");
    for p in &paths {
        eprintln!("  {}", p.display());
    }
    eprintln!();

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
            std::fs::remove_dir_all(p)
                .with_context(|| format!("remove dir {}", p.display()))?;
            println!("removed dir {}", p.display());
        } else if p.exists() {
            std::fs::remove_file(p)
                .with_context(|| format!("remove {}", p.display()))?;
            println!("removed {}", p.display());
        }
    }

    println!();
    println!("next steps:");
    println!("  1. Element → Settings → Sessions → sign out the old matrirc device.");
    println!("  2. matrirc login @you:server.org");
    println!("       prompts for password, then walks you through emoji verification");
    println!("       from another Element device to enable E2EE rooms.");
    println!("  3. matrirc run");
    Ok(())
}

pub fn install_irssi(force: bool, dry_run: bool) -> Result<()> {
    let bin = std::env::current_exe().context("current_exe")?;
    let script = render(&bin)?;

    if dry_run {
        print!("{script}");
        return Ok(());
    }

    let (script_path, autorun_link) = irssi_paths()?;

    if script_path.exists() && !force {
        return Err(anyhow!(
            "{} already exists; pass --force to overwrite",
            script_path.display()
        ));
    }
    std::fs::create_dir_all(script_path.parent().unwrap())
        .with_context(|| format!("create {}", script_path.parent().unwrap().display()))?;
    std::fs::write(&script_path, script)
        .with_context(|| format!("write {}", script_path.display()))?;
    println!("installed {}", script_path.display());

    std::fs::create_dir_all(autorun_link.parent().unwrap())
        .with_context(|| format!("create {}", autorun_link.parent().unwrap().display()))?;
    if autorun_link.exists() || autorun_link.symlink_metadata().is_ok() {
        if force {
            std::fs::remove_file(&autorun_link)
                .with_context(|| format!("remove {}", autorun_link.display()))?;
        } else {
            return Err(anyhow!(
                "{} already exists; pass --force to overwrite",
                autorun_link.display()
            ));
        }
    }
    std::os::unix::fs::symlink("../matrirc.pl", &autorun_link)
        .with_context(|| format!("symlink {}", autorun_link.display()))?;
    println!("symlinked {} -> ../matrirc.pl", autorun_link.display());

    println!("now: /script load matrirc  (or restart irssi)");
    Ok(())
}

fn render(bin: &std::path::Path) -> Result<String> {
    let bin_str = bin
        .to_str()
        .ok_or_else(|| anyhow!("binary path is not valid UTF-8: {}", bin.display()))?;
    if bin_str.contains('\'') {
        return Err(anyhow!(
            "binary path contains a single quote, refusing to embed: {bin_str}"
        ));
    }
    Ok(PERL_TEMPLATE.replace("__MATRIRC_BIN__", bin_str))
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
    use std::path::Path;

    #[test]
    fn render_substitutes_path() {
        let s = render(Path::new("/opt/matrirc/bin/matrirc")).unwrap();
        assert!(s.contains("'/opt/matrirc/bin/matrirc'"));
        assert!(!s.contains("__MATRIRC_BIN__"));
    }

    #[test]
    fn render_rejects_quote_injection() {
        let bad = Path::new("/tmp/'; rm -rf /; echo '");
        assert!(render(bad).is_err());
    }

    #[test]
    fn template_has_placeholder_and_required_keys() {
        assert!(PERL_TEMPLATE.contains("__MATRIRC_BIN__"));
        assert!(PERL_TEMPLATE.contains("Irssi::timeout_add"));
        assert!(PERL_TEMPLATE.contains("connect $HOST $PORT"));
    }
}
