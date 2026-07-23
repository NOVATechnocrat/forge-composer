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
        "serve" => {
            // M0: the HTTP server is exercised via the testkit in tests; the bare
            // binary prints a banner for now. Full serve wiring lands with the
            // extension integration milestone.
            println!("composerd 0.1.0 — M0 spine; see docs/superpowers/specs/");
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            eprintln!("usage: {prog} [serve|sessions|ledger <id>]");
            std::process::exit(2);
        }
    }
}

fn state_dir() -> PathBuf {
    composerd::state::state_dir()
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
