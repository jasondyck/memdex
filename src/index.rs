//! In-memory SQLite FTS5 index over Claude Code memory markdown files.
//!
//! The markdown files stay the source of truth: nothing is written back, and the
//! index lives only in RAM. Every query first re-stats the files and re-indexes
//! any that changed, so results never go stale.

use crate::embed::{Embedder, dot};
use rusqlite::{Connection, params};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Ranking / chunking knobs. Defaults are the benchmarked best; `memdex bench
/// --tune "w=4,3,1 prefix=1 …"` overrides them for experiments.
#[derive(Clone, Debug)]
pub struct Tuning {
    /// BM25 column weights: note name, heading, body.
    pub w: (f64, f64, f64),
    /// Match word prefixes (`tailscale` also hits `tailscaled`).
    pub prefix: bool,
    /// Drop common English words from queries.
    pub stop: bool,
    /// Max sections from one note in a result list.
    pub per_note: usize,
    /// Sections longer than `max_chunk` are split into pieces of ~`target_chunk`.
    pub max_chunk: usize,
    pub target_chunk: usize,
    /// BM25 weight of the hidden context column (the note's name + description,
    /// indexed with every section but never shown). 0 disables it.
    pub wc: f64,
    /// Snippets show the lines that match the query instead of the section start.
    pub focus: bool,
    /// Weight of the embedding ranking in reciprocal-rank fusion with BM25 (0 = off).
    pub sem: f64,
    /// RRF constant k: higher flattens rank differences.
    pub rrf: f64,
    /// Passage size (chars) for embeddings; a section's score is its best passage.
    pub pass: usize,
    /// Snippet line choice also uses embedding similarity (ties after word matches).
    pub sfocus: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        // Chosen 2026-11-02 by sweeping 768 configs on 94 tuning questions and confirming on
        // 43 untouched held-out ones (bench/README): note-sized chunks rank best once
        // snippets are query-focused; the hidden context column adds a little MRR.
        Tuning {
            w: (4.0, 3.0, 1.0),
            prefix: true,
            stop: true,
            per_note: 2,
            max_chunk: 4000,
            target_chunk: 3000,
            wc: 1.0,
            focus: true,
            sem: 0.0,
            rrf: 60.0,
            pass: 400,
            sfocus: false,
        }
    }
}

impl Tuning {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut t = Tuning::default();
        for kv in spec.split_whitespace() {
            let (k, v) = kv.split_once('=').ok_or(format!("bad tune `{kv}`"))?;
            let nums: Vec<f64> = v
                .split(',')
                .map(|x| x.parse::<f64>().map_err(|e| format!("{kv}: {e}")))
                .collect::<Result<_, _>>()?;
            let b = nums.first().copied().unwrap_or(0.0) != 0.0;
            match (k, nums.len()) {
                ("w", 3) => t.w = (nums[0], nums[1], nums[2]),
                ("prefix", 1) => t.prefix = b,
                ("stop", 1) => t.stop = b,
                ("per", 1) => t.per_note = nums[0] as usize,
                ("chunk", 2) => (t.max_chunk, t.target_chunk) = (nums[0] as usize, nums[1] as usize),
                ("wc", 1) => t.wc = nums[0],
                ("focus", 1) => t.focus = b,
                ("sem", 1) => t.sem = nums[0],
                ("rrf", 1) => t.rrf = nums[0],
                ("pass", 1) => t.pass = nums[0] as usize,
                ("sfocus", 1) => t.sfocus = b,
                _ => return Err(format!("bad tune `{kv}`")),
            }
        }
        Ok(t)
    }
}

pub struct Chunk {
    pub heading: String,
    /// Note-level context (name + description), searchable but not displayed.
    pub ctx: String,
    pub body: String,
}

pub struct Hit {
    pub id: i64,
    pub project: String,
    pub file: String,
    pub heading: String,
    pub body: String,
}

pub struct Index {
    conn: Connection,
    /// (directory, label) pairs; the label is what results show as `project`.
    dirs: Vec<(PathBuf, String)>,
    /// Note names (`stem`) or `label/stem` this index must never expose.
    exclude: Vec<String>,
    seen: HashMap<PathBuf, SystemTime>,
    pub tune: Tuning,
    emb: Option<Arc<Embedder>>,
    /// Pick up newly created ~/.claude/projects/*/memory dirs on every refresh.
    pub watch_claude: bool,
    /// Passage embeddings per file: (chunk rowid, unit vector).
    vecs: HashMap<PathBuf, Vec<(i64, Vec<f32>)>>,
}

impl Index {
    pub fn new(dirs: Vec<(PathBuf, String)>, exclude: Vec<String>) -> rusqlite::Result<Self> {
        Self::with_tuning(dirs, exclude, Tuning::default())
    }

    pub fn with_tuning(dirs: Vec<(PathBuf, String)>, exclude: Vec<String>, tune: Tuning) -> rusqlite::Result<Self> {
        Self::with_model(dirs, exclude, tune, None)
    }

    pub fn with_model(
        dirs: Vec<(PathBuf, String)>,
        exclude: Vec<String>,
        tune: Tuning,
        emb: Option<Arc<Embedder>>,
    ) -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE chunks USING fts5(
                 path UNINDEXED, project UNINDEXED, file, heading, ctx, body,
                 tokenize = 'porter unicode61 remove_diacritics 2');
             CREATE VIRTUAL TABLE tri USING fts5(body, tokenize = 'trigram');",
        )?;
        let mut idx = Index {
            conn,
            dirs,
            exclude,
            seen: HashMap::new(),
            tune,
            emb,
            vecs: HashMap::new(),
            watch_claude: false,
        };
        idx.refresh()?;
        Ok(idx)
    }

    /// Label of the configured directory holding `p`.
    pub fn label_of(&self, p: &Path) -> String {
        let parent = p.parent();
        self.dirs
            .iter()
            .find(|(d, _)| Some(d.as_path()) == parent)
            .map(|(_, l)| l.clone())
            .unwrap_or_default()
    }

    fn excluded(&self, label: &str, stem: &str) -> bool {
        self.exclude
            .iter()
            .any(|e| e == stem || *e == format!("{label}/{stem}"))
    }

    /// Every visible `*.md` note in the configured directories (non-recursive).
    fn files(&self) -> Vec<(PathBuf, SystemTime)> {
        let mut out = Vec::new();
        for (dir, label) in &self.dirs {
            let Ok(rd) = fs::read_dir(dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // MEMORY.md is the index Claude Code already loads into every session.
                if !name.ends_with(".md") || name == "MEMORY.md" || name == "README.md" || name.starts_with('.') {
                    continue;
                }
                if self.excluded(label, &stem(&p)) {
                    continue;
                }
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    out.push((p, m));
                }
            }
        }
        out
    }

    /// Re-index files that were added, changed or removed since the last call.
    pub fn refresh(&mut self) -> rusqlite::Result<usize> {
        if self.watch_claude {
            for (d, l) in crate::config::claude_dirs() {
                if !self.dirs.iter().any(|(x, _)| *x == d) {
                    self.dirs.push((d, l));
                }
            }
        }
        let current: HashMap<PathBuf, SystemTime> = self.files().into_iter().collect();
        let mut changed = 0;
        let removed: Vec<PathBuf> = self
            .seen
            .keys()
            .filter(|p| !current.contains_key(*p))
            .cloned()
            .collect();
        let labels: HashMap<PathBuf, String> = current.keys().map(|p| (p.clone(), self.label_of(p))).collect();
        let tx = self.conn.transaction()?;
        for p in &removed {
            delete_path(&tx, p)?;
            self.vecs.remove(p);
            changed += 1;
        }
        for (p, mtime) in &current {
            if self.seen.get(p) == Some(mtime) {
                continue;
            }
            // Deleting scans the FTS table (path is UNINDEXED), so only do it for
            // files indexed before — otherwise the first build is quadratic.
            if self.seen.contains_key(p) {
                delete_path(&tx, p)?;
            }
            if let Ok(text) = fs::read_to_string(p) {
                let file = stem(p);
                let project = &labels[p];
                let path = p.to_string_lossy();
                let mut ins_chunk = tx.prepare_cached(
                    "INSERT INTO chunks(path, project, file, heading, ctx, body) VALUES (?1,?2,?3,?4,?5,?6)",
                )?;
                let mut ins_tri = tx.prepare_cached("INSERT INTO tri(rowid, body) VALUES (?1, ?2)")?;
                let mut pv = Vec::new();
                for c in chunk_markdown_with(&text, &self.tune) {
                    let id = ins_chunk.insert(params![path, project, file, c.heading, c.ctx, c.body])?;
                    ins_tri.execute(params![id, c.body])?;
                    if let Some(e) = &self.emb {
                        for t in passages(&file, &c, self.tune.pass) {
                            if let Some(v) = e.embed(&t) {
                                pv.push((id, v));
                            }
                        }
                    }
                }
                self.vecs.insert(p.clone(), pv);
            } else {
                self.vecs.remove(p);
            }
            changed += 1;
        }
        tx.commit()?;
        for p in removed {
            self.seen.remove(&p);
        }
        self.seen.extend(current);
        Ok(changed)
    }

    pub fn file_count(&self) -> usize {
        self.seen.len()
    }

    /// Hybrid search: BM25 over words (+ trigram fallback for identifiers like IPs),
    /// fused with embedding similarity when a model is loaded, then at most
    /// `per_note` sections per note.
    pub fn search(&mut self, query: &str, limit: usize, project: Option<&str>) -> rusqlite::Result<Vec<Hit>> {
        const POOL: usize = 40;
        self.refresh()?;
        let mut bm: Vec<Hit> = Vec::new();
        if let Some(q) = word_query_with(query, self.tune.prefix, self.tune.stop) {
            let ((wf, wh, wb), wc) = (self.tune.w, self.tune.wc);
            let mut st = self.conn.prepare_cached(&format!(
                "SELECT rowid, project, file, heading, body FROM chunks
                 WHERE chunks MATCH ?1 AND (?2 IS NULL OR project LIKE '%' || ?2 || '%')
                 ORDER BY bm25(chunks, 0, 0, {wf:.3}, {wh:.3}, {wc:.3}, {wb:.3}) LIMIT ?3"
            ))?;
            bm.extend(st.query_map(params![q, project, POOL as i64], row_to_hit)?.flatten());
        }
        if bm.len() < limit
            && let Some(q) = trigram_query(query)
        {
            let mut st = self.conn.prepare_cached(
                "SELECT c.rowid, c.project, c.file, c.heading, c.body
                 FROM tri JOIN chunks c ON c.rowid = tri.rowid
                 WHERE tri MATCH ?1 AND (?2 IS NULL OR c.project LIKE '%' || ?2 || '%')
                 ORDER BY bm25(tri) LIMIT ?3",
            )?;
            for h in st.query_map(params![q, project, POOL as i64], row_to_hit)?.flatten() {
                if !bm.iter().any(|x| x.id == h.id) {
                    bm.push(h);
                }
            }
        }
        let ranked = match self.semantic(query, project, POOL) {
            Some(sem) => {
                // Reciprocal-rank fusion: robust to the two scores' different scales.
                let k = self.tune.rrf;
                let mut score: HashMap<i64, f64> = HashMap::new();
                for (r, h) in bm.iter().enumerate() {
                    *score.entry(h.id).or_default() += 1.0 / (k + r as f64 + 1.0);
                }
                for (r, (id, _)) in sem.iter().enumerate() {
                    *score.entry(*id).or_default() += self.tune.sem / (k + r as f64 + 1.0);
                }
                let mut ids: Vec<(i64, f64)> = score.into_iter().collect();
                ids.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
                let mut by_id: HashMap<i64, Hit> = bm.into_iter().map(|h| (h.id, h)).collect();
                let mut out = Vec::new();
                for (id, _) in ids.into_iter().take(POOL) {
                    match by_id.remove(&id) {
                        Some(h) => out.push(h),
                        None => out.extend(self.chunk_row(id)?),
                    }
                }
                out
            }
            None => bm,
        };
        // Keep at most `per_note` sections per note so one long note can't
        // crowd every other note out of the results.
        let mut per: HashMap<String, usize> = HashMap::new();
        let mut hits = Vec::new();
        for h in ranked {
            let n = per.entry(format!("{}/{}", h.project, h.file)).or_default();
            if *n < self.tune.per_note {
                *n += 1;
                hits.push(h);
                if hits.len() == limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    /// Sections ranked by their best passage's cosine similarity to the query.
    fn semantic(&self, query: &str, project: Option<&str>, n: usize) -> Option<Vec<(i64, f32)>> {
        if self.tune.sem <= 0.0 {
            return None;
        }
        let q = self.emb.as_ref()?.embed(query)?;
        let mut best: HashMap<i64, f32> = HashMap::new();
        for (p, vs) in &self.vecs {
            if project.is_some_and(|pr| !self.label_of(p).contains(pr)) {
                continue;
            }
            for (id, v) in vs {
                let s = dot(&q, v);
                let b = best.entry(*id).or_insert(f32::MIN);
                if s > *b {
                    *b = s;
                }
            }
        }
        let mut v: Vec<(i64, f32)> = best.into_iter().collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        Some(v)
    }

    fn chunk_row(&self, id: i64) -> rusqlite::Result<Option<Hit>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT rowid, project, file, heading, body FROM chunks WHERE rowid = ?1")?;
        let mut rows = st.query_map(params![id], row_to_hit)?;
        rows.next().transpose()
    }

    /// What an agent sees for one hit: query-focused lines when `focus` is on
    /// (word matches first, then — with `sfocus` — embedding similarity).
    pub fn snippet(&self, body: &str, query: &str, budget: usize) -> String {
        if !self.tune.focus {
            let cut: String = body.chars().take(budget).collect();
            return if cut.len() < body.len() {
                format!("{cut}…")
            } else {
                cut
            };
        }
        let qv = if self.tune.sfocus {
            self.emb.as_ref().and_then(|e| e.embed(query).map(|v| (e, v)))
        } else {
            None
        };
        match qv {
            Some((e, v)) => focus_snippet_with(body, query, budget, &|l: &str| {
                e.embed(l).map(|lv| dot(&v, &lv)).unwrap_or(0.0)
            }),
            None => focus_snippet(body, query, budget),
        }
    }

    pub fn chunk(&mut self, id: i64) -> rusqlite::Result<Option<Hit>> {
        self.refresh()?;
        let mut st = self
            .conn
            .prepare("SELECT rowid, project, file, heading, body FROM chunks WHERE rowid = ?1")?;
        let mut rows = st.query_map(params![id], row_to_hit)?;
        rows.next().transpose()
    }

    /// Locate a memory file by name (with or without `.md`), optionally scoped to a project.
    pub fn find_file(&self, name: &str, project: Option<&str>) -> Result<PathBuf, String> {
        if name.contains('/') || name.contains("..") {
            return Err("file must be a bare name like `release-checklist`".into());
        }
        let want = name.strip_suffix(".md").unwrap_or(name);
        let mut found: Vec<&PathBuf> = self
            .seen
            .keys()
            .filter(|p| stem(p) == want)
            .filter(|p| project.is_none_or(|pr| self.label_of(p).contains(pr)))
            .collect();
        found.sort();
        match found.len() {
            0 => Err(format!("no memory file named `{want}`")),
            1 => Ok(found[0].clone()),
            _ => Err(format!(
                "`{want}` exists in several projects ({}); pass `project`",
                found.iter().map(|p| self.label_of(p)).collect::<Vec<_>>().join(", ")
            )),
        }
    }
}

/// Texts embedded for one section: note name + heading as context, then the body
/// in pieces of ~`pass` chars at line boundaries.
fn passages(file: &str, c: &Chunk, pass: usize) -> Vec<String> {
    let prefix = format!("{} {}", file.replace(['-', '_'], " "), c.heading);
    let pass = if pass == 0 { usize::MAX } else { pass };
    let mut out = Vec::new();
    let mut cur = String::new();
    for l in c.body.lines().filter(|l| !l.trim().is_empty()) {
        if !cur.is_empty() && cur.len() + l.len() > pass {
            out.push(format!("{prefix}\n{cur}"));
            cur.clear();
        }
        cur.push_str(l);
        cur.push('\n');
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(format!("{prefix}\n{cur}"));
    }
    out
}

fn row_to_hit(r: &rusqlite::Row) -> rusqlite::Result<Hit> {
    Ok(Hit {
        id: r.get(0)?,
        project: r.get(1)?,
        file: r.get(2)?,
        heading: r.get(3)?,
        body: r.get(4)?,
    })
}

fn delete_path(conn: &Connection, p: &Path) -> rusqlite::Result<()> {
    let path = p.to_string_lossy();
    conn.execute(
        "DELETE FROM tri WHERE rowid IN (SELECT rowid FROM chunks WHERE path = ?1)",
        params![path],
    )?;
    conn.execute("DELETE FROM chunks WHERE path = ?1", params![path])?;
    Ok(())
}

fn stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "for", "from", "how", "i", "in", "is", "it", "me",
    "my", "of", "on", "or", "that", "the", "this", "to", "was", "what", "when", "where", "which", "who", "why", "with",
    "you", "your", "we", "our", "can", "should", "about", "use", "used",
];

/// Free text → FTS5 query: each meaningful word as a quoted prefix term, OR-ed so
/// BM25 ranks chunks matching more of the words higher.
#[cfg(test)]
pub fn word_query(q: &str) -> Option<String> {
    word_query_with(q, true, true)
}

pub fn word_query_with(q: &str, prefix: bool, stop: bool) -> Option<String> {
    let mut terms: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|t| !(t.is_empty() || stop && STOPWORDS.contains(&t.as_str())))
        .collect();
    terms.dedup();
    if terms.is_empty() {
        return None;
    }
    let star = if prefix { "*" } else { "" };
    Some(
        terms
            .iter()
            .map(|t| format!("\"{t}\"{star}"))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// Raw whitespace-separated terms (≥3 chars) for the trigram table, so
/// `203.0.113.7` or `build-7f3a` match as substrings.
pub fn trigram_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| "?!,;:()[]{}'\"`".contains(c)))
        .filter(|t| t.chars().count() >= 3 && !STOPWORDS.contains(&t.to_lowercase().as_str()))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Lowercased query words (stopwords dropped), cut to a 5-char stem so
/// `migrations` still finds `migration`. Used to pick snippet lines.
fn query_stems(q: &str) -> Vec<String> {
    let mut v: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(&t.as_str()))
        .map(|t| t.chars().take(5).collect())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Up to `budget` chars of `body`, preferring the lines that contain the most
/// query words; kept in original order with `…` marking gaps.
pub fn focus_snippet(body: &str, query: &str, budget: usize) -> String {
    focus_snippet_with(body, query, budget, &|_| 0.0)
}

/// `sim` breaks ties between lines with equal word matches (e.g. embedding similarity).
pub fn focus_snippet_with(body: &str, query: &str, budget: usize, sim: &dyn Fn(&str) -> f32) -> String {
    if body.chars().count() <= budget {
        return body.to_string();
    }
    let stems = query_stems(query);
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    let score = |l: &str| {
        let low = l.to_lowercase();
        stems.iter().filter(|s| low.contains(s.as_str())).count()
    };
    let mut order: Vec<usize> = (0..lines.len()).collect();
    let sims: Vec<i64> = lines.iter().map(|l| (sim(l) * 1e6) as i64).collect();
    order.sort_by_key(|&i| (std::cmp::Reverse(score(lines[i])), std::cmp::Reverse(sims[i]), i));
    let mut keep = vec![false; lines.len()];
    let mut used = 0;
    for &i in &order {
        let len = lines[i].chars().count() + 1;
        if used + len <= budget {
            keep[i] = true;
            used += len;
        } else if used == 0 {
            // A single over-long line: centre a window on its first match.
            let low = lines[i].to_lowercase();
            let at = stems.iter().filter_map(|s| low.find(s.as_str())).min().unwrap_or(0);
            let chars: Vec<char> = lines[i].chars().collect();
            let char_at = lines[i][..at.min(lines[i].len())].chars().count();
            let start = char_at
                .saturating_sub(budget / 3)
                .min(chars.len().saturating_sub(budget));
            let end = (start + budget).min(chars.len());
            let pre = if start > 0 { "…" } else { "" };
            let post = if end < chars.len() { "…" } else { "" };
            return format!("{pre}{}{post}", chars[start..end].iter().collect::<String>());
        }
    }
    let mut out = String::new();
    let mut prev_kept = true;
    for (i, l) in lines.iter().enumerate() {
        if keep[i] {
            if !prev_kept || (i > 0 && out.is_empty()) {
                out.push_str("…\n");
            }
            out.push_str(l);
            out.push('\n');
        }
        prev_kept = keep[i];
    }
    if !prev_kept {
        out.push('…');
    }
    out.trim_end().to_string()
}

/// Split a memory note into searchable sections. The frontmatter `name` and
/// `description` become a summary chunk; each heading starts a new chunk; long
/// sections are split at blank lines or top-level bullets.
#[cfg(test)]
pub fn chunk_markdown(text: &str) -> Vec<Chunk> {
    chunk_markdown_with(text, &Tuning::default())
}

pub fn chunk_markdown_with(text: &str, t: &Tuning) -> Vec<Chunk> {
    let (meta, body) = split_frontmatter(text);
    let mut out = Vec::new();
    let mut ctx = String::new();
    if let Some((name, desc)) = meta {
        ctx = format!("{name}: {desc}");
        out.push(Chunk {
            heading: "(summary)".into(),
            ctx: String::new(),
            body: ctx.clone(),
        });
    }
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut cur = String::new();
    let flush = |stack: &[(usize, String)], cur: &mut String, out: &mut Vec<Chunk>| {
        let heading = stack.iter().map(|(_, h)| h.as_str()).collect::<Vec<_>>().join(" > ");
        for piece in split_long(cur.trim(), t.max_chunk, t.target_chunk) {
            out.push(Chunk {
                heading: heading.clone(),
                ctx: ctx.clone(),
                body: piece,
            });
        }
        cur.clear();
    };
    let mut in_code = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let level = line.chars().take_while(|&c| c == '#').count();
        if !in_code && (1..=6).contains(&level) && line[level..].starts_with(' ') {
            flush(&stack, &mut cur, &mut out);
            stack.retain(|(l, _)| *l < level);
            stack.push((level, line[level..].trim().to_string()));
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    flush(&stack, &mut cur, &mut out);
    out
}

fn split_frontmatter(text: &str) -> (Option<(String, String)>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text);
    };
    let fm = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('-').trim_start_matches('\n');
    let get = |key: &str| {
        fm.lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(key)
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
            .unwrap_or_default()
    };
    let (name, desc) = (get("name:"), get("description:"));
    if name.is_empty() && desc.is_empty() {
        (None, body)
    } else {
        (Some((name, desc)), body)
    }
}

/// Split at paragraph breaks or top-level bullets, packing pieces up to `target`.
fn split_long(s: &str, max: usize, target: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![];
    }
    if s.len() <= max {
        return vec![s.to_string()];
    }
    let mut blocks: Vec<String> = Vec::new();
    let mut b = String::new();
    for line in s.lines() {
        let boundary =
            line.trim().is_empty() || line.starts_with("- ") || line.starts_with("* ") || line.starts_with("**");
        if boundary && !b.trim().is_empty() {
            blocks.push(std::mem::take(&mut b));
        }
        if !line.trim().is_empty() {
            b.push_str(line);
            b.push('\n');
        }
    }
    if !b.trim().is_empty() {
        blocks.push(b);
    }
    let mut out = Vec::new();
    let mut acc = String::new();
    for blk in blocks {
        if !acc.is_empty() && acc.len() + blk.len() > target {
            out.push(acc.trim().to_string());
            acc.clear();
        }
        acc.push_str(&blk);
    }
    if !acc.trim().is_empty() {
        out.push(acc.trim().to_string());
    }
    out
}

/// Return a file's text without frontmatter, or only the sections whose heading
/// contains `section` (case-insensitive), each including its sub-sections.
pub fn read_file(path: &Path, section: Option<&str>) -> Result<String, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (_, body) = split_frontmatter(&text);
    let Some(sec) = section.map(str::to_lowercase) else {
        return Ok(body.to_string());
    };
    let mut out = String::new();
    let mut take_level: Option<usize> = None;
    let mut in_code = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let level = line.chars().take_while(|&c| c == '#').count();
        let is_heading = !in_code && (1..=6).contains(&level) && line[level..].starts_with(' ');
        if is_heading {
            if take_level.is_some_and(|l| level <= l) {
                take_level = None;
            }
            if take_level.is_none() && line.to_lowercase().contains(&sec) {
                take_level = Some(level);
            }
        }
        if take_level.is_some() {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    // Most notes use **bold lead-ins** instead of headings: fall back to the
    // paragraphs / top-level bullets that mention the term.
    let blocks: Vec<String> = split_long_all(body)
        .into_iter()
        .filter(|b| b.to_lowercase().contains(&sec))
        .collect();
    if blocks.is_empty() {
        Err(format!("nothing in this file mentions `{sec}`"))
    } else {
        Ok(blocks.join("\n…\n"))
    }
}

/// Every paragraph / top-level bullet block, unpacked (no size packing).
fn split_long_all(s: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut b = String::new();
    for line in s.lines() {
        let boundary =
            line.trim().is_empty() || line.starts_with("- ") || line.starts_with("* ") || line.starts_with("**");
        if boundary && !b.trim().is_empty() {
            blocks.push(std::mem::take(&mut b).trim().to_string());
        }
        if !line.trim().is_empty() {
            b.push_str(line);
            b.push('\n');
        }
    }
    if !b.trim().is_empty() {
        blocks.push(b.trim().to_string());
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_becomes_summary_and_headings_nest() {
        let md = "---\nname: db\ndescription: Database notes\nmetadata:\n  type: project\n---\n\nintro\n# A\none\n## B\ntwo\n# C\nthree\n";
        let c = chunk_markdown(md);
        assert_eq!(c[0].heading, "(summary)");
        assert_eq!(c[0].body, "db: Database notes");
        assert_eq!(c[1].body, "intro");
        assert_eq!(c[2].heading, "A");
        assert_eq!(c[3].heading, "A > B");
        assert_eq!(c[4].heading, "C");
    }

    #[test]
    fn hashes_in_code_blocks_are_not_headings() {
        let c = chunk_markdown("# Real\n```\n# comment\n```\n");
        assert_eq!(c.len(), 1);
        assert!(c[0].body.contains("# comment"));
    }

    #[test]
    fn long_sections_split_at_bullets() {
        let body: String = (0..40)
            .map(|i| format!("- bullet number {i} with some padding text here\n"))
            .collect();
        let t = Tuning {
            max_chunk: 1200,
            target_chunk: 800,
            ..Tuning::default()
        };
        let c = chunk_markdown_with(&format!("# H\n{body}"), &t);
        assert!(c.len() > 1);
        assert!(c.iter().all(|x| x.body.len() <= t.max_chunk && x.heading == "H"));
    }

    #[test]
    fn queries_drop_stopwords_and_quote_terms() {
        assert_eq!(
            word_query("What port is Grafana on?").unwrap(),
            "\"port\"* OR \"grafana\"*"
        );
        assert_eq!(trigram_query("what is 203.0.113.7?").unwrap(), "\"203.0.113.7\"");
        assert!(word_query("the of is").is_none());
    }

    #[test]
    fn focus_snippet_prefers_matching_lines() {
        let body: String = (0..30)
            .map(|i| format!("- filler line number {i} about nothing much\n"))
            .collect::<String>()
            + "- Grafana is on port 3000\n";
        let s = focus_snippet(&body, "what port is grafana on", 200);
        assert!(s.contains("3000"), "{s}");
        // Skipped filler shows as a gap marker before the matching last line.
        assert!(s.contains("…\n- Grafana is on port 3000"), "{s}");
        assert!(s.chars().count() <= 210);
        // Short bodies come back untouched.
        assert_eq!(focus_snippet("short", "x", 200), "short");
    }

    #[test]
    fn section_read_includes_subsections_only() {
        let dir = std::env::temp_dir().join(format!("memdex-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("n.md");
        fs::write(&p, "# One\na\n## Sub\nb\n# Two\nc\n").unwrap();
        let s = read_file(&p, Some("one")).unwrap();
        assert!(s.contains("a\n") && s.contains("## Sub") && !s.contains("# Two"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn index_finds_words_identifiers_and_picks_up_edits() {
        let root = std::env::temp_dir().join(format!("memdex-idx-{}", std::process::id()));
        let mem = root.join("-x-proj").join("memory");
        fs::create_dir_all(&mem).unwrap();
        fs::write(mem.join("db.md"), "# Database\nPostgres at 203.0.113.7 and .8\n").unwrap();
        let mut idx = Index::new(vec![(mem.clone(), "proj".into())], vec![]).unwrap();
        let top = &idx.search("postgres database", 5, None).unwrap()[0];
        assert_eq!((top.file.as_str(), top.project.as_str()), ("db", "proj"));
        assert_eq!(idx.search("113.7", 5, None).unwrap().len(), 1);
        // mtime granularity: make sure the edit is seen as a change.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(mem.join("db.md"), "# Database\nnow using sqlite\n").unwrap();
        let f = fs::File::options().append(true).open(mem.join("db.md")).unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(idx.search("postgres", 5, None).unwrap().is_empty());
        assert_eq!(idx.search("sqlite", 5, None).unwrap().len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn excluded_notes_are_invisible_to_search_and_read() {
        let root = std::env::temp_dir().join(format!("memdex-excl-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("work.md"), "secret project plans\n").unwrap();
        fs::write(root.join("home.md"), "home project plans\n").unwrap();
        let mut idx = Index::new(vec![(root.clone(), "lab".into())], vec!["lab/work".into()]).unwrap();
        let hits = idx.search("project plans", 5, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, "home");
        assert!(idx.find_file("work", None).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
