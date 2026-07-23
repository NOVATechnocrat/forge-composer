use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(|s| s.as_str()).unwrap_or("composerd");
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("serve");

    match sub {
        "--version" | "-V" => {
            println!("composerd {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        "init" => {
            if let Err(code) = cmd_init(&args[2..]) {
                std::process::exit(code);
            }
        }
        "sessions" => {
            if let Err(code) = cmd_sessions() {
                std::process::exit(code);
            }
        }
        "ledger" => {
            let id = match args.get(2) {
                Some(s) => s,
                None => {
                    eprintln!("usage: {prog} ledger <session-id>");
                    std::process::exit(2);
                }
            };
            if let Err(code) = cmd_ledger(id) {
                std::process::exit(code);
            }
        }
        "checkpoints" => {
            let id = match args.get(2) {
                Some(s) => s,
                None => {
                    eprintln!("usage: {prog} checkpoints <session-id>");
                    std::process::exit(2);
                }
            };
            if let Err(code) = cmd_checkpoints(id) {
                std::process::exit(code);
            }
        }
        "serve" => {
            if let Err(e) = run_serve() {
                eprintln!("composerd: {e}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: {prog} [serve|init|sessions|ledger <id>|checkpoints <id>]");
            std::process::exit(2);
        }
    }
}

fn state_dir() -> PathBuf {
    composerd::state::state_dir()
}

/// `composerd init [--dir <state-dir>] [--provider ollama|fireworks]` — write a
/// starter config.toml ONLY when absent (never clobber; exit nonzero otherwise).
fn cmd_init(args: &[String]) -> Result<(), i32> {
    let mut dir = state_dir();
    let mut provider = "ollama".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => {
                i += 1;
                let Some(d) = args.get(i) else {
                    eprintln!("init: --dir requires a value");
                    return Err(2);
                };
                dir = PathBuf::from(d);
            }
            "--provider" => {
                i += 1;
                let Some(p) = args.get(i) else {
                    eprintln!("init: --provider requires a value");
                    return Err(2);
                };
                provider = p.clone();
            }
            other => {
                eprintln!("init: unknown argument: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    let model = match provider.as_str() {
        "ollama" => "qwen2.5:14b-instruct",
        "fireworks" => "accounts/fireworks/models/glm-5p2",
        other => {
            eprintln!("init: unknown provider: {other} (expected ollama|fireworks)");
            return Err(2);
        }
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("init: cannot create state dir {}: {e}", dir.display());
        return Err(1);
    }
    let cfg_path = dir.join("config.toml");
    if cfg_path.exists() {
        eprintln!(
            "init: refusing to clobber existing config: {}",
            cfg_path.display()
        );
        return Err(1);
    }

    let provider_block = match provider.as_str() {
        "ollama" => format!(
            "[providers.ollama]\nbase_url = \"http://127.0.0.1:11434/v1\"\n"
        ),
        "fireworks" => format!(
            "[providers.fireworks]\nbase_url = \"https://api.fireworks.ai/inference/v1\"\napi_key_env = \"FIREWORKS_API_KEY\"\n"
        ),
        _ => unreachable!(),
    };
    let template = format!(
        "[server]\nport = 8642\n\n\
{provider_block}\n\
# Cursor-parity: auto-apply edits. The safety net is the cockpit — every edit\n\
# takes a shadow checkpoint, so /diff shows what changed and /restore reverts it.\n\
[roles.orchestrator]\nprovider = \"{provider}\"\nmodel = \"{model}\"\n\n\
[roles.coder]\nprovider = \"{provider}\"\nmodel = \"{model}\"\n\n\
[policy]\nauto_approve_edits = true\n\n\
# [budgets]\n# session_usd = 5.0\n"
    );
    if let Err(e) = std::fs::write(&cfg_path, template) {
        eprintln!("init: cannot write {}: {e}", cfg_path.display());
        return Err(1);
    }
    println!("{}", cfg_path.display());
    Ok(())
}

fn run_serve() -> anyhow::Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let dir = composerd::state::state_dir();
        let (addr, handle) = composerd::serve::bind_and_serve(&dir, None).await?;
        println!("composerd 0.1.0 listening on http://{addr}");
        handle.await?;
        Ok(())
    })
}

fn cmd_sessions() -> Result<(), i32> {
    let dir = state_dir();
    let store = ledger::SessionStore::new(dir.join("sessions"), ledger::Redactor::default());
    let sessions = store.list_sessions().map_err(|e| {
        eprintln!("error listing sessions: {e}");
        1
    })?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for s in &sessions {
        let _ = writeln!(out, "{s}");
    }
    Ok(())
}

fn cmd_ledger(id: &str) -> Result<(), i32> {
    let dir = state_dir();
    let path = dir.join("sessions").join(id).join("ledger.jsonl");
    if !path.exists() {
        eprintln!("unknown session: {id}");
        return Err(2);
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error reading ledger: {e}");
            return Err(1);
        }
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(&bytes);
    Ok(())
}

fn cmd_checkpoints(id: &str) -> Result<(), i32> {
    let dir = state_dir();
    let session_dir = dir.join("sessions").join(id);
    if !session_dir.exists() {
        eprintln!("unknown session: {id}");
        return Err(2);
    }
    let workspace = composerd::state::load_meta(&dir, id)
        .map_err(|e| {
            eprintln!("error reading session meta: {e}");
            1
        })?
        .map(|m| m.workspace)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let shadow = tools::Shadow::init(&session_dir, &workspace).map_err(|e| {
        eprintln!("error opening shadow repo: {e}");
        1
    })?;
    let list = shadow.list().map_err(|e| {
        eprintln!("error listing checkpoints: {e}");
        1
    })?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (hash, label) in &list {
        let _ = writeln!(out, "{hash} {label}");
    }
    Ok(())
}
