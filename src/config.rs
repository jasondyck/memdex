//! Per-agent configuration: which memory dirs an agent may read, which notes are
//! hidden from it, whether it may write, and (for HTTP clients) its bearer token.
//!
//! File format, one directive per line, `#` comments, `~` expands to $HOME:
//!
//! ```text
//! agent helper-bot          # name recorded on everything this agent remembers
//! claude-dirs               # every ~/.claude/projects/*/memory dir
//! dir ~/memory shared       # a directory to index, with an optional label
//! write-dir ~/memory        # where `remember` writes (also indexed, label "shared")
//! exclude payroll           # hide a note by name ...
//! exclude home/client-list  # ... or by label/name
//! token 3f9c…               # HTTP mode: bearer token for this client
//! read-only                 # no `remember` tool
//! ```

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub agent: String,
    pub dirs: Vec<(PathBuf, String)>,
    pub exclude: Vec<String>,
    pub write_dir: Option<PathBuf>,
    pub token: Option<String>,
    /// Model2Vec model directory for hybrid (embedding) search; None = BM25 only.
    pub model: Option<PathBuf>,
    /// Re-scan ~/.claude/projects on every call so a project's FIRST memory is
    /// found mid-session (set by `claude-dirs` and the no-config default).
    pub watch_claude: bool,
}

pub const SHARED_LABEL: &str = "shared";

/// The user's home directory: `HOME` on Unix, `USERPROFILE` on Windows.
pub fn home() -> PathBuf {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_default()
}

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~") {
        Some(rest) => home().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(p),
    }
}

/// How Claude Code names a project's folder under ~/.claude/projects: every character
/// that is not an ASCII letter or digit becomes `-` (`/home/a/.x` → `-home-a--x`,
/// `C:\Users\a\proj` → `C--Users-a-proj`).
pub fn encode_project_path(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `~/.claude/projects/-home-alex-projects-webshop/memory` → `projects-webshop`;
/// the home-directory project becomes `home`.
pub fn claude_label(memory_dir: &Path) -> String {
    let dir = memory_dir
        .parent()
        .and_then(|d| d.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let home = encode_project_path(&home());
    match dir.strip_prefix(&home) {
        Some("") => "home".into(),
        Some(rest) => rest.trim_start_matches('-').to_string(),
        None => dir.trim_start_matches('-').to_string(),
    }
}

/// Where `memdex tokens` looks for a model when none is given. Embeddings are OPT-IN
/// (`model DIR` / `--model DIR`): on the 2026-11-02 benchmarks they did not beat
/// BM25 on independent questions, and cost 18–130 MB of RAM.
pub fn default_model() -> Option<PathBuf> {
    let d = home().join(".local/share/memdex/models").join(DEFAULT_MODEL);
    d.join("model.safetensors").exists().then_some(d)
}

pub const DEFAULT_MODEL: &str = "potion-base-8M";

pub fn claude_dirs() -> Vec<(PathBuf, String)> {
    let root = home().join(".claude/projects");
    let mut dirs: Vec<(PathBuf, String)> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("memory"))
        .filter(|p| p.is_dir())
        .map(|p| {
            let l = claude_label(&p);
            (p, l)
        })
        .collect();
    dirs.sort();
    dirs
}

impl Config {
    fn empty() -> Self {
        Config {
            agent: "agent".into(),
            dirs: vec![],
            exclude: vec![],
            write_dir: None,
            token: None,
            model: None,
            watch_claude: false,
        }
    }

    /// No config file: Claude Code on this machine — every Claude memory dir plus
    /// the shared vault, with write access to the shared vault.
    pub fn default_local() -> Self {
        let mut c = Config::empty();
        c.agent = "claude".into();
        c.dirs = claude_dirs();
        c.watch_claude = true;
        c.set_write_dir(home().join("memory"));
        c
    }

    pub fn set_write_dir(&mut self, p: PathBuf) {
        if !self.dirs.iter().any(|(d, _)| d == &p) {
            self.dirs.push((p.clone(), SHARED_LABEL.into()));
        }
        self.write_dir = Some(p);
    }

    pub fn add_dir(&mut self, p: PathBuf, label: Option<String>) {
        if self.dirs.iter().any(|(d, _)| d == &p) {
            return;
        }
        let label = label.unwrap_or_else(|| {
            if p.ends_with("memory")
                && p.parent()
                    .and_then(|x| x.parent())
                    .is_some_and(|x| x.ends_with(".claude/projects"))
            {
                claude_label(&p)
            } else {
                p.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }
        });
        self.dirs.push((p, label));
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut c = Config::empty();
        for (n, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let key = it.next().unwrap_or("");
            let arg = it.next();
            let need = |a: Option<&str>| {
                a.map(str::to_string)
                    .ok_or(format!("line {}: `{key}` needs a value", n + 1))
            };
            match key {
                "agent" => c.agent = need(arg)?,
                "claude-dirs" => {
                    c.watch_claude = true;
                    for (d, l) in claude_dirs() {
                        c.add_dir(d, Some(l));
                    }
                }
                "dir" => {
                    let p = expand(&need(arg)?);
                    c.add_dir(p, it.next().map(str::to_string));
                }
                "write-dir" => c.set_write_dir(expand(&need(arg)?)),
                "exclude" => c.exclude.push(need(arg)?),
                "token" => c.token = Some(need(arg)?),
                "read-only" => c.write_dir = None,
                "model" => c.model = Some(expand(&need(arg)?)),
                "no-model" => c.model = None,
                other => return Err(format!("line {}: unknown directive `{other}`", n + 1)),
            }
        }
        if c.token.as_ref().is_some_and(|t| t.len() < 24) {
            return Err("token must be at least 24 characters".into());
        }
        Ok(c)
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let c = Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        if c.token.is_some() {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path).map(|m| m.permissions().mode()).unwrap_or(0);
            if mode & 0o077 != 0 {
                return Err(format!(
                    "{} holds a token but is readable by others; chmod 600 it",
                    path.display()
                ));
            }
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_directives() {
        let c = Config::parse(
            "agent helper-bot\ndir /tmp/a alpha # comment\nwrite-dir /tmp/w\nexclude payroll\nexclude home/x\ntoken 0123456789abcdef0123456789\n",
        )
        .unwrap();
        assert_eq!(c.agent, "helper-bot");
        assert_eq!(c.dirs[0], (PathBuf::from("/tmp/a"), "alpha".to_string()));
        assert_eq!(c.dirs[1], (PathBuf::from("/tmp/w"), SHARED_LABEL.to_string()));
        assert_eq!(c.write_dir, Some(PathBuf::from("/tmp/w")));
        assert_eq!(c.exclude, vec!["payroll", "home/x"]);
    }

    #[test]
    fn read_only_drops_write_and_rejects_junk() {
        let c = Config::parse("write-dir /tmp/w\nread-only\n").unwrap();
        assert!(c.write_dir.is_none());
        assert!(Config::parse("bogus x").is_err());
        assert!(Config::parse("token short").is_err());
    }

    #[test]
    fn claude_label_strips_home() {
        let enc = encode_project_path(&home());
        let p = PathBuf::from(format!("/x/.claude/projects/{enc}-projects-webshop/memory"));
        assert_eq!(claude_label(&p), "projects-webshop");
        let p = PathBuf::from(format!("/x/.claude/projects/{enc}/memory"));
        assert_eq!(claude_label(&p), "home");
    }

    #[test]
    fn encodes_like_claude_code() {
        assert_eq!(
            encode_project_path(Path::new("/home/a/.herdr/x_y")),
            "-home-a--herdr-x-y"
        );
        assert_eq!(encode_project_path(Path::new("C:\\Users\\a\\proj")), "C--Users-a-proj");
    }
}
