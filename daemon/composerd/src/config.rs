//! config.toml load/init, role resolution, and secret enumeration.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Config {
    pub server: ServerCfg,
    pub providers: BTreeMap<String, ProviderCfg>,
    pub roles: BTreeMap<String, RoleCfg>,
    #[serde(default)]
    pub policy: PolicyCfg,
    #[serde(default)]
    pub pricing: BTreeMap<String, PriceCfg>,
    #[serde(default)]
    pub budgets: BudgetCfg,
    #[serde(default)]
    pub forgeloop: ForgeloopCfg,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ServerCfg {
    pub port: u16,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ProviderCfg {
    pub base_url: String,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RoleCfg {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub escalation: Vec<String>,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ForgeloopCfg {
    #[serde(default)]
    pub dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PolicyCfg {
    #[serde(default)]
    pub auto_approve_edits: bool,
    #[serde(default = "default_approval_timeout")]
    pub approval_timeout_secs: u64,
    #[serde(default)]
    pub rules: Vec<policy::Rule>,
}

impl Default for PolicyCfg {
    fn default() -> Self {
        Self {
            auto_approve_edits: false,
            approval_timeout_secs: default_approval_timeout(),
            rules: Vec::new(),
        }
    }
}

fn default_approval_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PriceCfg {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct BudgetCfg {
    #[serde(default)]
    pub session_usd: Option<f64>,
}

/// Cost of one model call, or None when the model has no price entry —
/// unknown pricing is surfaced as unmetered, never silently $0.
pub fn cost_usd(cfg: &Config, model: &str, prompt: u64, completion: u64) -> Option<f64> {
    let p = cfg.pricing.get(model)?;
    Some(
        (prompt as f64 / 1_000_000.0) * p.input_per_mtok
            + (completion as f64 / 1_000_000.0) * p.output_per_mtok,
    )
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
        kind: match provider.kind.as_str() {
            "" | "openai" => gateway::ProviderKind::OpenAI,
            "anthropic" => gateway::ProviderKind::Anthropic,
            other => anyhow::bail!("unknown provider kind: {other}"),
        },
    })
}

/// The role's provider config followed by its escalation tiers, in order.
/// Each escalation entry naming an unknown role is an `Err` (config bug, fail
/// loudly at turn start rather than silently rerouting).
pub fn resolve_chain(
    cfg: &Config,
    role: &str,
) -> anyhow::Result<Vec<(String, gateway::ProviderConfig)>> {
    let mut out = vec![(role.to_string(), resolve_role(cfg, role)?)];
    let role_cfg = cfg
        .roles
        .get(role)
        .ok_or_else(|| anyhow::anyhow!("unknown role: {role}"))?;
    for next in &role_cfg.escalation {
        out.push((next.clone(), resolve_role(cfg, next)?));
    }
    Ok(out)
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

/// Every configured `api_key_env` name (whether or not currently set) — the
/// terminal executor scrubs these from child envs in addition to the
/// KEY|TOKEN|SECRET|PASSWORD|CREDENTIAL name match.
pub fn api_key_env_names(cfg: &Config) -> Vec<String> {
    cfg.providers
        .values()
        .filter_map(|p| p.api_key_env.clone())
        .collect()
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

    #[test]
    fn pricing_and_budgets_parse_and_cost_computes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"[server]
port = 9000

[providers.stub]
base_url = "http://127.0.0.1:0/v1"

[roles.orchestrator]
provider = "stub"
model = "stub-m2"

[pricing."stub-m2"]
input_per_mtok = 1.0
output_per_mtok = 2.0

[budgets]
session_usd = 5.0
"#,
        )
        .unwrap();
        let cfg = load_or_init(d.path()).unwrap();
        assert_eq!(cfg.budgets.session_usd, Some(5.0));
        // 1M prompt @ $1 + 0.5M completion @ $2 = $2.00
        let c = cost_usd(&cfg, "stub-m2", 1_000_000, 500_000).unwrap();
        assert!((c - 2.0).abs() < 1e-9, "{c}");
        assert!(cost_usd(&cfg, "unknown-model", 1, 1).is_none());
    }

    #[test]
    fn pricing_and_budgets_default_empty() {
        let d = tempfile::tempdir().unwrap();
        let cfg = load_or_init(d.path()).unwrap();
        assert!(cfg.pricing.is_empty());
        assert!(cfg.budgets.session_usd.is_none());
    }

    #[test]
    fn forgeloop_dir_and_escalation_chain_parse() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"[server]
port = 9000

[providers.a]
base_url = "http://127.0.0.1:1/v1"

[providers.b]
base_url = "http://127.0.0.1:2/v1"

[roles.orchestrator]
provider = "a"
model = "m-a"
escalation = ["fallback"]

[roles.fallback]
provider = "b"
model = "m-b"

[forgeloop]
dir = "/tmp/fl"
"#,
        )
        .unwrap();
        let cfg = load_or_init(d.path()).unwrap();
        assert_eq!(cfg.forgeloop.dir.as_deref(), Some(std::path::Path::new("/tmp/fl")));
        let chain = resolve_chain(&cfg, "orchestrator").unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].0, "orchestrator");
        assert_eq!(chain[0].1.model, "m-a");
        assert_eq!(chain[1].0, "fallback");
        assert_eq!(chain[1].1.model, "m-b");
    }

    #[test]
    fn escalation_to_unknown_role_errors() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            r#"[server]
port = 9000

[providers.a]
base_url = "http://127.0.0.1:1/v1"

[roles.orchestrator]
provider = "a"
model = "m-a"
escalation = ["ghost"]
"#,
        )
        .unwrap();
        let cfg = load_or_init(d.path()).unwrap();
        assert!(resolve_chain(&cfg, "orchestrator").is_err());
    }
}
