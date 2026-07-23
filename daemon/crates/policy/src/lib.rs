//! policy — deny-by-default command verdicts over parsed argv.

/// `*` matches any run (including empty), `?` one char.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn rec(p: &[u8], t: &[u8]) -> bool {
        match (p.split_first(), t.split_first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some((pc, _)), None) => *pc == b'*' && rec(&p[1..], t),
            (Some((b'*', _)), Some(_)) => rec(&p[1..], t) || rec(p, &t[1..]),
            (Some((b'?', _)), Some(_)) => rec(&p[1..], &t[1..]),
            (Some((pc, _)), Some((tc, _))) => *pc == *tc && rec(&p[1..], &t[1..]),
        }
    }
    rec(pattern.as_bytes(), text.as_bytes())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Auto,
    Ask,
    Deny(String),
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Auto,
    Ask,
    Deny,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct Rule {
    pub globs: Vec<String>,
    pub action: RuleAction,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    user_rules: Vec<Rule>,
}

impl Policy {
    pub fn new(user_rules: Vec<Rule>) -> Self {
        Self { user_rules }
    }

    /// Verdict for a raw shell command string. Splits on `&&`, `||`, `;`, `|`
    /// into segments; the strictest segment verdict wins (Deny > Ask > Auto).
    pub fn check(&self, command: &str) -> Verdict {
        if command.contains('`') || command.contains("$(") {
            return Verdict::Deny("command substitution".into());
        }
        let mut strictest = Verdict::Auto;
        for raw_seg in split_raw_segments(command) {
            let argv = match shell_words::split(&raw_seg) {
                Ok(a) => a,
                Err(_) => return Verdict::Deny("unparseable command".into()),
            };
            if argv.is_empty() {
                return Verdict::Deny("empty command segment".into());
            }
            let v = self.check_segment(&argv);
            if matches!(v, Verdict::Deny(_)) {
                return v;
            }
            strictest = stricter(&strictest, &v);
        }
        strictest
    }

    fn check_segment(&self, argv: &[String]) -> Verdict {
        if let Some(reason) = hard_deny(argv) {
            return Verdict::Deny(reason);
        }
        for rule in &self.user_rules {
            if rule_matches(rule, argv) {
                return match rule.action {
                    RuleAction::Auto => Verdict::Auto,
                    RuleAction::Ask => Verdict::Ask,
                    RuleAction::Deny => Verdict::Deny("user rule".into()),
                };
            }
        }
        if is_readonly(argv) {
            return Verdict::Auto;
        }
        Verdict::Ask
    }
}

fn stricter(a: &Verdict, b: &Verdict) -> Verdict {
    fn rank(v: &Verdict) -> u8 {
        match v {
            Verdict::Auto => 0,
            Verdict::Ask => 1,
            Verdict::Deny(_) => 2,
        }
    }
    if rank(b) > rank(a) {
        b.clone()
    } else {
        a.clone()
    }
}

/// Split a raw command string on the control operators `&&`, `||`, `;`, `|`
/// (quote-aware: operators inside single/double quotes are literal). Each
/// returned slice is one segment's worth of shell text.
fn split_raw_segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let bytes = command.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            cur.push(b as char);
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            quote = Some(b);
            cur.push(b as char);
            i += 1;
            continue;
        }
        // Two-char operators first.
        if b == b'&' && i + 1 < bytes.len() && bytes[i + 1] == b'&' {
            out.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        if b == b'|' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            out.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        if b == b';' || b == b'|' {
            out.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(b as char);
        i += 1;
    }
    out.push(cur);
    out
}

fn rule_matches(rule: &Rule, argv: &[String]) -> bool {
    if argv.len() < rule.globs.len() {
        return false;
    }
    rule.globs
        .iter()
        .zip(argv.iter())
        .all(|(g, a)| glob_match(g, a))
}

/// Built-in non-overridable hard denies. Returns a reason string on match.
fn hard_deny(argv: &[String]) -> Option<String> {
    // Table of element-wise glob prefixes.
    const TABLE: &[(&[&str], &str)] = &[
        (&["sudo"], "sudo is not allowed"),
        (&["mkfs*"], "mkfs is not allowed"),
        (&["git", "push", "--force*"], "force push is not allowed"),
        (&["git", "push", "-f*"], "force push is not allowed"),
        (&["chmod", "-R", "777"], "chmod -R 777 is not allowed"),
    ];
    for (globs, reason) in TABLE {
        if argv.len() >= globs.len()
            && globs.iter().zip(argv.iter()).all(|(g, a)| glob_match(g, a))
        {
            return Some((*reason).into());
        }
    }
    // dd with of=/dev/...
    if argv.first().map(|s| s == "dd").unwrap_or(false)
        && argv.iter().any(|a| a.starts_with("of=/dev/"))
    {
        return Some("dd to a block device is not allowed".into());
    }
    // rm with a flag containing both r and f (case-insensitive).
    if argv.first().map(|s| s == "rm").unwrap_or(false)
        && argv
            .iter()
            .filter(|a| a.starts_with('-'))
            .any(|a| {
                let low = a.to_lowercase();
                low.contains('r') && low.contains('f')
            })
    {
        return Some("rm -rf is not allowed".into());
    }
    // mint.sh protection.
    if argv.iter().any(|a| a.contains("forgeloop/harness/mint.sh")) {
        return Some("mint.sh is sealed by the Architect".into());
    }
    // .env read protection: any element ending ".env" -> deny unless argv[0] == "ls".
    if argv.iter().any(|a| a.ends_with(".env")) && argv.first().map(|s| s != "ls").unwrap_or(true) {
        return Some(".env files are protected".into());
    }
    // .ssh protection: any element containing /.ssh/ or ending /.ssh.
    if argv
        .iter()
        .any(|a| a.contains("/.ssh/") || a.ends_with("/.ssh"))
    {
        return Some(".ssh paths are protected".into());
    }
    None
}

fn is_readonly(argv: &[String]) -> bool {
    // Narrowing: a few otherwise-read-only commands become mutating once
    // certain flags appear, so they fall through to Ask instead of Auto.
    if let Some(first) = argv.first() {
        if first == "find" {
            const FIND_MUTATING: &[&str] = &[
                "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprintf", "-fls",
            ];
            if argv.iter().skip(1).any(|a| FIND_MUTATING.contains(&a.as_str())) {
                return false;
            }
        }
        if first == "git" && argv.get(1).map(|s| s == "branch").unwrap_or(false) {
            const BRANCH_MUTATING: &[&str] = &[
                "-d", "-D", "-m", "-M", "-c", "-C", "--delete", "--move", "--copy", "--force",
                "--edit-description", "--set-upstream-to", "--unset-upstream",
            ];
            if argv.iter().skip(2).any(|a| BRANCH_MUTATING.contains(&a.as_str())) {
                return false;
            }
        }
    }
    const READONLY: &[&str] = &[
        "ls", "pwd", "rg", "grep", "find", "wc", "head", "tail", "cat", "file",
        "stat", "which",
    ];
    if let Some(first) = argv.first() {
        if READONLY.contains(&first.as_str()) {
            return true;
        }
    }
    const GIT_RO: &[&[&str]] = &[
        &["git", "status"],
        &["git", "diff"],
        &["git", "log"],
        &["git", "show"],
        &["git", "branch"],
    ];
    for prefix in GIT_RO {
        if argv.len() >= prefix.len()
            && prefix.iter().zip(argv.iter()).all(|(p, a)| p == a)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_denies_are_not_overridable() {
        let permissive = Policy::new(vec![Rule {
            globs: vec!["*".into()],
            action: RuleAction::Auto,
        }]);
        for cmd in [
            "rm -rf /tmp/x", "rm -fr .", "rm -Rf /", "sudo apt install x",
            "mkfs.ext4 /dev/sda1", "dd if=/dev/zero of=/dev/sda",
            "git push --force origin main", "git push -f",
            "chmod -R 777 .", "bash ~/Code/forgeloop/harness/mint.sh --status minted",
            "cat ~/.aws/creds.env", "cp secrets.env /tmp/", "echo `whoami`",
            "echo $(rm -rf /)", "cat ~/.ssh/id_ed25519",
        ] {
            assert!(matches!(permissive.check(cmd), Verdict::Deny(_)), "not denied: {cmd}");
        }
    }

    #[test]
    fn read_only_is_auto_and_default_is_ask() {
        let p = Policy::default();
        assert_eq!(p.check("ls -la"), Verdict::Auto);
        assert_eq!(p.check("rg TODO src"), Verdict::Auto);
        assert_eq!(p.check("git status"), Verdict::Auto);
        assert_eq!(p.check("cargo build"), Verdict::Ask);
        assert_eq!(p.check("echo hello && cargo test"), Verdict::Ask); // strictest wins
        assert!(matches!(p.check("ls; rm -rf /"), Verdict::Deny(_)));
    }

    #[test]
    fn user_rules_apply_in_order_between_layers() {
        let p = Policy::new(vec![
            Rule { globs: vec!["cargo".into(), "build*".into()], action: RuleAction::Auto },
            Rule { globs: vec!["npm".into()], action: RuleAction::Deny },
        ]);
        assert_eq!(p.check("cargo build --release"), Verdict::Auto);
        assert!(matches!(p.check("npm install"), Verdict::Deny(_)));
        assert_eq!(p.check("cargo publish"), Verdict::Ask);
    }

    #[test]
    fn readonly_commands_with_mutating_args_are_not_auto() {
        let p = Policy::default();
        for cmd in [
            "find . -name canary.txt -delete",
            "find /tmp -exec rm",
            "find . -okdir chmod 600",
            "git branch -D main",
            "git branch -M old new",
            "git branch --delete feature",
        ] {
            assert_eq!(p.check(cmd), Verdict::Ask, "must not auto-run: {cmd}");
        }
        // Genuinely read-only forms stay Auto.
        assert_eq!(p.check("find . -name canary.txt"), Verdict::Auto);
        assert_eq!(p.check("git branch --list"), Verdict::Auto);
        assert_eq!(p.check("git branch"), Verdict::Auto);
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("--force*", "--force-with-lease"));
        assert!(glob_match("mkfs*", "mkfs.ext4"));
        assert!(glob_match("*", "anything"));
        assert!(!glob_match("git", "gitx"));
    }
}
