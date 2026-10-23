use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: String,
    pub token: String,
    pub name: String,
    pub tls_insecure: bool,
    pub sock: PathBuf,
    pub state_dir: PathBuf,
    pub mcp_listen: String,
    pub mcp_token: String,
    pub remote_messages: String,
    pub local_only: bool,
    pub event_log_size: usize,
    #[serde(rename = "endpoint")]
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
            .map(|v| PathBuf::from(v).join("agent-intercom/client.sock"))
            .unwrap_or_else(|| {
                #[cfg(unix)]
                let uid = unsafe { libc::getuid() };
                #[cfg(not(unix))]
                let uid = 0;
                std::env::temp_dir().join(format!("agent-intercom-{uid}/client.sock"))
            });
        Self {
            server: "https://127.0.0.1:8443".into(),
            token: String::new(),
            name: String::new(),
            tls_insecure: false,
            sock,
            state_dir: home.join(".agent-intercom"),
            mcp_listen: "127.0.0.1:18790".into(),
            mcp_token: String::new(),
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
pub fn default_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("agent-intercom.toml")
}

impl Config {
    pub fn load(opts: &super::cli::Options) -> Result<Self, String> {
        let path = opts.config.clone().unwrap_or_else(default_path);
        let mut c = match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).map_err(|e| format!("parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        for (key, target) in [
            ("SERVER", &mut c.server),
            ("TOKEN", &mut c.token),
            ("NAME", &mut c.name),
            ("MCP_LISTEN", &mut c.mcp_listen),
            ("MCP_TOKEN", &mut c.mcp_token),
            ("REMOTE_MESSAGES", &mut c.remote_messages),
        ] {
            if let Ok(v) = std::env::var(format!("AGENT_INTERCOM_{key}")) {
                if !v.is_empty() {
                    *target = v;
                }
            }
        }
        if let Some(v) = std::env::var_os("AGENT_INTERCOM_SOCK") {
            if !v.is_empty() {
                c.sock = v.into();
            }
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
        if !self.mcp_listen.is_empty()
            && self.mcp_token.trim().is_empty()
            && !loopback_listen(&self.mcp_listen)
        {
            return Err(format!(
                "mcp_listen {:?} is not a loopback address; set mcp_token to expose MCP beyond this host",
                self.mcp_listen
            ));
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
        let error =
            toml::from_str::<Config>("[[endpoint]]\nname = 'hook'\nurl = 'https://x'\nsecrte = 's'\n")
                .unwrap_err();
        assert!(error.to_string().contains("secrte"), "{error}");
        let parsed: Config = toml::from_str(
            "local_only = true\nmcp_listen = ''\n[[endpoint]]\nname = 'hook'\nurl = 'https://x'\n",
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
    fn non_loopback_mcp_listen_requires_a_token() {
        let config = |listen: &str, token: &str| Config {
            mcp_listen: listen.into(),
            mcp_token: token.into(),
            ..Config::default()
        };
        for listen in ["0.0.0.0:18790", "[::]:18790", "192.0.2.1:0", "example.com:80"] {
            let error = config(listen, "").validate().unwrap_err();
            assert!(error.contains("mcp_token"), "{listen}: {error}");
            assert!(config(listen, "  ").validate().is_err(), "{listen}");
            assert!(config(listen, "secret").validate().is_ok(), "{listen}");
        }
        for listen in ["", "127.0.0.1:0", "127.1.2.3:9", "[::1]:18790", "localhost:18790"] {
            assert!(config(listen, "").validate().is_ok(), "{listen}");
        }
    }
}
