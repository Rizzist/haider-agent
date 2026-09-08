//! One command grammar for the control executable and its interactive host.
use super::{BareTuiOptions, parse_bare_tui_options, ssh};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveCommand<'a> {
    FrontDoor(BareTuiOptions),
    Tui(&'a [String]),
    Ssh(&'a [String]),
}

#[derive(Debug)]
pub enum Command<'a> {
    Version,
    SelfTest,
    InstallBundle(&'a str, &'a str),
    Ready,
    Interactive(InteractiveCommand<'a>),
    Run(&'a [String]),
    Agent(&'a str, &'a [String]),
    Status(&'a [String]),
    SessionsWaitReady(&'a [String]),
    Daemon(&'a [String]),
    Sessions(&'a [String]),
    SessionConfig(&'a str, &'a [String]),
    SessionRecover(&'a str, &'a [String]),
    SessionSeen(&'a str, &'a [String]),
    SessionWorkspace(Option<&'a str>, &'a [String]),
    SessionItem(&'a str, &'a str, &'a [String]),
    SessionProvider(&'a [String]),
    SessionRetract(&'a [String]),
    Session(&'a [String]),
    SessionSubmit(&'a [String]),
    Account(&'a [String]),
    Provider(&'a [String]),
    Lockdown(&'a [String]),
    Models(&'a [String]),
    Fleet(&'a [String]),
    Events(&'a [String]),
    Graph(&'a [String]),
    Export(&'a [String]),
    Hooks(&'a [String]),
    Ssh(&'a [String]),
    Shell(&'a [String]),
    Update(&'a [String]),
    ResumeJson(&'a [String]),
    Import(&'a [String]),
    Unknown(&'a str),
}

/// Select the execution boundary without creating a runtime, resolving a
/// profile, connecting, or inspecting/loading the interactive executable.
pub fn parse_command(args: &[String]) -> Result<Command<'_>, String> {
    if let Some(options) = parse_bare_tui_options(args)? {
        return Ok(Command::Interactive(InteractiveCommand::FrontDoor(options)));
    }
    Ok(match args {
        [flag, source, destination] if flag == "--install-bundle" => {
            Command::InstallBundle(source, destination)
        }
        [v] if matches!(v.as_str(), "--version" | "-V" | "version") => Command::Version,
        [c] if c == "self-test" => Command::SelfTest,
        [c] if c == "--ready" => Command::Ready,
        [c, r @ ..] if c == "run" => Command::Run(r),
        [c, r @ ..] if c == "agent" || c == "workflow" => Command::Agent(c, r),
        [c, r @ ..] if c == "status" => Command::Status(r),
        [c, s, r @ ..] if c == "sessions" && s == "wait-ready" => Command::SessionsWaitReady(r),
        [c, r @ ..] if c == "daemon" => Command::Daemon(r),
        [c, r @ ..] if c == "sessions" => Command::Sessions(r),
        [c, s, r @ ..] if c == "session" && s == "submit" => Command::SessionSubmit(r),
        [c, id, s, r @ ..] if c == "session" && s == "config" => Command::SessionConfig(id, r),
        [c, id, s, r @ ..] if c == "session" && s == "recover" => Command::SessionRecover(id, r),
        [c, id, s, r @ ..] if c == "session" && s == "seen" => Command::SessionSeen(id, r),
        [c, s, r @ ..] if c == "session" && s == "workspace" => Command::SessionWorkspace(None, r),
        [c, id, s, r @ ..] if c == "session" && s == "workspace" => {
            Command::SessionWorkspace(Some(id), r)
        }
        [c, id, s, seq, r @ ..] if c == "session" && s == "item" => {
            Command::SessionItem(id, seq, r)
        }
        [c, p, s, r @ ..] if c == "session" && p == "provider" && s == "rebind" => {
            Command::SessionProvider(r)
        }
        [c, s, r @ ..] if c == "session" && s == "retract" => Command::SessionRetract(r),
        [c, r @ ..] if c == "session" => Command::Session(r),
        [c, r @ ..] if c == "account" => Command::Account(r),
        [c, r @ ..] if c == "provider" => Command::Provider(r),
        [c, r @ ..] if c == "lockdown" => Command::Lockdown(r),
        [c, r @ ..] if c == "models" => Command::Models(r),
        [c, r @ ..] if c == "fleet" => Command::Fleet(r),
        [c, r @ ..] if c == "events" => Command::Events(r),
        [c, r @ ..] if c == "graph" => Command::Graph(r),
        [c, r @ ..] if c == "export" => Command::Export(r),
        [c, r @ ..] if c == "hooks" => Command::Hooks(r),
        [c, r @ ..] if c == "ssh" && ssh::is_interactive_shell(r) => {
            Command::Interactive(InteractiveCommand::Ssh(r))
        }
        [c, r @ ..] if c == "ssh" => Command::Ssh(r),
        [c, r @ ..] if c == "shell" => Command::Shell(r),
        [c, r @ ..] if c == "update" => Command::Update(r),
        [c, r @ ..] if matches!(c.as_str(), "tui" | "talk") => {
            Command::Interactive(InteractiveCommand::Tui(r))
        }
        [c, r @ ..] if c == "resume" && r.iter().any(|a| a == "--json") => Command::ResumeJson(r),
        [c, r @ ..] if c == "resume" => {
            let options = match r {
                [] => BareTuiOptions {
                    browse_sessions: true,
                    ..BareTuiOptions::default()
                },
                [id] if !id.is_empty() && !id.starts_with('-') => BareTuiOptions {
                    session: Some(id.clone()),
                    ..BareTuiOptions::default()
                },
                _ => return Err("usage: haider resume [<session-id>]".into()),
            };
            Command::Interactive(InteractiveCommand::FrontDoor(options))
        }
        [c, r @ ..] if c == "import" => Command::Import(r),
        [other, ..] => Command::Unknown(other),
        [] => unreachable!("bare arguments handled above"),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }
    // Exhaustive projection: a new command variant must explicitly choose an
    // execution boundary here, so additions cannot silently escape the audit.
    fn interactive(command: Command<'_>) -> bool {
        match command {
            Command::Interactive(_) => true,
            Command::Version
            | Command::SelfTest
            | Command::InstallBundle(..)
            | Command::Ready
            | Command::Run(_)
            | Command::Agent(..)
            | Command::Status(_)
            | Command::SessionsWaitReady(_)
            | Command::Daemon(_)
            | Command::Sessions(_)
            | Command::SessionConfig(..)
            | Command::SessionRecover(..)
            | Command::SessionSeen(..)
            | Command::SessionWorkspace(..)
            | Command::SessionItem(..)
            | Command::SessionProvider(_)
            | Command::SessionRetract(_)
            | Command::Session(_)
            | Command::SessionSubmit(_)
            | Command::Account(_)
            | Command::Provider(_)
            | Command::Lockdown(_)
            | Command::Models(_)
            | Command::Fleet(_)
            | Command::Events(_)
            | Command::Graph(_)
            | Command::Export(_)
            | Command::Hooks(_)
            | Command::Ssh(_)
            | Command::Shell(_)
            | Command::Update(_)
            | Command::ResumeJson(_)
            | Command::Import(_)
            | Command::Unknown(_) => false,
        }
    }
    #[test]
    fn every_interactive_entry_selects_the_payload() {
        for argv in [
            vec![],
            vec!["tui"],
            vec!["talk"],
            vec!["tui", "--demo", "--plain"],
            vec!["resume"],
            vec!["resume", "session-a"],
            vec!["--resume"],
            vec!["--resume", "session-a"],
            vec!["--session", "session-a"],
            vec!["--no-update-check"],
            vec!["ssh", "shell", "host"],
        ] {
            assert!(
                interactive(parse_command(&args(&argv)).expect("valid route")),
                "{argv:?}"
            );
        }
    }
    #[test]
    fn every_headless_entry_stays_in_control_executable() {
        for argv in [
            vec!["--version"],
            vec!["-V"],
            vec!["version"],
            vec!["self-test"],
            vec!["--install-bundle", "stage", "bin"],
            vec!["--ready"],
            vec!["run"],
            vec!["agent"],
            vec!["workflow"],
            vec!["status"],
            vec!["sessions"],
            vec!["sessions", "wait-ready"],
            vec!["daemon"],
            vec!["session", "id"],
            vec!["session", "id", "config"],
            vec!["session", "id", "recover"],
            vec!["session", "id", "seen"],
            vec!["session", "workspace"],
            vec!["session", "id", "workspace"],
            vec!["session", "id", "item", "1"],
            vec!["session", "provider", "rebind"],
            vec!["session", "retract"],
            vec!["account"],
            vec!["provider"],
            vec!["lockdown"],
            vec!["models"],
            vec!["fleet"],
            vec!["events"],
            vec!["graph"],
            vec!["export"],
            vec!["hooks"],
            vec!["ssh", "list"],
            vec!["ssh", "shell", "host", "--", "echo hi"],
            vec!["shell"],
            vec!["update"],
            vec!["resume", "id", "--json"],
            vec!["import"],
            vec!["unknown"],
        ] {
            assert!(
                !interactive(parse_command(&args(&argv)).expect("headless route")),
                "{argv:?}"
            );
        }
    }
    #[test]
    fn removed_peer_verbs_are_unknown() {
        for verb in ["list", "send", "name", "watch", "wait-idle"] {
            let argv = args(&["peer", verb]);
            assert!(matches!(parse_command(&argv), Ok(Command::Unknown("peer"))));
        }
    }
    #[test]
    fn ssh_mode_and_resume_usage_keep_their_existing_grammar() {
        assert!(!interactive(
            parse_command(&args(&["ssh", "shell", "host", "--"])).expect("SSH usage route")
        ));
        assert!(parse_command(&args(&["resume", "a", "b"])).is_err());
        assert!(parse_command(&args(&["--session"])).is_err());
        assert_eq!(
            parse_command(&args(&["--resume", "a", "--no-update-check"]))
                .map(|command| match command {
                    Command::Interactive(InteractiveCommand::FrontDoor(options)) => Some(options),
                    _ => None,
                })
                .expect("options"),
            Some(BareTuiOptions {
                session: Some("a".into()),
                no_update_check: true,
                browse_sessions: false
            })
        );
    }
}
