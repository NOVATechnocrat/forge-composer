//! config.toml load/init, role resolution, and secret enumeration.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub server: ServerCfg,
    pub providers: BTreeMap<String, ProviderCfg>,
    pub roles: BTreeMap<String, RoleCfg>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ServerCfg {
    pub port: u16,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProviderCfg {
    pub base_url: String,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RoleCfg {
    pub provider: String,
    pub model: String,
}

const DEFAULT_CONFIG: &str = r#"[server]
port = 8642

[providers.ollama]
base_url = "http://127.0.0.1:11434/v1"

[providers.fireworks]
base_url = "https://api.fireworks.ai/inference/v1"
api_key_env = "FIREWORKS_API_KEY"

[roles.orchestrator]
provider = "ollama"
model = "qwen2.5:14b-instruct"
"#;

/// Load `<dir>/config.toml`, or write the default and parse it if absent.
pub fn load_or_init(dir: &Path) -> anyhow::Result<Config> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("config.toml");
    if !path.exists() {
        std::fs::write(&path, DEFAULT_CONFIG)?;
    }
    let text = std::fs::read_to_string(&path)?;
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("failed to parse config.toml: {e}"))?;
    if !cfg.roles.contains_key("orchestrator") {
        anyhow::bail!("config.toml must define a roles.orchestrator");
    }
    Ok(cfg)
}

/// Join provider + role, reading the api key from the env var named by the
/// provider's `api_key_env` (if any).
pub fn resolve_role(cfg: &Config, role: &str) -> anyhow::Result<gateway::ProviderConfig> {
    let role_cfg = cfg
        .roles
        .get(role)
        .ok_or_else(|| anyhow::anyhow!("unknown role: {role}"))?;
    let provider = cfg
        .providers
        .get(&role_cfg.provider)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {}", role_cfg.provider))?;
    let api_key = match &provider.api_key_env {
        Some(var) => std::env::var(var).ok().filter(|s| !s.is_empty()),
        None => None,
    };
    Ok(gateway::ProviderConfig {
        base_url: provider.base_url.clone(),
        model: role_cfg.model.clone(),
        api_key,
        kind: gateway::ProviderKind::OpenAI,
    })
}

/// Every resolvable `api_key_env` value currently present in the environment.
/// Feeds the Redactor so secrets never hit a ledger byte.
pub fn secrets(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for p in cfg.providers.values() {
        if let Some(var) = &p.api_key_env {
            if let Ok(val) = std::env::var(var) {
                if !val.is_empty() {
                    out.push(val);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_init_writes_default_with_orchestrator() {
        let d = tempfile::tempdir().unwrap();
        let cfg = load_or_init(d.path()).unwrap();
        assert_eq!(cfg.server.port, 8642);
        assert!(cfg.roles.contains_key("orchestrator"));
        assert!(d.path().join("config.toml").exists());
        // Second call reuses the file.
        let cfg2 = load_or_init(d.path()).unwrap();
        assert_eq!(cfg2.server.port, 8642);
    }

    #[test]
    fn resolve_role_picks_up_api_key_env_and_secrets_includes_it() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"[server]
port = 9000

[providers.stub]
base_url = "http://127.0.0.1:0/v1"
api_key_env = "FC_TEST_KEY_T3"

[roles.orchestrator]
provider = "stub"
model = "stub-model"
"#,
        )
        .unwrap();
        std::env::set_var("FC_TEST_KEY_T3", "sk-test-SECRET-999");
        let cfg = load_or_init(d.path()).unwrap();
        let pc = resolve_role(&cfg, "orchestrator").unwrap();
        assert_eq!(pc.model, "stub-model");
        assert_eq!(pc.api_key.as_deref(), Some("sk-test-SECRET-999"));
        let secs = secrets(&cfg);
        assert!(secs.contains(&"sk-test-SECRET-999".to_string()));
        std::env::remove_var("FC_TEST_KEY_T3");
    }
}
