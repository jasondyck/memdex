//! `remember`: append a fact to a topic note in the shared vault.
//!
//! Notes are plain markdown with Claude-memory-style frontmatter, so any agent,
//! Obsidian, or a human can read and edit them. Each fact is one dated bullet
//! tagged with the agent that wrote it. Writes are serialised with a lock file,
//! written atomically, and committed to git when the vault is a repository.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process::Command;

pub const MAX_FACT: usize = 2000;
const TYPES: &[&str] = &["user", "feedback", "project", "reference"];

/// Topic name → note file stem: lowercase ASCII letters, digits and dashes.
pub fn slugify(topic: &str) -> Option<String> {
    let mut s = String::new();
    for c in topic.trim().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').to_string();
    (!s.is_empty() && s.len() <= 64).then_some(s)
}

/// Why `text` looks like it contains a credential, if it does.
pub fn looks_secret(text: &str) -> Option<&'static str> {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_live_",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "tskey-",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "hf_",
    ];
    if text.contains("-----BEGIN") {
        return Some("a PEM key block");
    }
    for word in text.split(|c: char| c.is_whitespace() || "\"'`()[]{}<>,;".contains(c)) {
        let w = word.trim_matches(|c: char| ".:".contains(c));
        // Known token formats.
        if PREFIXES.iter().any(|p| w.starts_with(p) && w.len() >= p.len() + 16) {
            return Some("a known API-token format");
        }
        // key=value / key: value pairs whose key names a credential.
        // Checks every `=`/`:` so `https://host/?token=…` is caught too.
        for (i, c) in w.char_indices() {
            if c != '=' && c != ':' {
                continue;
            }
            let k = w[..i]
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let v = w[i + 1..].split('&').next().unwrap_or("");
            let credential_key = [
                "password", "passwd", "pwd", "secret", "token", "apikey", "api_key", "api-key", "auth",
            ]
            .iter()
            .any(|n| k.ends_with(n));
            if credential_key && v.len() >= 6 && !v.starts_with('<') && !v.starts_with("${") && !v.starts_with("***") {
                return Some("a credential assignment");
            }
        }
        // Long random-looking strings: mixed case + digits, 32+ chars.
        let alnum = w.len() >= 32 && w.chars().all(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(c));
        if alnum
            && w.chars().any(|c| c.is_ascii_uppercase())
            && w.chars().any(|c| c.is_ascii_lowercase())
            && w.chars().any(|c| c.is_ascii_digit())
        {
            return Some("a long random-looking string");
        }
    }
    // "password is hunter2" style phrasing.
    let lower = text.to_ascii_lowercase();
    for kw in ["password is ", "password: ", "passphrase is ", "pin is "] {
        if let Some(i) = lower.find(kw) {
            let rest = text[i + kw.len()..].split_whitespace().next().unwrap_or("");
            if rest.len() >= 4 && !rest.starts_with('<') {
                return Some("a password written out");
            }
        }
    }
    None
}

/// Today's local date as YYYY-MM-DD. Asks the OS for the local date (`date` on Unix,
/// PowerShell on Windows — never cmd's interactive `date`), falling back to UTC.
fn today() -> String {
    #[cfg(unix)]
    let local = Command::new("date").arg("+%F").output();
    #[cfg(windows)]
    let local = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-Date -Format yyyy-MM-dd",
        ])
        .output();
    #[cfg(not(any(unix, windows)))]
    let local: std::io::Result<std::process::Output> = Err(std::io::ErrorKind::Unsupported.into());
    local
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| is_date(s))
        .unwrap_or_else(utc_date)
}

fn is_date(s: &str) -> bool {
    s.len() == 10
        && s.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

/// UTC date from the system clock (civil-from-days, H. Hinnant).
fn utc_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Append `fact` to note `topic` in `vault`, creating the note if needed.
/// Returns a short confirmation for the calling agent.
pub fn remember(
    vault: &Path,
    agent: &str,
    topic: &str,
    fact: &str,
    description: Option<&str>,
    kind: Option<&str>,
) -> Result<String, String> {
    let fact = fact.trim();
    if fact.is_empty() {
        return Err("`fact` is empty".into());
    }
    if fact.chars().count() > MAX_FACT {
        return Err(format!("`fact` is over {MAX_FACT} characters; store one fact per call"));
    }
    let stem = slugify(topic).ok_or("`topic` needs letters or digits (max 64), e.g. `release-checklist`")?;
    let kind = kind.unwrap_or("project");
    if !TYPES.contains(&kind) {
        return Err(format!("`type` must be one of {}", TYPES.join(", ")));
    }
    for t in [Some(fact), description, Some(topic)].into_iter().flatten() {
        if let Some(why) = looks_secret(t) {
            return Err(format!(
                "refused: this looks like it contains {why}. Never store credentials; record WHERE the secret lives (file, vault entry), not its value."
            ));
        }
    }

    fs::create_dir_all(vault).map_err(|e| e.to_string())?;
    let lock = File::create(vault.join(".memdex.lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;

    let path = vault.join(format!("{stem}.md"));
    let date = today();
    // Continuation lines are indented so the fact stays one markdown bullet.
    let bullet = format!(
        "- {date} ({agent}): {}\n",
        fact.lines().collect::<Vec<_>>().join("\n  ")
    );
    let created = !path.exists();
    let text = if created {
        let desc = one_line(description.unwrap_or(fact));
        let desc: String = desc.chars().take(150).collect();
        format!(
            "---\nname: {stem}\ndescription: {}\nmetadata:\n  type: {kind}\n  source: {agent}\n  created: {date}\n---\n\n{bullet}",
            desc.replace('\n', " ")
        )
    } else {
        let mut t = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if !t.ends_with('\n') {
            t.push('\n');
        }
        t.push_str(&bullet);
        t
    };
    let tmp = vault.join(format!(".{stem}.md.tmp"));
    {
        let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    commit(vault, &stem, agent, created);
    drop(lock);
    Ok(format!(
        "{} `{stem}` ({}).",
        if created { "Created" } else { "Added to" },
        path.display()
    ))
}

/// Best-effort git commit so every write is attributable and reversible.
fn commit(vault: &Path, stem: &str, agent: &str, created: bool) {
    if !vault.join(".git").exists() {
        return;
    }
    let file = format!("{stem}.md");
    let msg = format!("{agent}: {} {stem}", if created { "create" } else { "update" });
    let ok = Command::new("git")
        .arg("-C")
        .arg(vault)
        .args(["add", "--", &file])
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        let _ = Command::new("git")
            .arg("-C")
            .arg(vault)
            .args([
                "-c",
                &format!("user.name=memdex/{agent}"),
                "-c",
                "user.email=memdex@localhost",
            ])
            .args(["commit", "-q", "--no-verify", "-m", &msg, "--", &file])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert!(is_date(&utc_date()), "{}", utc_date());
        assert!(is_date(&today()));
        assert!(!is_date("Mon Nov  2"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Release Checklist!").unwrap(), "release-checklist");
        assert_eq!(slugify("  ../etc/passwd ").unwrap(), "etc-passwd");
        assert!(slugify("!!!").is_none());
    }

    #[test]
    fn secrets_are_caught() {
        assert!(looks_secret("api key is sk-abcdefghijklmnopqrstuvwx").is_some());
        assert!(looks_secret("set PASSWORD=hunter2hunter2").is_some());
        assert!(looks_secret("url https://x/y?token=abcdef123456").is_some());
        assert!(looks_secret("the root password is Tr0ub4dor").is_some());
        assert!(looks_secret("key aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5").is_some());
        assert!(looks_secret("-----BEGIN OPENSSH PRIVATE KEY-----").is_some());
    }

    #[test]
    fn normal_facts_pass() {
        for ok in [
            "Grafana is on port 3000 (container 3000 → host 3001)",
            "API key lives in ~/.config/agent/env; rotate both copies together",
            "token-gated UI; token URL is in the journal",
            "commit 3f6a9c1e8b2d4f70a5c3e1b9d7f2a4c6e8b0d2f4a6c8e0b2d4f6a8c0e2b4d6f8",
            "password: <stored in Bitwarden>",
            "uuid 123e4567-e89b-12d3-a456-426614174000",
        ] {
            assert!(looks_secret(ok).is_none(), "{ok}");
        }
    }

    #[test]
    fn creates_then_appends() {
        let v = std::env::temp_dir().join(format!("memdex-rem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&v);
        let r = remember(
            &v,
            "bot-a",
            "Build Cache",
            "CI cache is keyed on the lockfile",
            Some("CI cache notes"),
            None,
        )
        .unwrap();
        assert!(r.starts_with("Created"));
        remember(
            &v,
            "claude",
            "build-cache",
            "stale after toolchain bumps\nclear it manually",
            None,
            None,
        )
        .unwrap();
        let t = fs::read_to_string(v.join("build-cache.md")).unwrap();
        assert!(t.starts_with("---\nname: build-cache\ndescription: CI cache notes\n"));
        assert!(t.contains("  source: bot-a\n"));
        assert!(t.contains("(bot-a): CI cache is keyed on the lockfile\n"));
        assert!(t.contains("(claude): stale after toolchain bumps\n  clear it manually\n"));
        assert!(remember(&v, "bot-a", "x", "PASSWORD=supersecret1", None, None).is_err());
        assert!(remember(&v, "bot-a", "x", "fine", None, Some("bogus")).is_err());
        fs::remove_dir_all(&v).unwrap();
    }
}
