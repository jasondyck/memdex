//! Claude Code session transcripts (`~/.claude/projects/<project>/<session>.jsonl`) as a
//! second, separate index ("history"), so curated notes keep their own ranking.
//!
//! Only the conversation is indexed — the user's prompts and Claude's text replies,
//! about 3% of a typical transcript. Tool calls, tool results (file dumps, command
//! output), thinking, sidechains and injected system text are skipped. Each prompt plus
//! the replies to it becomes one section ("exchange").
//!
//! Transcripts are append-only and can be hundreds of MB, so each file is read
//! incrementally from the last offset; only the still-open last exchange is re-indexed.

use crate::fts::{Doc, Fts, Phrase};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Transcript ids live above this so they never collide with note ids.
pub const ID_BASE: i64 = 1 << 40;
/// Long exchanges are capped (the start of the reply carries most of the signal).
const MAX_EXCHANGE: usize = 24_000;
/// Injected, non-conversational blocks Claude Code wraps in tags.
const DROP_TAGS: &[&str] = &[
    "system-reminder",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "command-message",
    "command-name",
    "bash-stdout",
    "bash-stderr",
    "task-notification",
];

#[derive(Default, Clone)]
struct Exchange {
    when: String,
    prompt: String,
    reply: String,
}

#[derive(Default)]
struct FileState {
    offset: u64,
    mtime: Option<SystemTime>,
    /// The exchange still being written to, and its id in the index.
    open: Option<(Exchange, i64)>,
}

#[derive(Default)]
pub struct Transcripts {
    fts: Fts,
    files: HashMap<PathBuf, FileState>,
    /// `.claude/projects/<dir>` → project label (same labels as the memory notes).
    labels: HashMap<PathBuf, String>,
    /// Transcript files seen on disk at the last refresh.
    on_disk: usize,
    /// Files that couldn't be opened (permissions, races with deletion).
    unreadable: std::collections::HashSet<PathBuf>,
}

/// What the history index holds, for `memdex stats`.
pub struct HistStats {
    pub on_disk: usize,
    pub read: usize,
    pub unreadable: usize,
    pub with_conversation: usize,
    pub exchanges: usize,
    pub bytes_scanned: u64,
}

fn strip_tags(text: &str) -> String {
    let mut out = text.to_string();
    for tag in DROP_TAGS {
        let (open, close) = (format!("<{tag}"), format!("</{tag}>"));
        while let Some(a) = out.find(&open) {
            match out[a..].find(&close) {
                Some(b) => out.replace_range(a..a + b + close.len(), ""),
                None => {
                    out.truncate(a);
                    break;
                }
            }
        }
    }
    // Pasted text is the user's input: keep it, drop just the wrapper tags.
    while let Some(a) = out.find("<pasted_content") {
        match out[a..].find('>') {
            Some(b) => out.replace_range(a..a + b + 1, ""),
            None => break,
        }
    }
    out.replace("</pasted_content>", "").trim().to_string()
}

/// The user's own words in a transcript line, if it is a real prompt.
fn prompt_text(e: &Value) -> Option<String> {
    if e["type"] != "user" || e["isMeta"] == true || e["isSidechain"] == true {
        return None;
    }
    let text = match &e["message"]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = strip_tags(&text);
    (!text.is_empty() && !text.starts_with("[Request interrupted")).then_some(text)
}

fn reply_text(e: &Value) -> Option<String> {
    if e["type"] != "assistant" || e["isSidechain"] == true {
        return None;
    }
    let text: Vec<&str> = e["message"]["content"]
        .as_array()?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// Cheap pre-filter: most bytes are tool output (file dumps, command logs) that never
/// needs a JSON parse. Only skips lines that are clearly tool traffic, independent of
/// JSON formatting; anything else is parsed and checked properly.
fn may_matter(line: &str) -> bool {
    if line.contains("\"tool_result\"") {
        return false; // tool output is never a prompt or a reply
    }
    if line.contains("\"tool_use\"") && !line.contains("\"text\"") {
        return false; // a tool call with no text block
    }
    line.contains("\"user\"") || line.contains("\"assistant\"")
}

fn add_exchange(fts: &mut Fts, key: &str, label: &str, file: &str, x: &Exchange) -> i64 {
    let prompt: String = x.prompt.chars().take(70).collect();
    let heading = format!("{} · {}", x.when, prompt.replace('\n', " "));
    let mut body = format!("User: {}\n\nClaude: {}", x.prompt, x.reply);
    if body.len() > MAX_EXCHANGE {
        let cut = (0..=MAX_EXCHANGE)
            .rev()
            .find(|i| body.is_char_boundary(*i))
            .unwrap_or(0);
        body.truncate(cut);
        body.push('…');
    }
    fts.insert(key, label, file, &heading, "", &body)
}

impl Transcripts {
    /// Pick up new / grown / removed transcripts under `roots` (each a `.claude/projects/<dir>`).
    pub fn refresh(&mut self, roots: &[(PathBuf, String)]) {
        for (dir, label) in roots {
            self.labels.insert(dir.clone(), label.clone());
        }
        let mut present = Vec::new();
        for (dir, _) in roots {
            let Ok(rd) = std::fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "jsonl") {
                    present.push((p, e.metadata().ok()));
                }
            }
        }
        self.on_disk = present.len();
        let keep: std::collections::HashSet<&PathBuf> = present.iter().map(|(p, _)| p).collect();
        let gone: Vec<PathBuf> = self.files.keys().filter(|p| !keep.contains(p)).cloned().collect();
        for p in gone {
            self.fts.remove_path(&p.to_string_lossy());
            self.files.remove(&p);
        }
        for (p, meta) in present {
            let (len, mtime) = meta.map(|m| (m.len(), m.modified().ok())).unwrap_or((0, None));
            let st = self.files.entry(p.clone()).or_default();
            if st.mtime == mtime && st.offset == len {
                continue;
            }
            if len < st.offset {
                // Rewritten or truncated: start over for this file.
                self.fts.remove_path(&p.to_string_lossy());
                *st = FileState::default();
            }
            self.read_new(&p, mtime);
        }
    }

    fn read_new(&mut self, path: &Path, mtime: Option<SystemTime>) {
        let label = path
            .parent()
            .and_then(|d| self.labels.get(d))
            .cloned()
            .unwrap_or_default();
        let session = path
            .file_stem()
            .map(|s| s.to_string_lossy().chars().take(8).collect::<String>())
            .unwrap_or_default();
        let key = path.to_string_lossy().into_owned();
        let Ok(mut f) = File::open(path) else {
            self.unreadable.insert(path.to_path_buf());
            return;
        };
        self.unreadable.remove(path);
        let st = self.files.get_mut(path).expect("state exists");
        if f.seek(SeekFrom::Start(st.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::new(f);
        let mut open = st.open.take();
        let mut finished: Vec<Exchange> = Vec::new();
        let mut consumed = 0u64;
        let mut line = String::new();
        loop {
            line.clear();
            let Ok(n) = reader.read_line(&mut line) else { break };
            if n == 0 || !line.ends_with('\n') {
                break; // EOF, or a line still being written: re-read it next time
            }
            consumed += n as u64;
            if !may_matter(&line) {
                continue;
            }
            let Ok(e) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(p) = prompt_text(&e) {
                if let Some((x, id)) = open.take() {
                    self.fts.remove_id(id);
                    finished.push(x);
                }
                let when = e["timestamp"].as_str().unwrap_or("").chars().take(10).collect();
                open = Some((
                    Exchange {
                        when,
                        prompt: p,
                        reply: String::new(),
                    },
                    0,
                ));
            } else if let Some(r) = reply_text(&e)
                && let Some((x, _)) = open.as_mut()
            {
                if !x.reply.is_empty() {
                    x.reply.push('\n');
                }
                x.reply.push_str(&r);
            }
        }
        let file = format!("session-{session}");
        for x in &finished {
            add_exchange(&mut self.fts, &key, &label, &file, x);
        }
        if let Some((x, old)) = open {
            if old != 0 {
                self.fts.remove_id(old);
            }
            let id = add_exchange(&mut self.fts, &key, &label, &file, &x);
            open = Some((x, id));
        }
        let st = self.files.get_mut(path).expect("state exists");
        st.offset += consumed;
        st.mtime = mtime;
        st.open = open;
    }

    pub fn search(&self, phrases: &[Phrase], w: [f64; 4], project: Option<&str>, limit: usize) -> Vec<i64> {
        self.fts
            .search(phrases, w, project, limit)
            .into_iter()
            .map(|id| id + ID_BASE)
            .collect()
    }

    pub fn substring(&self, terms: &[String], project: Option<&str>, limit: usize) -> Vec<i64> {
        self.fts
            .substring(terms, project, limit)
            .into_iter()
            .map(|id| id + ID_BASE)
            .collect()
    }

    pub fn get(&self, id: i64) -> Option<&Doc> {
        self.fts.get(id - ID_BASE)
    }

    pub fn sessions(&self) -> usize {
        self.files.len()
    }

    pub fn stats(&self) -> HistStats {
        HistStats {
            on_disk: self.on_disk,
            read: self.files.len() - self.unreadable.len(),
            unreadable: self.unreadable.len(),
            with_conversation: self.fts.path_count(),
            exchanges: self.fts.len(),
            bytes_scanned: self.files.values().map(|s| s.offset).sum(),
        }
    }

    pub fn exchanges(&self) -> usize {
        self.fts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(v: serde_json::Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn indexes_conversation_incrementally() {
        let root = std::env::temp_dir().join(format!("memdex-tx-{}", std::process::id()));
        let proj = root.join("-x-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join("abcdef12-0000.jsonl");
        let mut f = File::create(&path).unwrap();
        let ts = "2026-01-02T03:04:05Z";
        // Pretty JSON (spaces after colons) must work too: the filter can't rely on compact output.
        f.write_all(format!("{}\n", serde_json::to_string_pretty(&serde_json::json!({"type":"user","timestamp":ts,"message":{"role":"user","content":"pretty formatted prompt about yaks"}})).unwrap().replace('\n', " ")).as_bytes()).unwrap();
        f.write_all(line(serde_json::json!({"type":"user","timestamp":ts,"message":{"role":"user","content":"how do I rotate the staging db password?"}})).as_bytes()).unwrap();
        f.write_all(line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cat secrets"}}]}})).as_bytes()).unwrap();
        f.write_all(line(serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"zebra-tool-output"}]}})).as_bytes()).unwrap();
        f.write_all(line(serde_json::json!({"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>meta</local-command-caveat>"}})).as_bytes()).unwrap();
        f.write_all(line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Use the vault CLI, then restart the app."}]}})).as_bytes()).unwrap();
        drop(f);

        let roots = vec![(proj.clone(), "proj".to_string())];
        let mut t = Transcripts::default();
        t.refresh(&roots);
        let q = |t: &Transcripts, w: &str| {
            t.search(
                &[Phrase {
                    term: crate::porter::stem(w),
                    prefix: true,
                }],
                [4.0, 3.0, 1.0, 1.0],
                None,
                5,
            )
        };
        let hit = q(&t, "vault");
        assert_eq!(hit.len(), 1);
        let d = t.get(hit[0]).unwrap();
        assert_eq!((d.project.as_str(), d.file.as_str()), ("proj", "session-abcdef12"));
        assert!(d.heading.starts_with("2026-01-02 · how do I rotate"));
        assert_eq!(q(&t, "yaks").len(), 1, "pretty-printed JSON lines are indexed");
        assert!(q(&t, "zebra").is_empty(), "tool output must not be indexed");
        assert!(q(&t, "caveat").is_empty(), "meta lines must not be indexed");

        // Appending: the open exchange grows in place, a new prompt starts another.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Also update the kubernetes secret."}]}})).as_bytes()).unwrap();
        f.write_all(line(serde_json::json!({"type":"user","timestamp":ts,"message":{"role":"user","content":"thanks, what about the build cache?"}})).as_bytes()).unwrap();
        drop(f);
        t.refresh(&roots);
        assert_eq!(t.exchanges(), 3);
        let k = q(&t, "kubernetes");
        assert_eq!(k.len(), 1);
        assert!(
            t.get(k[0]).unwrap().body.contains("vault CLI"),
            "same exchange, extended"
        );
        assert_eq!(q(&t, "cache").len(), 1);
        drop(t);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn strips_injected_tags() {
        assert_eq!(
            strip_tags("hi <system-reminder>x\ny</system-reminder> there"),
            "hi  there"
        );
        assert_eq!(strip_tags("<command-name>/clear</command-name>"), "");
        assert_eq!(strip_tags("cut <system-reminder>never closed"), "cut");
        assert_eq!(
            strip_tags("<pasted_content id=\"x1\">log line</pasted_content> what broke?"),
            "log line what broke?"
        );
    }
}
