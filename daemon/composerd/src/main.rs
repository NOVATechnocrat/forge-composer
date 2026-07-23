use std::io::Write;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().map(|s| s.as_str()).unwrap_or("composerd");
    let sub = args.get(1).map(|s| s.as_str()).unwrap_or("serve");

    match sub {
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
            eprintln!("usage: {prog} [serve|sessions|ledger <id>|checkpoints <id>]");
            std::process::exit(2);
        }
    }
}

fn state_dir() -> PathBuf {
    composerd::state::state_dir()
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
