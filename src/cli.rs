use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

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
