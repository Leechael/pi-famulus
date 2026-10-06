use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pi-famulus");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run pi-famulus")
}

/// Body of one help section, from the heading through the blank line before the next.
fn help_section<'a>(help: &'a str, heading: &str) -> &'a str {
    let marker = format!("\n{heading}:\n");
    let start = help
        .find(&marker)
        .unwrap_or_else(|| panic!("{heading} missing from help:\n{help}"));
    let body = &help[start + marker.len()..];
    let end = body.find("\n\n").unwrap_or(body.len());
    &body[..end]
}

fn section_has_command(section: &str, name: &str) -> bool {
    section.lines().any(|line| {
        let rest = line.trim_start().strip_prefix(name);
        rest.is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()))
    })
}

#[test]
fn concurrent_capacity_config_sets_preserve_fields_and_valid_json() {
    let home = std::env::temp_dir().join(format!("pi-famulus-config-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.json"), r#"{"managerPath":"/tmp/manager","goneSessionRetention":"2h","unrelated":{"keep":true}}"#).unwrap();
    let workers: Vec<_> = (1..=16).map(|n| {
        let home = home.clone();
        std::thread::spawn(move || {
            let mut command = Command::new(BIN);
            command
                .arg("--home")
                .arg(&home)
                .args(["config", "set", "max-agents"])
                .arg(n.to_string())
                .output()
                .unwrap()
        })
    }).collect();
    for worker in workers {
        let output = worker.join().unwrap();
        assert!(output.status.success(), "config set failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("config.json")).unwrap()).unwrap();
    assert!((1..=16).contains(&config["maxAgents"].as_u64().unwrap()));
    assert_eq!(config["managerPath"], "/tmp/manager");
    assert_eq!(config["goneSessionRetention"], "2h");
    assert_eq!(config["unrelated"]["keep"], true);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn help_and_version_are_served_by_the_cli_framework() {
    let help = run(&["--help"]);
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Process management daemon for pi-famulus"));
    assert!(help.contains("PI_FAMULUS_HOME"), "{help}");
    assert!(help.contains("~/.pi/agent/pi-famulus"), "{help}");
    assert!(help.contains("PI_FAMULUS_PAGER"), "{help}");
    assert!(help.contains("completion"));
    assert!(help.contains("list"));
    assert!(
        help.contains("ls"),
        "the list alias should be discoverable: {help}"
    );
    let commands = help_section(&help, "Commands");
    let inspection = help_section(&help, "Inspection");
    let acting = help_section(&help, "Acting on tasks");
    let daemon = help_section(&help, "Daemon");
    assert!(
        commands.contains("completion") && commands.contains("help"),
        "ungrouped commands stay with the built-in help: {help}"
    );
    for name in ["status", "sessions", "list", "show", "stats", "agent", "events", "log", "tail", "doctor"]
    {
        assert!(
            section_has_command(inspection, name),
            "{name} should be under Inspection: {help}"
        );
    }
    for name in ["output", "wait", "stop", "kill-session", "start"] {
        assert!(
            section_has_command(acting, name),
            "{name} should be under Acting on tasks: {help}"
        );
    }
    for name in ["config", "shutdown", "upgrade", "daemon"] {
        assert!(
            section_has_command(daemon, name),
            "{name} should be under Daemon: {help}"
        );
    }
    let status_at = inspection.find("status").expect("status");
    let doctor_at = inspection.find("doctor").expect("doctor");
    assert!(status_at < doctor_at, "inspection order: {help}");
    assert!(
        help.find("\nCommands:\n").unwrap()
            < help.find("\nInspection:\n").unwrap()
            && help.find("\nInspection:\n").unwrap() < help.find("\nActing on tasks:\n").unwrap()
            && help.find("\nActing on tasks:\n").unwrap() < help.find("\nDaemon:\n").unwrap(),
        "section order: {help}"
    );
    assert!(
        !help.contains("goneSessionRetention"),
        "top-level help should stay short; details belong on subcommand --help: {help}"
    );
    let daemon_help = run(&["daemon", "--help"]);
    assert!(
        daemon_help.status.success(),
        "{}",
        String::from_utf8_lossy(&daemon_help.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&daemon_help.stdout).contains("--handover"),
        "internal flags stay hidden"
    );

    let subcommand_help = run(&["help", "list"]);
    assert!(subcommand_help.status.success());
    assert!(String::from_utf8_lossy(&subcommand_help.stdout).contains("--all"));

    let version = run(&["--version"]);
    assert!(version.status.success());
    let expected_prefix = format!("pi-famulus {}+", env!("CARGO_PKG_VERSION"));
    assert!(String::from_utf8_lossy(&version.stdout).starts_with(&expected_prefix));
}

#[test]
fn generated_completion_script_is_available() {
    let output = run(&["completion", "--shell", "fish"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("# @generated by usage-argv"));
    for shell in ["bash", "zsh", "fish"] {
        let output = run(&["completion", "--shell", shell]);
        assert!(output.status.success());
        let script = String::from_utf8_lossy(&output.stdout);
        assert!(script.contains("pi-famulus"), "{shell}: {script}");
    }

    let unsupported = run(&["completion", "--shell", "powershell"]);
    assert_eq!(
        unsupported.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&unsupported.stdout),
        String::from_utf8_lossy(&unsupported.stderr)
    );
}

#[test]
fn subcommands_are_required_and_flags_stay_command_scoped() {
    let bare = run(&[]);
    assert_eq!(
        bare.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&bare.stdout),
        String::from_utf8_lossy(&bare.stderr)
    );

    let output = run(&["sessions", "-a"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn handover_check_uses_the_exact_product_marker() {
    let output = run(&["__handover-check"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    let fields: Vec<_> = text.split_whitespace().collect();
    assert_eq!(fields.len(), 3, "{text}");
    assert_eq!(fields[0], "pi-famulus-handover");
    assert_eq!(fields[1], "1");
    assert!(fields[2].starts_with(concat!(env!("CARGO_PKG_VERSION"), "+")));
}

#[test]
fn doctor_resolves_the_new_home_and_environment_contract() {
    let root = std::env::temp_dir().join(format!("pi-famulus-cli-home-{}", std::process::id()));
    let env_home = root.join("env");
    let flag_home = root.join("flag");
    let doctor = |home_env: Option<&str>, flag: bool| {
        let mut cmd = Command::new(BIN);
        cmd.env("HOME", &root).env_remove("PI_FAMULUS_HOME");
        if let Some(value) = home_env {
            cmd.env("PI_FAMULUS_HOME", value);
        }
        if flag {
            cmd.arg("--home").arg(&flag_home);
        }
        cmd.arg("doctor").output().unwrap()
    };
    let expected_default = root.join(".pi/agent/pi-famulus");
    for (home_env, flag, expected) in [
        (None, false, &expected_default),
        (Some(""), false, &expected_default),
        (Some(env_home.to_str().unwrap()), false, &env_home),
        (Some(env_home.to_str().unwrap()), true, &flag_home),
    ] {
        let output = doctor(home_env, flag);
        assert_eq!(output.status.code(), Some(1));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.starts_with(&format!("home:   {}\n", expected.display())), "{text}");
        assert!(text.contains("PI_FAMULUS_HOME"), "{text}");
        assert!(!expected.exists(), "doctor must not create or start a manager");
    }
}

#[test]
fn unknown_flags_remain_errors() {
    let output = run(&["list", "--definitely-unknown"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("--definitely-unknown"));
}
