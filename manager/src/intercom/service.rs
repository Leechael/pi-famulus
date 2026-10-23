use super::{
    cli::{Options, ServiceCommand},
    config::Config,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const LABEL: &str = "com.agent-intercom.client";
const BINARY_NAME: &str = "pi-famulus";
const READY_WAIT: Duration = Duration::from_secs(5);
const READY_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Default)]
struct Status {
    installed: bool,
    loaded: bool,
    running: bool,
    state: String,
    pid: i64,
    file: PathBuf,
    log_path: PathBuf,
}

struct Detected {
    executable: PathBuf,
    manager_home: PathBuf,
    config_path: PathBuf,
    log_path: PathBuf,
    path: String,
    token_source: &'static str,
    token_value: String,
    would_save: Vec<&'static str>,
    tools: BTreeMap<String, String>,
    environment: Vec<(String, String)>,
}

pub async fn run(
    home: &Path,
    cfg: &Config,
    opts: &Options,
    action: ServiceCommand,
) -> Result<(), String> {
    // Keep the implementation compiled on Linux too; only execution needs launchd.
    if !cfg!(target_os = "macos") {
        return Err(format!(
            "service management is currently supported only on macOS (got {})",
            std::env::consts::OS
        ));
    }
    match action {
        ServiceCommand::Print => {
            let detected = prepare(home, cfg, opts).await?;
            let content = render_with_token_check(&detected, &cfg.token)?;
            warn_config_permissions(&detected.config_path);
            crate::out::bytes(content.as_bytes());
        }
        ServiceCommand::Install => {
            install(home, cfg, opts).await?;
            crate::out::line("service installed and daemon is ready");
        }
        ServiceCommand::Uninstall => {
            if loaded().await? {
                launchctl(&["bootout", &launch_service()]).await?;
            }
            match std::fs::remove_file(plist_path()) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
            crate::out::line("service uninstalled");
        }
        ServiceCommand::Start | ServiceCommand::Restart => {
            let plist = plist_path();
            match std::fs::metadata(&plist) {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(
                        "service is not installed; run `pi-famulus intercom service install` first"
                            .into(),
                    );
                }
                Err(e) => return Err(e.to_string()),
            }
            reject_external_daemon(home, cfg).await?;
            let mut restart = matches!(action, ServiceCommand::Restart);
            // An agent installed by an older release runs other arguments;
            // render it afresh so launchd reloads the current ones. If the
            // current configuration cannot render (install would refuse it),
            // keep the installed agent: `intercom serve` still runs `serve`.
            let rendered = match prepare(home, cfg, opts).await {
                Ok(detected) => render_with_token_check(&detected, &cfg.token),
                Err(error) => Err(error),
            };
            match rendered {
                Ok(content) => {
                    if std::fs::read(&plist).map_or(true, |current| current != content.as_bytes()) {
                        write_private_atomic(&plist, content.as_bytes(), false)?;
                        restart = true;
                    }
                }
                Err(error) => eprintln!("keeping the installed launch agent: {error}"),
            }
            if !loaded().await? {
                bootstrap(&plist).await?;
            } else if restart {
                // `kickstart -k` kills the job outright; bootout sends SIGTERM,
                // so the daemon settles intercom and stops its tasks first.
                launchctl(&["bootout", &launch_service()]).await?;
                bootstrap(&plist).await?;
            } else {
                launchctl(&["kickstart", &launch_service()]).await?;
            }
            crate::out::line(if matches!(action, ServiceCommand::Restart) {
                "service restarted"
            } else {
                "service started"
            });
        }
        ServiceCommand::Stop => {
            if loaded().await? {
                // kill leaves KeepAlive enabled and immediately respawns the job.
                launchctl(&["bootout", &launch_service()]).await?;
            }
            crate::out::line("service stopped");
        }
        ServiceCommand::Status => {
            let st = status().await?;
            let (report, issue) = match super::localrpc::call_raw(cfg, json!({"op":"status"})).await
            {
                Ok(response) if !response_error(&response).is_empty() => {
                    (None, format!("error ({})", response_error(&response)))
                }
                Ok(response) => (response.get("report").cloned(), String::new()),
                Err(error) => (None, format!("not responding ({error})")),
            };
            crate::out::bytes(format_status(&st, report.as_ref(), &cfg.sock, &issue).as_bytes());
        }
        ServiceCommand::Logs { follow } => {
            // Accept the compatibility flag; the original command also follows without it.
            let _ = follow;
            let log = log_path();
            std::fs::metadata(&log).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!("service log does not exist yet: {}", log.display())
                } else {
                    e.to_string()
                }
            })?;
            let result = tokio::process::Command::new("tail")
                .arg("-f")
                .arg(log)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()
                .await
                .map_err(|e| e.to_string())?;
            if !result.success() {
                return Err(format!("tail: {result}"));
            }
        }
    }
    Ok(())
}

fn plist_path() -> PathBuf {
    super::config::home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}
fn log_path() -> PathBuf {
    super::config::home().join("Library/Logs/agent-intercom/client.log")
}
fn uid() -> u32 {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}
fn launch_domain() -> String {
    format!("gui/{}", uid())
}
fn launch_service() -> String {
    format!("{}/{LABEL}", launch_domain())
}
fn absolute(path: &Path) -> Result<PathBuf, String> {
    std::path::absolute(path)
        .map(|path| clean_path(&path))
        .map_err(|e| e.to_string())
}

fn read_config(path: &Path) -> Result<toml::Table, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|e| format!("read config {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(format!("read config {}: {e}", path.display())),
    }
}
fn file_token(values: &toml::Table) -> &str {
    values
        .get("token")
        .and_then(toml::Value::as_str)
        .unwrap_or("")
}
fn file_local_only(values: &toml::Table) -> bool {
    values
        .get("local_only")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
}

fn token_plan(
    values: &toml::Table,
    token: &str,
    local_only: bool,
    save_token: bool,
    config_path: &Path,
) -> Result<(&'static str, String, Vec<&'static str>), String> {
    let saved = file_token(values);
    let mut would_save = Vec::new();
    let (source, value) = if !token.is_empty() && save_token && saved != token {
        would_save.push("token to config");
        ("would save to config", token)
    } else if !saved.is_empty() {
        ("config", saved)
    } else if !(local_only || file_local_only(values)) {
        return Err(if token.is_empty() {
            format!(
                "token is required; put it in {} before installing the service",
                config_path.display()
            )
        } else {
            format!("the token is only available from the environment or a command-line flag; put it in {} or rerun with --save-token", config_path.display())
        });
    } else if !token.is_empty() {
        ("ignored in local_only mode", token)
    } else {
        ("none", "")
    };
    if local_only && !file_local_only(values) {
        would_save.push("local_only=true");
    }
    Ok((source, value.to_owned(), would_save))
}

async fn prepare(home: &Path, cfg: &Config, opts: &Options) -> Result<Detected, String> {
    let config_path = absolute(
        &opts
            .config
            .clone()
            .unwrap_or_else(super::config::default_path),
    )?;
    let values = read_config(&config_path)?;
    let (token_source, token_value, would_save) = token_plan(
        &values,
        &cfg.token,
        cfg.local_only,
        opts.save_token,
        &config_path,
    )?;
    let executable = executable()?;
    let (path, tools) = daemon_path(
        &std::env::var("PATH").unwrap_or_default(),
        &login_shell_path().await,
    );
    let environment = carried_environment(&path, std::env::vars());
    Ok(Detected {
        executable,
        manager_home: absolute(home)?,
        config_path,
        log_path: log_path(),
        path,
        token_source,
        token_value,
        would_save,
        tools,
        environment,
    })
}

fn launchd_socket(values: &toml::Table) -> PathBuf {
    match values.get("sock").and_then(toml::Value::as_str) {
        Some(sock) if !sock.is_empty() => PathBuf::from(sock),
        _ => std::env::temp_dir().join(format!("agent-intercom-{}/client.sock", uid())),
    }
}
fn validate_socket(service_sock: &Path, cli_sock: &Path, config_path: &Path) -> Result<(), String> {
    if !service_sock.is_absolute() {
        return Err(format!("sock {:?} in {} is relative, but launchd starts the daemon from a different working directory; use an absolute path", service_sock, config_path.display()));
    }
    if clean_path(service_sock) != clean_path(cli_sock) {
        return Err(format!("service will use socket {} but this command uses {}; set sock = {:?} in {} before installing", service_sock.display(), cli_sock.display(), cli_sock, config_path.display()));
    }
    Ok(())
}
fn clean_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if out.file_name().is_some_and(|p| p != "..") {
                    out.pop();
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            _ => out.push(part.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

async fn install(home: &Path, cfg: &Config, opts: &Options) -> Result<(), String> {
    let detected = prepare(home, cfg, opts).await?;
    render_with_token_check(&detected, &cfg.token)?;
    let service_sock = launchd_socket(&read_config(&detected.config_path)?);
    validate_socket(&service_sock, &cfg.sock, &detected.config_path)?;
    reject_external_daemon(home, cfg).await?;
    persist_install_config(
        &detected.config_path,
        &cfg.token,
        cfg.local_only,
        opts.save_token,
    )?;
    let detected = prepare(home, cfg, opts).await?;
    warn_config_permissions(&detected.config_path);
    let content = render_with_token_check(&detected, &cfg.token)?;
    let plist = plist_path();
    std::fs::create_dir_all(plist.parent().ok_or("plist parent required")?)
        .map_err(|e| e.to_string())?;
    std::fs::create_dir_all(detected.log_path.parent().ok_or("log parent required")?)
        .map_err(|e| e.to_string())?;
    if loaded().await? {
        launchctl(&["bootout", &launch_service()]).await?;
    }
    write_private_atomic(&plist, content.as_bytes(), false)?;
    bootstrap(&plist).await?;
    wait_ready(cfg).await
}

fn persist_install_config(
    path: &Path,
    token: &str,
    local_only: bool,
    save_token: bool,
) -> Result<(), String> {
    let mut values = read_config(path)?;
    let mut changed = false;
    if !token.is_empty() && save_token && file_token(&values) != token {
        values.insert("token".into(), toml::Value::String(token.into()));
        changed = true;
    }
    if local_only && !file_local_only(&values) {
        values.insert("local_only".into(), toml::Value::Boolean(true));
        changed = true;
    }
    if !changed {
        return Ok(());
    }
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let text = toml::to_string(&values).map_err(|e| format!("persist install config: {e}"))?;
    write_private_atomic(&target, text.as_bytes(), true)
        .map_err(|e| format!("persist install config: {e}"))
}

fn write_private_atomic(path: &Path, bytes: &[u8], private_parent: bool) -> Result<(), String> {
    let dir = path.parent().ok_or("file parent required")?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(if private_parent { 0o700 } else { 0o755 });
    }
    #[cfg(not(unix))]
    let _ = private_parent;
    builder.create(dir).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            options.mode(0o600);
            let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|e| e.to_string())?;
            file.write_all(bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        {
            let mut file = options.open(&tmp).map_err(|e| e.to_string())?;
            file.write_all(bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        if let Ok(file) = std::fs::File::open(dir) {
            let _ = file.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}
fn warn_config_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode != 0o600 {
                eprintln!("warning: config file permissions are {mode:04o}; chmod 600 {} to protect its token", path.display());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;")
}
fn comment_value(s: &str) -> String {
    // XML comments may not contain `--`; split every hyphen run.
    let mut text = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '-' && text.ends_with('-') {
            text.push(' ');
        }
        text.push(if c == '\n' || c == '\r' { ' ' } else { c });
    }
    xml_escape(&text)
}
fn render_with_token_check(d: &Detected, token: &str) -> Result<String, String> {
    let text = render_plist(d);
    for candidate in [token, d.token_value.as_str()] {
        if !candidate.is_empty()
            && (text.contains(candidate) || text.contains(&xml_escape(candidate)))
        {
            return Err("the rendered launch agent would contain the relay token; remove the token from paths and carried environment values".into());
        }
    }
    Ok(text)
}
fn render_plist(d: &Detected) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml_escape(s));
    let tool =
        |name: &str| comment_value(d.tools.get(name).map(String::as_str).unwrap_or("not found"));
    let mut text = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<!-- Generated by pi-famulus intercom service print. -->\n<!-- Detected: executable={}; config={}; token={}; claude={}; codex={}; pi={}; herdr={}; PATH-source=current+login; PATH={} -->\n", comment_value(&d.executable.to_string_lossy()), comment_value(&d.config_path.to_string_lossy()), d.token_source, tool("claude"), tool("codex"), tool("pi"), tool("herdr"), comment_value(&d.path));
    if !d.would_save.is_empty() {
        text.push_str(&format!(
            "<!-- Install preview: would save {} -->\n",
            comment_value(&d.would_save.join("; would save "))
        ));
    }
    text.push_str(&format!("<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>{}\n\t<key>ProgramArguments</key>\n\t<array>\n", string(LABEL)));
    for arg in [
        d.executable.to_string_lossy().as_ref(),
        "--home",
        d.manager_home.to_string_lossy().as_ref(),
        "serve",
    ] {
        text.push_str(&format!("\t\t{}\n", string(arg)));
    }
    text.push_str(&format!("\t</array>\n\t<key>RunAtLoad</key><true/>\n\t<key>KeepAlive</key>\n\t<dict><key>SuccessfulExit</key><false/></dict>\n\t<key>ThrottleInterval</key><integer>5</integer>\n\t<key>StandardOutPath</key>{}\n\t<key>StandardErrorPath</key>{}\n", string(&d.log_path.to_string_lossy()), string(&d.log_path.to_string_lossy())));
    // `serve` holds the supervision lease; the manager it spawns reads the
    // bridge configuration from here.
    let mut env = d.environment.clone();
    env.retain(|(key, _)| key != "AGENT_INTERCOM_CONFIG");
    env.push((
        "AGENT_INTERCOM_CONFIG".into(),
        d.config_path.to_string_lossy().into_owned(),
    ));
    let env = sanitized_environment(env);
    if !env.is_empty() {
        text.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (key, value) in env {
            text.push_str(&format!(
                "\t\t<key>{}</key>{}\n",
                xml_escape(&key),
                string(&value)
            ));
        }
        text.push_str("\t</dict>\n");
    }
    text.push_str("</dict>\n</plist>\n");
    text
}
fn sensitive_env_key(key: &str) -> bool {
    key.starts_with("AGENT_INTERCOM_") && key.to_uppercase().contains("TOKEN")
}
fn sanitized_environment(mut env: Vec<(String, String)>) -> Vec<(String, String)> {
    env.retain(|(key, _)| !key.is_empty() && !sensitive_env_key(key));
    env.sort_by(|(a, _), (b, _)| (a != "PATH", a).cmp(&(b != "PATH", b)));
    env
}
fn carried_environment(
    path: &str,
    source: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut values = BTreeMap::new();
    for (key, value) in source {
        if value.is_empty() || sensitive_env_key(&key) || key == "AGENT_INTERCOM_SOCK" {
            continue;
        }
        if key == "PATH"
            || key.starts_with("AGENT_INTERCOM_")
            || matches!(
                key.as_str(),
                "CODEX_HOME"
                    | "CODEX_APP_SERVER_SOCK"
                    | "CODEX_APP_SERVER_SOCKET"
                    | "CLAUDE_CONFIG_DIR"
                    | "CLAUDE_CODE_HARBOR_KITE"
                    | "HTTP_PROXY"
                    | "HTTPS_PROXY"
                    | "ALL_PROXY"
                    | "NO_PROXY"
                    | "http_proxy"
                    | "https_proxy"
                    | "all_proxy"
                    | "no_proxy"
            )
        {
            values.insert(key, value);
        }
    }
    values.insert("PATH".into(), path.into());
    sanitized_environment(values.into_iter().collect())
}
fn daemon_path(current: &str, login: &[PathBuf]) -> (String, BTreeMap<String, String>) {
    let mut paths = Vec::new();
    for path in std::env::split_paths(current).chain(login.iter().cloned()) {
        if !path.as_os_str().is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    }
    let mut tools = BTreeMap::new();
    for name in ["claude", "codex", "pi", "herdr"] {
        let found = look_path_in(name, &paths);
        if let Some(path) = &found {
            if let Some(dir) = path.parent() {
                if !paths.iter().any(|p| p == dir) {
                    paths.push(dir.to_owned());
                }
            }
        }
        tools.insert(
            name.into(),
            found
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "not found".into()),
        );
    }
    (
        std::env::join_paths(paths)
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        tools,
    )
}
fn look_path_in(name: &str, paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find_map(|dir| {
        let candidate = dir.join(name);
        let meta = std::fs::metadata(&candidate).ok()?;
        if meta.is_dir() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        absolute(&candidate).ok()
    })
}
async fn login_shell_path() -> Vec<PathBuf> {
    let Some(shell) = std::env::var_os("SHELL").filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    read_login_shell_path(&shell).await
}
async fn read_login_shell_path(shell: &std::ffi::OsStr) -> Vec<PathBuf> {
    use tokio::io::AsyncReadExt;
    let mut cmd = tokio::process::Command::new(shell);
    cmd.args(["-lic", "printf '\\n__AGENT_INTERCOM_PATH__%s\\n' \"$PATH\""])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else {
        return Vec::new();
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut out = Vec::new();
    let mut err = Vec::new();
    // Preserve partial output on timeout and bound inherited pipe waits as Go's WaitDelay does.
    {
        let read = async {
            let _ = tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
        };
        tokio::pin!(read);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        tokio::select! {
            _ = &mut read => {
                if tokio::time::timeout_at(deadline, child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
            }
            result = tokio::time::timeout_at(deadline, child.wait()) => {
                if result.is_err() { let _ = child.kill().await; }
                let _ = tokio::time::timeout(Duration::from_millis(250), &mut read).await;
            }
        }
    }
    out.extend(err);
    parse_login_shell_path(&String::from_utf8_lossy(&out))
}
fn parse_login_shell_path(text: &str) -> Vec<PathBuf> {
    text.rsplit_once("__AGENT_INTERCOM_PATH__")
        .map(|(_, rest)| std::env::split_paths(rest.lines().next().unwrap_or("").trim()).collect())
        .unwrap_or_default()
}

fn executable() -> Result<PathBuf, String> {
    let started = std::env::current_exe().map_err(|e| e.to_string())?;
    let real = std::fs::canonicalize(&started)
        .map_err(|e| format!("resolve executable {}: {e}", started.display()))?;
    pick_executable(
        &real,
        &started,
        &std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>(),
    )
}
fn pick_executable(real: &Path, started: &Path, paths: &[PathBuf]) -> Result<PathBuf, String> {
    for candidate in [real, started] {
        if candidate.to_string_lossy().contains("/AppTranslocation/") {
            return Err("this app is running from AppTranslocation; move it to Applications, then install the service".into());
        }
    }
    let real = match std::fs::canonicalize(real) {
        Ok(path) => path,
        Err(_) => absolute(real)?,
    };
    let started = absolute(started)?;
    if real.to_string_lossy().contains(".app/Contents/MacOS/") {
        return Ok(real);
    }
    if started.to_string_lossy().contains(".app/Contents/MacOS/") {
        return Ok(started);
    }
    if let Some(candidate) = look_path_in(BINARY_NAME, paths) {
        if same_file(&candidate, &real) {
            return Ok(candidate);
        }
    }
    Ok(real)
}
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::canonicalize(a)
            .ok()
            .is_some_and(|a| std::fs::canonicalize(b).ok() == Some(a))
    }
}

fn service_not_found(error: &str) -> bool {
    let error = error.to_lowercase();
    [
        "could not find service",
        "service not found",
        "no such process",
    ]
    .iter()
    .any(|part| error.contains(part))
}
fn retryable_bootstrap_error(error: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("bootstrap failed: 5:") || error.contains("bootstrap failed: 37:")
}
async fn launchctl(args: &[&str]) -> Result<String, String> {
    let output = tokio::process::Command::new("launchctl")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("launchctl {}: {e}", args.join(" ")))?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let text = text.trim().to_owned();
    if output.status.success() {
        return Ok(text);
    }
    Err(format!(
        "launchctl {}: {}",
        args.join(" "),
        if text.is_empty() {
            output.status.to_string()
        } else {
            text
        }
    ))
}
async fn loaded() -> Result<bool, String> {
    match launchctl(&["print", &launch_service()]).await {
        Ok(_) => Ok(true),
        Err(error) if service_not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}
async fn bootstrap(plist: &Path) -> Result<(), String> {
    for attempt in 0..10 {
        match launchctl(&["bootstrap", &launch_domain(), &plist.to_string_lossy()]).await {
            Ok(_) => return Ok(()),
            Err(error) if attempt < 9 && retryable_bootstrap_error(&error) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!()
}
async fn status() -> Result<Status, String> {
    let file = plist_path();
    let installed = match std::fs::metadata(&file) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.to_string()),
    };
    let mut status = Status {
        installed,
        file,
        log_path: log_path(),
        state: "not loaded".into(),
        ..Status::default()
    };
    match launchctl(&["print", &launch_service()]).await {
        Ok(output) => parse_status(&mut status, &output),
        Err(error) if service_not_found(&error) => {}
        Err(error) => return Err(error),
    }
    Ok(status)
}
fn parse_status(status: &mut Status, output: &str) {
    status.loaded = true;
    let mut depth = 0;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.ends_with('{') {
            depth += 1;
            continue;
        }
        if trimmed == "}" {
            depth -= 1;
            continue;
        }
        if depth != 1 || !line.starts_with('\t') || line.starts_with("\t\t") {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(" = ") {
            match key {
                "state" => status.state = value.into(),
                "pid" => {
                    if let Ok(pid) = value.parse() {
                        status.pid = pid;
                    }
                }
                _ => {}
            }
        }
    }
    status.running = status.state == "running" && status.pid > 0;
}
fn response_error(response: &Value) -> &str {
    response["error"].as_str().unwrap_or("")
}
fn report_pid(report: &Value) -> i64 {
    report["pid"].as_i64().unwrap_or(0)
}
/// The launch agent runs `serve`, which leases the manager: ready once the
/// agent is running and the manager reports a held lease.
fn service_ready(status: &Status, report: &Value) -> bool {
    status.loaded && status.pid > 0 && report_pid(report) > 0 && report["supervised"] == true
}
fn format_status(status: &Status, report: Option<&Value>, cli_sock: &Path, issue: &str) -> String {
    let state = if !status.loaded {
        "not loaded".into()
    } else if status.running {
        format!("running (pid {})", status.pid)
    } else {
        status.state.clone()
    };
    let mut text = format!(
        "Launch agent: {state}\nInstalled: {}\nPlist: {}\nLogs: {}\n",
        status.installed,
        status.file.display(),
        status.log_path.display()
    );
    if !issue.is_empty() {
        text.push_str(&format!("Daemon: {issue}\n"));
        return text;
    }
    let Some(report) = report.filter(|r| !r.is_null()) else {
        text.push_str("Daemon: no status report\n");
        return text;
    };
    let field = |key: &str| report[key].as_str().unwrap_or("");
    let pid = report_pid(report);
    let socket = field("socket");
    text.push_str(&format!(
        "Daemon: {} (pid {pid})\nRelay: {}\nSocket: {socket}\n",
        field("daemon"),
        field("relay")
    ));
    if !cli_sock.as_os_str().is_empty()
        && !socket.is_empty()
        && clean_path(cli_sock) != clean_path(Path::new(socket))
    {
        text.push_str(&format!(
            "Warning: CLI socket {} differs from daemon socket {socket}\n",
            cli_sock.display()
        ));
    }
    let supervised = report["supervised"] == true;
    if status.loaded && status.pid > 0 && !supervised {
        text.push_str(&format!(
            "Warning: manager pid {pid} is not supervised by the launch agent (pid {})\n",
            status.pid
        ));
    } else if !status.loaded && supervised {
        text.push_str(&format!(
            "Warning: manager pid {pid} is supervised outside the launch agent\n"
        ));
    }
    text
}
/// The service's `serve` leases whatever manager runs for `home`, so a
/// self-managed one is fine; refuse only when the intercom socket is served
/// by some other process (another home, or the standalone agent-intercom).
async fn reject_external_daemon(home: &Path, cfg: &Config) -> Result<(), String> {
    #[cfg(unix)]
    match tokio::net::UnixStream::connect(&cfg.sock).await {
        Ok(stream) => drop(stream),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(());
        }
        Err(e) => {
            return Err(format!(
                "inspect existing daemon at {}: {e}",
                cfg.sock.display()
            ))
        }
    }
    let response = super::localrpc::call_raw(cfg, json!({"op":"status"}))
        .await
        .map_err(|e| format!("inspect existing daemon at {}: {e}", cfg.sock.display()))?;
    let pid = response
        .get("report")
        .map(report_pid)
        .filter(|pid| *pid > 0);
    let manager = crate::lifecycle::read_pid_file(home).map(|file| i64::from(file.pid));
    if pid.is_some() && pid == manager {
        return Ok(());
    }
    Err(format!(
        "{} is served by another process{}, not the manager at {}; stop it first",
        cfg.sock.display(),
        pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default(),
        home.display()
    ))
}
async fn wait_ready(cfg: &Config) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    loop {
        let check = async {
            let response = super::localrpc::call_raw(cfg, json!({"op":"status"})).await?;
            if !response_error(&response).is_empty() {
                return Err(response_error(&response).to_owned());
            }
            let report = response
                .get("report")
                .filter(|r| !r.is_null())
                .ok_or("daemon returned an empty status report")?;
            let status = status().await?;
            if service_ready(&status, report) {
                Ok(())
            } else {
                Err(format!(
                    "manager pid {} is not yet supervised by launchd pid {}",
                    report_pid(report),
                    status.pid
                ))
            }
        };
        let error = match tokio::time::timeout_at(deadline, check).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => error,
            Err(_) => "daemon status request timed out".into(),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("launch agent was loaded, but its daemon did not become ready within 5s (socket {}): {error}", cfg.sock.display()));
        }
        tokio::time::sleep(READY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detected() -> Detected {
        Detected {
            executable: "/opt/homebrew/bin/pi-famulus".into(),
            manager_home: "/Users/test/manager & home".into(),
            config_path: "/Users/test/client.toml".into(),
            log_path: "/Users/test/Library/Logs/agent-intercom/client.log".into(),
            path: "/opt/homebrew/bin:/usr/bin".into(),
            token_source: "config",
            token_value: "secret-relay-token".into(),
            would_save: Vec::new(),
            tools: BTreeMap::new(),
            environment: vec![("PATH".into(), "/opt/homebrew/bin:/usr/bin".into())],
        }
    }
    fn temp_dir() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("famulus-service-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn external_manager_process() {
        let Some(home) = std::env::var_os("PI_FAMULUS_SERVICE_MANAGER_TEST_HOME") else {
            return;
        };
        assert_eq!(crate::daemon::run(PathBuf::from(home), true, None).await, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn installation_leases_this_homes_manager_but_rejects_a_foreign_socket_owner() {
        struct Manager {
            child: std::process::Child,
            root: PathBuf,
        }
        impl Drop for Manager {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
        let root = PathBuf::from("/tmp").join(format!("fsvc-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
        std::fs::create_dir(&root).unwrap();
        // The bridge refuses a group-writable socket directory (umask 002).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.join("manager");
        let sock = root.join("i.sock");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "intercom::service::tests::external_manager_process",
                "--nocapture",
            ])
            .env("PI_FAMULUS_SERVICE_MANAGER_TEST_HOME", &home)
            .env("HOME", &root)
            .env("AGENT_INTERCOM_CONFIG", root.join("absent.toml"))
            .env("AGENT_INTERCOM_SOCK", &sock)
            .env_remove(super::super::DISABLE_ENV)
            .env_remove("PI_FAMULUS_TEST_CLOCK")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut manager = Manager {
            child,
            root: root.clone(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::net::UnixStream::connect(&sock).await.is_err() {
            assert!(
                manager.child.try_wait().unwrap().is_none(),
                "real manager exited during setup"
            );
            assert!(
                tokio::time::Instant::now() < deadline,
                "real manager never bridged {}",
                sock.display()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let cfg = Config {
            sock: sock.clone(),
            ..Config::default()
        };
        let own = reject_external_daemon(&home, &cfg).await;
        assert!(own.is_ok(), "the service must lease this home's manager: {own:?}");
        let foreign = reject_external_daemon(&root.join("other-home"), &cfg).await;
        assert!(
            foreign.is_err_and(|error| error.contains("served by another process")),
            "the service must not adopt a socket another manager serves"
        );
        let missing = Config {
            sock: root.join("missing.sock"),
            ..Config::default()
        };
        assert!(reject_external_daemon(&home, &missing).await.is_ok());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn restart_stops_the_service_gracefully_instead_of_kickstart_kill() {
        const CHILD: &str = "PI_FAMULUS_SERVICE_RESTART_TEST_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let cfg = Config {
                sock: root.join("missing-intercom.sock"),
                local_only: true,
                ..Config::default()
            };
            let opts = Options {
                config: None,
                sock: None,
                state_dir: None,
                server: None,
                token: None,
                name: None,
                mcp_listen: None,
                mcp_token: None,
                remote_messages: None,
                local_only: false,
                insecure: false,
                save_token: false,
            };
            run(&root.join("manager"), &cfg, &opts, ServiceCommand::Restart)
                .await
                .unwrap();
            return;
        }
        let root = PathBuf::from("/tmp").join(format!("fsvc-{}", uuid::Uuid::new_v4()));
        let agents = root.join("Library/LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join(format!("{LABEL}.plist")), "<plist/>").unwrap();
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls = root.join("launchctl.log");
        let launchctl = bin.join("launchctl");
        std::fs::write(
            &launchctl,
            format!("#!/bin/sh\necho \"$*\" >> '{}'\n", calls.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launchctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "intercom::service::tests::restart_stops_the_service_gracefully_instead_of_kickstart_kill",
                "--nocapture",
            ])
            .env(CHILD, &root)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("HOME", &root)
            .env_remove("PI_FAMULUS_TEST_CLOCK")
            .output()
            .unwrap();
        let log = std::fs::read_to_string(&calls).unwrap_or_default();
        let plist = std::fs::read_to_string(agents.join(format!("{LABEL}.plist"))).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            output.status.success(),
            "restart failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            plist.contains("<string>serve</string>") && !plist.contains("<string>intercom</string>"),
            "restart kept a stale launch agent: {plist}"
        );
        let verbs: Vec<&str> = log
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|verb| *verb != "print")
            .collect();
        assert!(
            !log.contains("kickstart -k"),
            "restart killed the daemon instead of stopping it: {log}"
        );
        assert_eq!(verbs, ["bootout", "bootstrap"], "{log}");
    }

    #[test]
    fn plist_comments_never_contain_double_hyphens() {
        let mut d = detected();
        d.executable = PathBuf::from("/opt/a---b/--x-/pi-famulus-");
        d.path = "/bin:/x----y".into();
        let text = render_plist(&d);
        for comment in text.split("<!--").skip(1) {
            let body = comment.split("-->").next().unwrap();
            assert!(
                !body.contains("--") && !body.ends_with('-'),
                "invalid XML comment: {body:?}"
            );
        }
    }

    #[test]
    fn plist_preserves_launchd_contract_and_manager_arguments() {
        let mut d = detected();
        d.would_save = vec!["token to config", "local_only=true"];
        d.environment
            .push(("HTTP_PROXY".into(), "http://u:p&ss@proxy".into()));
        d.environment
            .push(("AGENT_INTERCOM_MCP_TOKEN".into(), "mcp-secret".into()));
        let text = render_with_token_check(&d, "secret-relay-token").unwrap();
        for expected in [
            "<string>--home</string>",
            "<string>/Users/test/manager &amp; home</string>",
            "<string>serve</string>",
            "<key>AGENT_INTERCOM_CONFIG</key>",
            "<key>SuccessfulExit</key><false/>",
            "<key>ThrottleInterval</key><integer>5</integer>",
            "<key>StandardOutPath</key>",
            "<key>StandardErrorPath</key>",
            "http://u:p&amp;ss@proxy",
            "would save token to config; would save local_only=true",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("secret-relay-token"));
        assert!(!text.contains("mcp-secret"));
        assert!(!text.contains("<string>--sock</string>"));
    }

    #[test]
    fn token_leak_detection_covers_saved_and_xml_escaped_values() {
        let mut d = detected();
        d.token_value = "tok&en<secret>".into();
        d.environment
            .push(("CODEX_HOME".into(), d.token_value.clone()));
        assert!(render_with_token_check(&d, "")
            .unwrap_err()
            .contains("would contain the relay token"));
        d.environment.pop();
        d.path = "flag-token".into();
        assert!(render_with_token_check(&d, "flag-token").is_err());
    }

    #[test]
    fn token_plans_match_config_and_save_token_precedence() {
        let mut values = toml::Table::new();
        let path = Path::new("/tmp/client.toml");
        assert!(token_plan(&values, "", false, false, path)
            .unwrap_err()
            .contains("token is required"));
        assert!(token_plan(&values, "env", false, false, path)
            .unwrap_err()
            .contains("--save-token"));
        assert_eq!(
            token_plan(&values, "env", false, true, path).unwrap(),
            (
                "would save to config",
                "env".into(),
                vec!["token to config"]
            )
        );
        assert_eq!(
            token_plan(&values, "env", true, false, path).unwrap(),
            (
                "ignored in local_only mode",
                "env".into(),
                vec!["local_only=true"]
            )
        );
        values.insert("token".into(), toml::Value::String("file".into()));
        assert_eq!(
            token_plan(&values, "env", false, false, path).unwrap().1,
            "file"
        );
        assert_eq!(
            token_plan(&values, "new", false, true, path).unwrap().1,
            "new"
        );
        values.insert("local_only".into(), toml::Value::Boolean(true));
        assert!(token_plan(&values, "file", true, true, path)
            .unwrap()
            .2
            .is_empty());
    }

    #[test]
    fn carried_environment_filters_secrets_socket_and_unrelated_values() {
        let source = [
            ("PATH", "/old"),
            ("AGENT_INTERCOM_TOKEN", "secret"),
            ("AGENT_INTERCOM_MCP_TOKEN", "secret"),
            ("AGENT_INTERCOM_someToken", "secret"),
            ("AGENT_INTERCOM_SOCK", "/wrong/socket"),
            ("AGENT_INTERCOM_SERVER", "relay"),
            ("CODEX_HOME", "/codex"),
            ("CLAUDE_CONFIG_DIR", "/claude"),
            ("CLAUDE_CODE_HARBOR_KITE", "1"),
            ("http_proxy", "http://proxy"),
            ("HTTPS_PROXY", "http://proxy"),
            ("UNRELATED_SECRET", "secret"),
            ("AGENT_INTERCOM_EMPTY", ""),
        ];
        let env = carried_environment("/new", source.map(|(k, v)| (k.into(), v.into())));
        assert_eq!(env[0], ("PATH".into(), "/new".into()));
        assert_eq!(env.len(), 7);
        assert!(!env
            .iter()
            .any(|(k, _)| k.contains("TOKEN") || k == "AGENT_INTERCOM_SOCK"));
        assert_eq!(env[1].0, "AGENT_INTERCOM_SERVER");
    }

    #[cfg(unix)]
    #[test]
    fn socket_validation_rejects_relative_and_mismatched_paths() {
        let config = Path::new("/tmp/client.toml");
        assert!(validate_socket(
            Path::new("relative.sock"),
            Path::new("relative.sock"),
            config
        )
        .unwrap_err()
        .contains("relative"));
        assert!(
            validate_socket(Path::new("/one"), Path::new("/two"), config)
                .unwrap_err()
                .contains("service will use socket")
        );
        validate_socket(Path::new("/one/../two"), Path::new("/two"), config).unwrap();
        assert_eq!(
            launchd_socket(&toml::Table::new()),
            std::env::temp_dir().join(format!("agent-intercom-{}/client.sock", uid()))
        );
    }

    #[test]
    fn config_updates_preserve_unknown_values_and_noop_is_read_only() {
        let dir = temp_dir();
        let path = dir.join("client.toml");
        let initial = "# preserved on no-op\nserver = 'https://relay'\ntoken = 'old'\n[future]\nenabled = true\n";
        std::fs::write(&path, initial).unwrap();
        persist_install_config(&path, "ignored", false, false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), initial);
        persist_install_config(&path, "new", true, true).unwrap();
        let values = read_config(&path).unwrap();
        assert_eq!(file_token(&values), "new");
        assert!(file_local_only(&values));
        assert_eq!(values["future"]["enabled"].as_bool(), Some(true));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn config_updates_follow_symlinks_and_preserve_private_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = temp_dir();
        let target = dir.join("actual.toml");
        let link = dir.join("client.toml");
        std::fs::write(&target, "token = 'old'\n").unwrap();
        symlink(&target, &link).unwrap();
        persist_install_config(&link, "new", false, true).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(file_token(&read_config(&target).unwrap()), "new");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn launchctl_status_ignores_nested_coalition_state_and_pid() {
        let mut st = Status::default();
        parse_status(&mut st, "gui/501/com.agent-intercom.client = {\n\tstate = running\n\tpid = 123\n\tcoalitions = {\n\t\tstate = waiting\n\t\tpid = 999\n\t}\n}\n");
        assert!(st.loaded && st.running);
        assert_eq!(st.pid, 123);
        assert_eq!(st.state, "running");
        assert!(service_ready(&st, &json!({"pid":456,"supervised":true})));
        assert!(!service_ready(&st, &json!({"pid":456,"supervised":false})));
        assert!(!service_ready(&st, &json!({"supervised":true})));
        st.loaded = false;
        assert!(!service_ready(&st, &json!({"pid":456,"supervised":true})));
    }

    #[test]
    fn status_output_matches_daemon_and_mismatch_warnings() {
        let mut st = Status {
            installed: true,
            loaded: true,
            running: true,
            pid: 123,
            file: "/agent.plist".into(),
            log_path: "/client.log".into(),
            ..Status::default()
        };
        let report = json!({"daemon":"running", "relay":"connected", "pid":456, "socket":"/other.sock", "supervised":false});
        let text = format_status(&st, Some(&report), Path::new("/cli.sock"), "");
        assert!(text.starts_with("Launch agent: running (pid 123)\nInstalled: true\nPlist: /agent.plist\nLogs: /client.log\n"));
        assert!(text.contains("Daemon: running (pid 456)\nRelay: connected\nSocket: /other.sock\n"));
        assert!(
            text.contains("Warning: CLI socket /cli.sock differs from daemon socket /other.sock")
        );
        assert!(text.contains(
            "Warning: manager pid 456 is not supervised by the launch agent (pid 123)"
        ));
        let leased = json!({"daemon":"running", "pid":456, "socket":"/cli.sock", "supervised":true});
        assert!(!format_status(&st, Some(&leased), Path::new("/cli.sock"), "").contains("Warning"));
        st.loaded = false;
        assert!(format_status(&st, Some(&leased), Path::new(""), "")
            .contains("is supervised outside the launch agent"));
        assert!(format_status(&st, None, Path::new(""), "").ends_with("Daemon: no status report\n"));
        assert!(
            format_status(&st, None, Path::new(""), "not responding (offline)")
                .ends_with("Daemon: not responding (offline)\n")
        );
    }

    #[test]
    fn launchctl_error_classification_is_narrow() {
        for error in [
            "Could not find service",
            "Service not found",
            "No such process",
        ] {
            assert!(service_not_found(error));
        }
        assert!(!service_not_found("launchctl: No such file or directory"));
        for error in [
            "Bootstrap failed: 5: Input/output error",
            "Bootstrap failed: 37: Operation already in progress",
        ] {
            assert!(retryable_bootstrap_error(error));
        }
        assert!(!retryable_bootstrap_error(
            "Bootstrap failed: 1: permission denied"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn shell_path_marker_uses_last_marker_and_ignores_shell_noise() {
        assert_eq!(parse_login_shell_path("noise\n__AGENT_INTERCOM_PATH__/old\nnoise\n__AGENT_INTERCOM_PATH__/bin:/opt/bin\nbye"), vec![PathBuf::from("/bin"), PathBuf::from("/opt/bin")]);
        assert!(parse_login_shell_path("no marker").is_empty());
        assert_eq!(comment_value("x--y\nz&\""), "x- -y z&amp;&#34;");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn login_shell_timeout_keeps_already_emitted_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let shell = dir.join("shell");
        std::fs::write(&shell, "#!/bin/sh\nprintf '\\n__AGENT_INTERCOM_PATH__/from/login:/bin\\n'\nexec /bin/sleep 5\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            read_login_shell_path(shell.as_os_str()).await,
            vec![PathBuf::from("/from/login"), PathBuf::from("/bin")]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn path_merges_deduplicates_and_finds_executable_tools() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let codex = dir.join("codex");
        std::fs::write(&codex, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (path, tools) = daemon_path("/usr/bin:/usr/bin", &[dir.clone(), dir.clone()]);
        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            vec![PathBuf::from("/usr/bin"), dir.clone()]
        );
        assert_eq!(tools["codex"], codex.display().to_string());
        assert!(look_path_in("missing-tool", std::slice::from_ref(&dir)).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn executable_prefers_stable_path_link_but_keeps_app_bundle() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = temp_dir();
        let real = dir.join("versioned-binary");
        let link = dir.join(BINARY_NAME);
        std::fs::write(&real, "binary").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&real, &link).unwrap();
        assert_eq!(
            pick_executable(&real, &real, std::slice::from_ref(&dir)).unwrap(),
            link
        );
        assert!(pick_executable(
            Path::new("/private/AppTranslocation/abc/pi-famulus"),
            &real,
            &[]
        )
        .unwrap_err()
        .contains("AppTranslocation"));
        let app = Path::new("/Applications/Famulus.app/Contents/MacOS/pi-famulus");
        assert_eq!(pick_executable(app, app, &[]).unwrap(), app);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
