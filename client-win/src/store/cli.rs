//! `bubbleplay-client key ...` and `bubbleplay-client pcs ...`.
//!
//! ```text
//! key create [--label L] [--passphrase-prompt]
//! key import <file.bpkey>
//! key export <file.bpkey>
//! key show
//! pcs list
//! pcs remove <id>
//! ```

use std::io::Write as _;
use std::path::Path;

use anyhow::{bail, Context, Result};

use super::console::read_passphrase;
use super::key::KeyStore;
use super::pcs::PcStore;

/// True when the arguments start a store subcommand rather than the UI or a dev session.
pub fn is_subcommand(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("key" | "pcs"))
}

/// Runs a store subcommand. `args` starts with `key` or `pcs`.
pub fn run(args: &[String]) -> Result<()> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["key", "create", rest @ ..] => key_create(rest),
        ["key", "import", file] => key_import(Path::new(file)),
        ["key", "export", file] => key_export(Path::new(file)),
        ["key", "show"] => key_show(),
        ["pcs", "list"] => pcs_list(),
        ["pcs", "remove", id] => pcs_remove(id),
        _ => bail!(
            "usage: key create [--label L] [--passphrase-prompt] | key import <file> | \
             key export <file> | key show | pcs list | pcs remove <id>"
        ),
    }
}

fn key_create(flags: &[&str]) -> Result<()> {
    let mut label = None;
    let mut ask = false;
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        match *flag {
            "--label" => label = Some(flags.next().context("--label requires a value")?),
            "--passphrase-prompt" => ask = true,
            other => bail!("unknown key create option: {other}"),
        }
    }
    let label = match label {
        Some(label) => (*label).to_owned(),
        None => std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows viewer".to_owned()),
    };
    let store = KeyStore::default_location()?;
    if store.exists() {
        bail!("a viewer key already exists at {}", store.path().display());
    }
    let passphrase = if ask {
        let first = read_passphrase("New key passphrase: ")?;
        if first.is_empty() {
            bail!("the passphrase is empty; leave out --passphrase-prompt for a key without one");
        }
        let second = read_passphrase("Repeat the passphrase: ")?;
        if *first != *second {
            bail!("the passphrases do not match");
        }
        Some(first)
    } else {
        None
    };
    let key = store.create(&label, passphrase.as_ref().map(|p| p.as_str()))?;
    print_line(&key.id().to_string())
}

fn key_import(file: &Path) -> Result<()> {
    let store = KeyStore::default_location()?;
    let key = super::key::read_plain(file)?;
    let prompt = format!("Passphrase for {}: ", key.id());
    let imported = store.import(key, &mut || read_passphrase(&prompt))?;
    print_line(&imported.id().to_string())
}

fn key_export(file: &Path) -> Result<()> {
    let store = KeyStore::default_location()?;
    let text = store.export(&mut |id| read_passphrase(&format!("Passphrase for {id}: ")))?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .with_context(|| {
            format!(
                "create {} (an existing file is not replaced)",
                file.display()
            )
        })?;
    out.write_all(text.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .with_context(|| format!("write {}", file.display()))?;
    print_line(&format!("exported the viewer key to {}", file.display()))
}

fn key_show() -> Result<()> {
    let key = KeyStore::default_location()?.load()?;
    print_line(&key.id().to_string())
}

fn pcs_list() -> Result<()> {
    let entries = PcStore::default_location()?.load()?;
    if entries.is_empty() {
        return print_line("no PCs");
    }
    for entry in entries {
        let pinned = if entry.public.is_empty() {
            "not paired"
        } else {
            "paired"
        };
        print_line(&format!(
            "{}  {}  {}  {pinned}",
            entry.id, entry.name, entry.relay
        ))?;
    }
    Ok(())
}

fn pcs_remove(id: &str) -> Result<()> {
    if !PcStore::default_location()?.remove(id)? {
        bail!("PC {id} is not in the list");
    }
    print_line(&format!("removed {id}"))
}

fn print_line(text: &str) -> Result<()> {
    let mut out = std::io::stdout();
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .context("write to standard output")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn subcommands_are_recognised_and_malformed_ones_refused() {
        assert!(is_subcommand(&args(&["key", "show"])));
        assert!(is_subcommand(&args(&["pcs", "list"])));
        assert!(!is_subcommand(&args(&["--ui-demo"])));
        assert!(!is_subcommand(&args(&[])));
        for bad in [
            &["key"][..],
            &["key", "import"],
            &["key", "show", "extra"],
            &["pcs", "remove"],
            &["pcs", "add", "x"],
            &["key", "create", "--label"],
            &["key", "create", "--bogus"],
        ] {
            assert!(run(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
