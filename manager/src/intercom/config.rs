use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub enabled: bool,
    pub server: String,
    pub token: String,
    pub name: String,
    pub tls_insecure: bool,
    pub sock: PathBuf,
    pub state_dir: PathBuf,
    pub mcp_listen: String,
    pub mcp_token: String,
    pub mcp_allow_non_loopback: bool,
    pub remote_messages: String,
    pub local_only: bool,
    pub event_log_size: usize,
    pub endpoints: Vec<Endpoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub secret: String,
    #[serde(default = "hmac_auth")]
    pub auth: String,
}
fn hmac_auth() -> String {
    "hmac".into()
}

impl Default for Config {
    fn default() -> Self {
        let home = home();
        let sock = std::env::var_os("XDG_RUNTIME_DIR")
            .map(|v| PathBuf::from(v).join("pi-famulus/client.sock"))
            .unwrap_or_else(|| {
                #[cfg(unix)]
                let uid = unsafe { libc::getuid() };
                #[cfg(not(unix))]
                let uid = 0;
                std::env::temp_dir().join(format!("pi-famulus-{uid}/client.sock"))
            });
        Self {
            enabled: false,
            server: "https://127.0.0.1:8443".into(),
            token: String::new(),
            name: String::new(),
            tls_insecure: false,
            sock,
            state_dir: home.join(".pi/agent/pi-famulus/messaging"),
            mcp_listen: String::new(),
            mcp_token: String::new(),
            mcp_allow_non_loopback: false,
            remote_messages: "allow".into(),
            local_only: false,
            event_log_size: 4096,
            endpoints: Vec::new(),
        }
    }
}

pub fn loopback_listen(listen: &str) -> bool {
    if let Ok(address) = listen.parse::<std::net::SocketAddr>() {
        return address.ip().is_loopback();
    }
    listen
        .rsplit_once(':')
        .is_some_and(|(host, _)| host.eq_ignore_ascii_case("localhost"))
}
/// A webhook secret (bearer token or HMAC key) may only travel over TLS,
/// except to this host's own loopback listeners.
pub fn webhook_url_allowed(url: &str, secret: &str) -> Result<(), String> {
    if secret.is_empty() {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url).map_err(|error| format!("invalid url: {error}"))?;
    let loopback = parsed.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if parsed.scheme() == "https" || loopback {
        Ok(())
    } else {
        Err(format!(
            "refusing to send a webhook secret to non-https URL {url}; use https:// or a loopback host"
        ))
    }
}
impl Config {
    /// The configured device name, or the hostname when none is set.
    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            hostname()
        } else {
            self.name.clone()
        }
    }
}
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 256];
        if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len()) } == 0 {
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            return String::from_utf8_lossy(&bytes[..end]).into_owned();
        }
    }
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into())
}
pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}
pub fn unified_path(home: &Path) -> PathBuf {
    home.join("config.json")
}
fn read_json(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("parse {}: {e}", path.display()))?;
            value
                .as_object()
                .cloned()
                .ok_or_else(|| format!("{} must contain a JSON object", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::Map::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
/// Atomically persist the shared JSON file without discarding unrelated
/// Famulus settings. Configuration may contain relay and MCP bearer tokens.
pub fn write_json(
    path: &Path,
    values: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), String> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let dir = target.parent().ok_or("config file parent required")?;
    private_dir(dir)?;
    let tmp = dir.join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let mut bytes = serde_json::to_vec_pretty(values).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    private_write(&tmp, &bytes)?;
    if let Err(error) = std::fs::rename(&tmp, &target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {error}", target.display()));
    }
    Ok(())
}

impl Config {
    pub fn load(home: &Path, opts: &super::cli::Options) -> Result<Self, String> {
        let path = unified_path(home);
        let values = read_json(&path)?;
        let mut c = if let Some(messaging) = values.get("messaging") {
            serde_json::from_value(messaging.clone())
                .map_err(|e| format!("parse {} messaging: {e}", path.display()))?
        } else {
            Self::default()
        };
        if c.state_dir == Self::default().state_dir {
            c.state_dir = home.join("messaging");
        }
        for (v, target) in [
            (&opts.server, &mut c.server),
            (&opts.token, &mut c.token),
            (&opts.name, &mut c.name),
            (&opts.mcp_listen, &mut c.mcp_listen),
            (&opts.mcp_token, &mut c.mcp_token),
            (&opts.remote_messages, &mut c.remote_messages),
        ] {
            if let Some(v) = v {
                *target = v.clone();
            }
        }
        if let Some(v) = &opts.sock {
            c.sock = v.clone();
        }
        if let Some(v) = &opts.state_dir {
            c.state_dir = v.clone();
        }
        c.local_only |= opts.local_only;
        c.tls_insecure |= opts.insecure;
        if c.sock.is_relative() || c.state_dir.is_relative() {
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            if c.sock.is_relative() {
                c.sock = cwd.join(&c.sock);
            }
            if c.state_dir.is_relative() {
                c.state_dir = cwd.join(&c.state_dir);
            }
        }
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<(), String> {
        if !["allow", "refuse"].contains(&self.remote_messages.as_str()) {
            return Err("remote_messages must be allow or refuse".into());
        }
        if self.sock.as_os_str().is_empty() || self.state_dir.as_os_str().is_empty() {
            return Err("socket and state directory must not be empty".into());
        }
        if !self.mcp_listen.is_empty() && !loopback_listen(&self.mcp_listen) {
            if !self.mcp_allow_non_loopback {
                return Err(format!(
                    "mcp_listen {:?} is not a loopback address; MCP is plain HTTP, so its bearer token \
                     and every message would cross the network unencrypted. Set \
                     mcp_allow_non_loopback = true only on a trusted network or behind a TLS proxy",
                    self.mcp_listen
                ));
            }
            if self.mcp_token.trim().is_empty() {
                return Err(format!(
                    "mcp_listen {:?} is not a loopback address; set mcp_token to expose MCP beyond this host",
                    self.mcp_listen
                ));
            }
        }
        for endpoint in &self.endpoints {
            if !crate::daemon::valid_session_id(&endpoint.name) {
                return Err(format!(
                    "endpoint name {:?} must be 1-128 letters, digits, '-', '_' or '.'",
                    endpoint.name
                ));
            }
            webhook_url_allowed(&endpoint.url, &endpoint.secret)
                .map_err(|error| format!("endpoint {}: {error}", endpoint.name))?;
        }
        Ok(())
    }
    pub fn http(&self) -> Result<reqwest::Client, String> {
        reqwest::Client::builder()
            .danger_accept_invalid_certs(self.tls_insecure)
            .min_tls_version(reqwest::tls::Version::TLS_1_3)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Identity {
    pub device_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credential: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server: String,
}
impl Identity {
    pub fn load(dir: &Path) -> Result<Self, String> {
        private_dir(dir)?;
        let path = dir.join("device.json");
        if !path.exists() {
            let v = Self {
                device_id: uuid::Uuid::new_v4().to_string(),
                ..Self::default()
            };
            let tmp = dir.join(format!(".device-{}", uuid::Uuid::new_v4()));
            private_write(&tmp, &serde_json::to_vec(&v).map_err(|e| e.to_string())?)?;
            let result = std::fs::hard_link(&tmp, &path);
            let _ = std::fs::remove_file(&tmp);
            if let Err(e) = result {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(e.to_string());
                }
            }
        }
        let v: Self = serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        uuid::Uuid::parse_str(&v.device_id).map_err(|_| "invalid device_id".to_string())?;
        Ok(v)
    }
    pub fn save(&self, dir: &Path) -> Result<(), String> {
        private_dir(dir)?;
        let tmp = dir.join(format!(".device-{}", uuid::Uuid::new_v4()));
        private_write(&tmp, &serde_json::to_vec(self).map_err(|e| e.to_string())?)?;
        std::fs::rename(&tmp, dir.join("device.json")).map_err(|e| e.to_string())
    }
}
pub const MCP_TOKEN_FILE: &str = "mcp-token";
/// The MCP bearer token used when none is configured: generated once, kept
/// owner-only in the state directory, and reused across restarts so client
/// registrations stay valid.
pub fn generated_mcp_token(dir: &Path) -> Result<String, String> {
    private_dir(dir)?;
    let path = dir.join(MCP_TOKEN_FILE);
    if !path.exists() {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let tmp = dir.join(format!(".mcp-token-{}", uuid::Uuid::new_v4()));
        private_write(&tmp, token.as_bytes())?;
        let result = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        if let Err(e) = result {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.to_string());
            }
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !m.is_file() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 != 0 {
            return Err(format!(
                "{} must be a regular file owned by this user with mode 0600",
                path.display()
            ));
        }
    }
    let token = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let token = token.trim();
    if token.len() < 32 {
        return Err(format!("{} holds no usable token", path.display()));
    }
    Ok(token.into())
}
pub fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(|error| error.to_string())
}
fn private_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .and_then(|mut f| f.write_all(bytes))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_config_keys_are_rejected_instead_of_ignored() {
        let error = toml::from_str::<Config>("mcp_tokne = 'typo'\n").unwrap_err();
        assert!(error.to_string().contains("mcp_tokne"), "{error}");
        let error = toml::from_str::<Config>(
            "[[endpoints]]\nname = 'hook'\nurl = 'https://x'\nsecrte = 's'\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("secrte"), "{error}");
        let parsed: Config = toml::from_str(
            "localOnly = true\nmcpListen = ''\n[[endpoints]]\nname = 'hook'\nurl = 'https://x'\n",
        )
        .unwrap();
        assert_eq!(parsed.endpoints[0].auth, "hmac");
        let wire = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(
            serde_json::from_value::<Config>(wire).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn an_unset_name_displays_as_the_hostname() {
        let mut config = Config::default();
        assert_eq!(config.display_name(), hostname());
        assert!(!config.display_name().is_empty());
        config.name = "workstation".into();
        assert_eq!(config.display_name(), "workstation");
    }

    #[test]
    fn messaging_is_loaded_from_the_shared_json_file() {
        let home = std::env::temp_dir().join(format!("famulus-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.json"),
            "{\"foregroundBudgetMs\":1234,\"messaging\":{\"name\":\"workstation\",\"localOnly\":true,\"mcpListen\":\"127.0.0.1:9\",\"endpoints\":[{\"name\":\"hook\",\"url\":\"https://example.test/hook\"}]}}\n",
        )
        .unwrap();
        let loaded = Config::load(&home, &super::super::cli::Options::without_overrides()).unwrap();
        assert_eq!(loaded.name, "workstation");
        assert!(loaded.local_only);
        assert_eq!(loaded.mcp_listen, "127.0.0.1:9");
        assert_eq!(loaded.endpoints.len(), 1);
        assert_eq!(loaded.state_dir, home.join("messaging"));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn non_loopback_mcp_listen_requires_an_explicit_opt_in_and_a_token() {
        let config = |listen: &str, token: &str, allow: bool| Config {
            mcp_listen: listen.into(),
            mcp_token: token.into(),
            mcp_allow_non_loopback: allow,
            ..Config::default()
        };
        for listen in [
            "0.0.0.0:18790",
            "[::]:18790",
            "192.0.2.1:0",
            "example.com:80",
        ] {
            let error = config(listen, "secret", false).validate().unwrap_err();
            assert!(error.contains("mcp_allow_non_loopback"), "{listen}: {error}");
            assert!(error.contains("unencrypted"), "{listen}: {error}");
            let error = config(listen, "", true).validate().unwrap_err();
            assert!(error.contains("mcp_token"), "{listen}: {error}");
            assert!(config(listen, "  ", true).validate().is_err(), "{listen}");
            assert!(config(listen, "secret", true).validate().is_ok(), "{listen}");
        }
        for listen in [
            "",
            "127.0.0.1:0",
            "127.1.2.3:9",
            "[::1]:18790",
            "localhost:18790",
        ] {
            assert!(config(listen, "", false).validate().is_ok(), "{listen}");
        }
        let parsed: Config = toml::from_str("mcpAllowNonLoopback = true\n").unwrap();
        assert!(parsed.mcp_allow_non_loopback);
    }

    #[test]
    fn webhook_secrets_require_https_except_on_loopback() {
        let config = |url: &str, secret: &str| Config {
            endpoints: vec![Endpoint {
                name: "hook".into(),
                url: url.into(),
                secret: secret.into(),
                auth: "bearer".into(),
            }],
            ..Config::default()
        };
        for url in ["http://hooks.example.com/x", "http://192.0.2.1/x", "ftp://x/y"] {
            let error = config(url, "s").validate().unwrap_err();
            assert!(error.contains("non-https"), "{url}: {error}");
            assert!(config(url, "").validate().is_ok(), "{url} without a secret");
        }
        for url in [
            "https://hooks.example.com/x",
            "http://127.0.0.1:9/x",
            "http://127.9.9.9/x",
            "http://[::1]:9/x",
            "http://LOCALHOST:9/x",
        ] {
            assert!(config(url, "s").validate().is_ok(), "{url}");
        }
        assert!(webhook_url_allowed("http://localhost.evil.test/", "s").is_err());
    }

    #[test]
    fn endpoint_names_must_be_valid_reply_origins() {
        let config = |name: &str| Config {
            endpoints: vec![Endpoint {
                name: name.into(),
                url: "https://hooks.example.com/x".into(),
                secret: String::new(),
                auth: "bearer".into(),
            }],
            ..Config::default()
        };
        for name in ["my hook", "a/b", "", "hook\n"] {
            assert!(config(name).validate().is_err(), "{name:?}");
        }
        assert!(config("hook-1.prod_x").validate().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn generated_mcp_token_is_private_random_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("intercom-mcp-token-{}", uuid::Uuid::new_v4()));
        let dir = root.join("state");
        let first = generated_mcp_token(&dir).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(generated_mcp_token(&dir).unwrap(), first);
        let path = dir.join(MCP_TOKEN_FILE);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let other = generated_mcp_token(&root.join("other")).unwrap();
        assert_ne!(other, first);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = generated_mcp_token(&dir).unwrap_err();
        assert!(error.contains("0600"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
