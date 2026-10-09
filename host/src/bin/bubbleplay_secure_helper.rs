//! Scheduled task target. No implicit mode, dev bypass or permanent service.
#![windows_subsystem = "windows"]
#[allow(dead_code)]
#[path = "../tray/logic.rs"]
mod logic;
#[derive(Debug, PartialEq, Eq)]
enum Command {
    Launch,
    Serve,
    SetAdminMode(bool),
}
fn parse_args(args: &[String]) -> anyhow::Result<Command> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--launch"] => Ok(Command::Launch),
        ["--serve"] => Ok(Command::Serve),
        ["--set-admin-mode", "on"] => Ok(Command::SetAdminMode(true)),
        ["--set-admin-mode", "off"] => Ok(Command::SetAdminMode(false)),
        _ => Err(anyhow::anyhow!(
            "expected --launch, --serve, or --set-admin-mode on|off"
        )),
    }
}
fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let command = parse_args(&args)?;
    if let Command::SetAdminMode(on) = command {
        // This branch also runs as a UAC-elevated console administrator,
        // unlike launch/serve. It needs no SYSTEM-only logging setup.
        return bubbleplay_host::admin_mode::set_admin_mode(on);
    }
    let Ok(path) = bubbleplay_host::secure_helper::trust::log_path() else {
        anyhow::bail!("helper requires protected SYSTEM execution");
    };
    let log = logic::SharedLog::new(path);
    let _ = tracing_subscriber::fmt()
        .with_writer(move || log.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .try_init();
    match command {
        Command::Launch => bubbleplay_host::secure_helper::runtime::launch(),
        Command::Serve => bubbleplay_host::secure_helper::runtime::serve(),
        Command::SetAdminMode(_) => unreachable!(),
    }
}
fn main() {
    if let Err(e) = run() {
        eprintln!("secure helper stopped: {e}");
        tracing::error!(error=%e,"secure helper stopped");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> anyhow::Result<Command> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }
    #[test]
    fn explicit_commands_parse() {
        assert_eq!(parse(&["--launch"]).unwrap(), Command::Launch);
        assert_eq!(parse(&["--serve"]).unwrap(), Command::Serve);
        assert_eq!(
            parse(&["--set-admin-mode", "on"]).unwrap(),
            Command::SetAdminMode(true)
        );
        assert_eq!(
            parse(&["--set-admin-mode", "off"]).unwrap(),
            Command::SetAdminMode(false)
        );
    }
    #[test]
    fn admin_mode_arguments_are_exact_and_exclusive() {
        for args in [
            vec![],
            vec!["--set-admin-mode"],
            vec!["--set-admin-mode", "ON"],
            vec!["--set-admin-mode", "1"],
            vec!["--set-admin-mode", ""],
            vec!["--set-admin-mode", "off", "on"],
            vec!["--set-admin-mode=on"],
            vec!["--launch", "--set-admin-mode", "on"],
        ] {
            assert!(parse(&args).is_err());
        }
    }
}
