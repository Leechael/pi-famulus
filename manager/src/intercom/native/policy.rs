use super::*;
use std::{
    io::Read,
    path::{Path, PathBuf},
};

const LIMIT: u64 = 2 << 20;
fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(format!(
                "cross-machine message refused: cannot read settings {}: {e}",
                path.display()
            ));
        }
    };
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > LIMIT {
        return Err("cross-machine message refused: source exceeds policy size limit".into());
    }
    Ok(Some(bytes))
}
fn check_json(bytes: &[u8], path: &Path) -> Result<(), String> {
    let settings: Value = serde_json::from_slice(bytes).map_err(|e| {
        format!(
            "cross-machine message refused: malformed settings {}: {e}",
            path.display()
        )
    })?;
    let settings = settings.as_object().ok_or_else(|| {
        format!(
            "cross-machine message refused: malformed settings object in {}",
            path.display()
        )
    })?;
    for key in ["policyHelper", "policyHelpers"] {
        if settings.get(key).is_some_and(|v| !v.is_null()) {
            return Err(format!(
                "cross-machine message refused: {} configures {key}; effective managed policy cannot be verified",
                path.display()
            ));
        }
    }
    if let Some(value) = settings.get("isolatePeerMachines") {
        if value.as_bool() != Some(false) {
            return Err(format!(
                "cross-machine message refused: isolatePeerMachines in {}; native cross-machine approval unavailable",
                path.display()
            ));
        }
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn process(pid: u32) -> Result<(Vec<String>, HashMap<String, String>), String> {
    let args = read(&PathBuf::from(format!("/proc/{pid}/cmdline")))?
        .ok_or("source process disappeared")?;
    let env = read(&PathBuf::from(format!("/proc/{pid}/environ")))?
        .ok_or("source process environment unavailable")?;
    Ok((
        args.split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect(),
        env.split(|b| *b == 0)
            .filter_map(|v| {
                let s = String::from_utf8_lossy(v);
                s.split_once('=').map(|(k, v)| (k.into(), v.into()))
            })
            .collect(),
    ))
}
#[cfg(target_os = "macos")]
fn process(pid: u32) -> Result<(Vec<String>, HashMap<String, String>), String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut length = 0usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || length > LIMIT as usize
    {
        return Err("cannot inspect source process launch settings".into());
    }
    let mut data = vec![0u8; length];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            data.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || length < 4
    {
        return Err("cannot inspect source process launch settings".into());
    }
    data.truncate(length);
    let argc = i32::from_ne_bytes(data[..4].try_into().unwrap());
    if argc <= 0 {
        return Err("source process has no arguments".into());
    }
    let mut offset = 4;
    while offset < data.len() && data[offset] != 0 {
        offset += 1;
    }
    while offset < data.len() && data[offset] == 0 {
        offset += 1;
    }
    let mut parts = data[offset..].split(|b| *b == 0);
    let args = parts
        .by_ref()
        .take(argc as usize)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    let env = parts
        .filter_map(|s| {
            let s = String::from_utf8_lossy(s);
            s.split_once('=').map(|(k, v)| (k.into(), v.into()))
        })
        .collect();
    Ok((args, env))
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn process(_pid: u32) -> Result<(Vec<String>, HashMap<String, String>), String> {
    Err("source process inspection supports Linux/macOS only".into())
}
fn worktree(dir: &Path) -> Result<Option<PathBuf>, String> {
    let git = dir.join(".git");
    match std::fs::metadata(&git) {
        Ok(m) if m.is_dir() => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot inspect project metadata: {e}")),
        _ => {}
    }
    let raw = read(&git)?.unwrap_or_default();
    let raw = String::from_utf8_lossy(&raw);
    let gitdir = raw
        .trim()
        .strip_prefix("gitdir: ")
        .ok_or("unrecognized project metadata")?;
    let gitdir = dir.join(gitdir);
    let Some(common) = read(&gitdir.join("commondir"))? else {
        return Ok(None);
    };
    let common = String::from_utf8_lossy(&common);
    if common.trim().is_empty() {
        return Err("empty worktree commondir".into());
    }
    let common = std::fs::canonicalize(gitdir.join(common.trim())).map_err(|e| e.to_string())?;
    Ok(common
        .parent()
        .map(|p| p.join(".claude/settings.local.json")))
}
/// Detect settings channels a local file scan cannot verify. Returns a
/// human-readable channel description when the source may receive
/// server-managed settings (claude.ai / apps gateway).
fn server_managed_channel(
    config: &Path,
    env: &HashMap<String, String>,
) -> Result<Option<&'static str>, String> {
    if env
        .get("CLAUDE_CODE_OAUTH_TOKEN")
        .is_some_and(|value| !value.is_empty())
    {
        return Ok(Some("CLAUDE_CODE_OAUTH_TOKEN"));
    }
    let Some(raw) = read(&config.join(".credentials.json"))? else {
        return Ok(None);
    };
    let credentials: Value = serde_json::from_slice(&raw).map_err(|e| {
        format!("cross-machine message refused: cannot parse Claude credentials to rule out claude.ai managed settings: {e}")
    })?;
    if ["accessToken", "refreshToken"].iter().any(|field| {
        credentials["claudeAiOauth"][field]
            .as_str()
            .is_some_and(|token| !token.is_empty())
    }) {
        return Ok(Some(
            "claude.ai OAuth (claudeAiOauth credentials); claude.ai and the apps gateway push \
             managed settings to signed-in clients",
        ));
    }
    Ok(None)
}
pub(super) fn check(row: &Value) -> Result<(), String> {
    let path = PathBuf::from(str_field(row, "_registry"));
    let current = super::read_claude_entry(&path).map_err(|e| {
        format!("cross-machine message refused: Claude source registry unavailable: {e}")
    })?;
    if current["sessionId"] != str_field(row, "id").trim_start_matches("claude/")
        || current["pid"] != row["pid"]
        || current["procStart"] != row["_proc_start"]
        || current["messagingSocketPath"] != row["_socket"]
    {
        return Err(
            "cross-machine message refused: Claude source registry changed; refresh listing".into(),
        );
    }
    if !str_field(&current, "hostSessionId").is_empty() {
        return Err(
            "cross-machine message refused: hostSessionId policy cannot be verified".into(),
        );
    }
    let pid = current["pid"].as_u64().unwrap_or(0) as u32;
    let start = str_field(&current, "procStart");
    let cwd = PathBuf::from(str_field(&current, "cwd"));
    if pid == 0
        || start.is_empty()
        || start != alias::process_start(pid)
        || !cwd.is_absolute()
        || str_field(&current, "messagingSocketPath").is_empty()
    {
        return Err("cross-machine message refused: incomplete Claude source identity".into());
    }
    if !["", "cli"].contains(&str_field(&current, "entrypoint"))
        || !["interactive", "bg", "daemon", "daemon-worker"].contains(&str_field(&current, "kind"))
    {
        return Err("cross-machine message refused: unverified hosted Claude source".into());
    }
    let (args, env) = process(pid).map_err(|e| format!("cross-machine message refused: {e}"))?;
    if args.is_empty() {
        return Err("cross-machine message refused: source launch settings unavailable".into());
    }
    if env
        .get("CLAUDE_CODE_ENTRYPOINT")
        .is_some_and(|v| !v.is_empty() && v != "cli")
    {
        return Err("cross-machine message refused: unverified source entrypoint".into());
    }
    for arg in args.iter().skip(1) {
        if [
            "--settings",
            "--setting-sources",
            "--managed-settings",
            "--sdk-url",
            "--bare",
            "--restricted",
        ]
        .contains(&arg.split('=').next().unwrap_or(""))
        {
            return Err(format!(
                "cross-machine message refused: source uses {arg}; effective settings cannot be verified"
            ));
        }
    }
    let home = PathBuf::from(
        env.get("HOME")
            .ok_or("cross-machine message refused: cannot determine source HOME")?,
    );
    if !home.is_absolute() {
        return Err("cross-machine message refused: source HOME is relative".into());
    }
    let config = env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    if !config.is_absolute() {
        return Err("cross-machine message refused: source CLAUDE_CONFIG_DIR is relative".into());
    }
    // Fail closed on server-managed settings channels. claude.ai / the apps
    // gateway settle managed settings with higher precedence than every local
    // file scanned below, deliver them to OAuth-signed-in clients at runtime,
    // and have no local effective-settings interface this scan can query. A
    // source signed in via claude.ai therefore cannot be verified, so native
    // cross-machine forwarding stays disabled for it (local, non-cross-machine
    // behavior is unaffected — this check only runs for cross-machine sends).
    if let Some(channel) = server_managed_channel(&config, &env)? {
        return Err(format!(
            "cross-machine message refused: the source Claude session is signed in via \
             {channel}, so claude.ai / apps-gateway managed settings may apply with higher \
             precedence than local settings files, and no local effective-settings interface \
             exists to verify them. Native cross-machine forwarding is disabled for this \
             source: use an ANTHROPIC_API_KEY login for cross-machine Claude sources, or send \
             from a non-Claude session."
        ));
    }
    let mut paths = vec![config.join("settings.json")];
    for dir in cwd.ancestors() {
        paths.push(dir.join(".claude/settings.json"));
        paths.push(dir.join(".claude/settings.local.json"));
        if let Some(path) = worktree(dir)? {
            paths.push(path);
        }
    }
    #[cfg(target_os = "macos")]
    let managed = PathBuf::from("/Library/Application Support/ClaudeCode");
    #[cfg(not(target_os = "macos"))]
    let managed = PathBuf::from("/etc/claude-code");
    paths.push(managed.join("managed-settings.json"));
    match std::fs::read_dir(managed.join("managed-settings.d")) {
        Ok(entries) => {
            for entry in entries {
                let path = entry.map_err(|e| e.to_string())?.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    paths.push(path);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read managed settings directory: {e}")),
    }
    for path in paths {
        if let Some(raw) = read(&path)? {
            check_json(&raw, &path)?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        let username = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .map_err(|e| e.to_string())?;
        let username = String::from_utf8_lossy(&username.stdout).trim().to_string();
        for path in [
            PathBuf::from("/Library/Managed Preferences/com.anthropic.claudecode.plist"),
            PathBuf::from("/Library/Managed Preferences")
                .join(username)
                .join("com.anthropic.claudecode.plist"),
        ] {
            if read(&path)?.is_none() {
                continue;
            }
            let output = std::process::Command::new("/usr/bin/plutil")
                .args(["-convert", "json", "-o", "-", "--"])
                .arg(&path)
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() || output.stdout.len() as u64 > LIMIT {
                return Err("cannot parse managed plist".into());
            }
            check_json(&output.stdout, &path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_ai_oauth_credentials_fail_closed_and_api_key_sources_verify() {
        let dir = std::env::temp_dir().join(format!("pi-policy-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir(&dir).unwrap();
        // No credentials at all: only local files decide.
        assert_eq!(server_managed_channel(&dir, &HashMap::new()).unwrap(), None);
        // API-key style credentials carry no claude.ai session.
        std::fs::write(
            dir.join(".credentials.json"),
            br#"{"apiKey":"sk-ant-fixture"}"#,
        )
        .unwrap();
        assert_eq!(server_managed_channel(&dir, &HashMap::new()).unwrap(), None);
        // claude.ai OAuth: server-managed settings are invisible to local scans.
        std::fs::write(
            dir.join(".credentials.json"),
            br#"{"claudeAiOauth":{"accessToken":"secret"}}"#,
        )
        .unwrap();
        let channel = server_managed_channel(&dir, &HashMap::new()).unwrap();
        assert!(
            channel.is_some_and(|channel| channel.contains("claude.ai")),
            "expected the claude.ai channel: {channel:?}"
        );
        // Refresh-only credentials still represent an OAuth session: the
        // client can obtain a fresh access token and managed settings later.
        std::fs::write(
            dir.join(".credentials.json"),
            br#"{"claudeAiOauth":{"refreshToken":"secret-refresh"}}"#,
        )
        .unwrap();
        assert!(server_managed_channel(&dir, &HashMap::new())
            .unwrap()
            .is_some());
        // An injected OAuth token environment is equally unverifiable.
        std::fs::remove_file(dir.join(".credentials.json")).unwrap();
        let env = HashMap::from([("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "token".to_string())]);
        assert!(server_managed_channel(&dir, &env).unwrap().is_some());
        // Unparseable credentials can never authorize: fail closed, not Ok.
        std::fs::write(dir.join(".credentials.json"), b"not json").unwrap();
        assert!(server_managed_channel(&dir, &HashMap::new()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
