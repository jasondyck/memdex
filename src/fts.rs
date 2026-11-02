//! In-memory full-text index replacing SQLite FTS5. Reproduces what memdex used:
//! - a word index (Porter-stemmed unicode61 tokens) over four weighted columns
//!   (note name, heading, hidden context, body), ranked with FTS5's exact BM25
//!   (k1 = 1.2, b = 0.75, per-column weights, idf floor 1e-6);
//! - prefix terms (`"stem"*`), OR-ed phrases;
//! - a substring search ranked like FTS5's trigram tokenizer (for IPs, hostnames).

use crate::porter::tokens;
use std::collections::{BTreeMap, HashMap};

const K1: f64 = 1.2;
const B: f64 = 0.75;
pub const COLS: usize = 4;

pub struct Doc {
    pub project: String,
    pub file: String,
    pub heading: String,
    pub body: String,
    /// Total tokens across the four indexed columns (FTS5's per-row size).
    len: u32,
    /// Trigram count of the body (the trigram table's per-row size).
    tri_len: u32,
    /// Term ids, for removal.
    terms: Vec<u32>,
}

/// Compact layout: each term string is stored once (`vocab`), postings are packed
/// `(doc slot, per-column counts)` vectors, and bodies are kept once (substring
/// search compares case-insensitively in place).
#[derive(Default)]
pub struct Fts {
    /// Slot = id - 1; removed sections leave `None` so ids stay stable.
    docs: Vec<Option<Doc>>,
    live: usize,
    by_path: HashMap<String, Vec<u32>>,
    vocab: BTreeMap<String, u32>,
    post: Vec<Vec<(u32, [u16; COLS])>>,
    total_len: u64,
    total_tri: u64,
}

/// One query phrase: a stemmed token, optionally a prefix.
pub struct Phrase {
    pub term: String,
    pub prefix: bool,
}

fn idf(n_rows: usize, n_hit: usize) -> f64 {
    let v = ((n_rows as f64 - n_hit as f64 + 0.5) / (n_hit as f64 + 0.5)).ln();
    if v <= 0.0 { 1e-6 } else { v }
}

fn bm25_term(freq: f64, doc_len: f64, avgdl: f64) -> f64 {
    freq * (K1 + 1.0) / (freq + K1 * (1.0 - B + B * doc_len / avgdl))
}

fn project_ok(doc: &Doc, project: Option<&str>) -> bool {
    project.is_none_or(|p| doc.project.to_lowercase().contains(&p.to_lowercase()))
}

impl Fts {
    /// Index one section; returns its id (stable until its file changes).
    pub fn insert(&mut self, path: &str, project: &str, file: &str, heading: &str, ctx: &str, body: &str) -> i64 {
        let slot = self.docs.len() as u32;
        let mut counts: HashMap<String, [u16; COLS]> = HashMap::new();
        let mut len = 0u32;
        for (c, text) in [file, heading, ctx, body].into_iter().enumerate() {
            for t in tokens(text) {
                let e = &mut counts.entry(t).or_default()[c];
                *e = e.saturating_add(1);
                len += 1;
            }
        }
        let mut terms = Vec::with_capacity(counts.len());
        for (t, cnt) in counts {
            let tid = match self.vocab.get(&t) {
                Some(&id) => id,
                None => {
                    let id = self.post.len() as u32;
                    self.vocab.insert(t, id);
                    self.post.push(Vec::new());
                    id
                }
            };
            // Slots only grow, so each posting list stays sorted by slot.
            self.post[tid as usize].push((slot, cnt));
            terms.push(tid);
        }
        let tri_len = body.chars().count().saturating_sub(2) as u32;
        self.total_len += len as u64;
        self.total_tri += tri_len as u64;
        self.by_path.entry(path.to_string()).or_default().push(slot);
        self.docs.push(Some(Doc {
            project: project.into(),
            file: file.into(),
            heading: heading.into(),
            body: body.into(),
            len,
            tri_len,
            terms,
        }));
        self.live += 1;
        slot as i64 + 1
    }

    pub fn remove_path(&mut self, path: &str) {
        for slot in self.by_path.remove(path).unwrap_or_default() {
            let Some(doc) = self.docs[slot as usize].take() else {
                continue;
            };
            for tid in doc.terms {
                let p = &mut self.post[tid as usize];
                if let Ok(i) = p.binary_search_by_key(&slot, |(s, _)| *s) {
                    p.remove(i);
                }
            }
            self.total_len -= doc.len as u64;
            self.total_tri -= doc.tri_len as u64;
            self.live -= 1;
        }
    }

    /// Remove one section (used to replace a transcript's still-growing last exchange).
    pub fn remove_id(&mut self, id: i64) {
        let Ok(slot) = u32::try_from(id - 1) else { return };
        let Some(doc) = self.docs.get_mut(slot as usize).and_then(Option::take) else {
            return;
        };
        for tid in doc.terms {
            let p = &mut self.post[tid as usize];
            if let Ok(i) = p.binary_search_by_key(&slot, |(s, _)| *s) {
                p.remove(i);
            }
        }
        for slots in self.by_path.values_mut() {
            slots.retain(|s| *s != slot);
        }
        self.total_len -= doc.len as u64;
        self.total_tri -= doc.tri_len as u64;
        self.live -= 1;
    }

    pub fn len(&self) -> usize {
        self.live
    }

    pub fn get(&self, id: i64) -> Option<&Doc> {
        let slot = usize::try_from(id - 1).ok()?;
        self.docs.get(slot)?.as_ref()
    }

    fn doc(&self, slot: u32) -> &Doc {
        self.docs[slot as usize].as_ref().expect("posting points at a live doc")
    }

    /// Docs matching a phrase, with summed per-column counts (prefix phrases
    /// merge every term starting with the stem).
    fn matches(&self, ph: &Phrase) -> HashMap<u32, [u32; COLS]> {
        let mut out: HashMap<u32, [u32; COLS]> = HashMap::new();
        let mut add = |tid: u32| {
            for (slot, c) in &self.post[tid as usize] {
                let e = out.entry(*slot).or_default();
                for i in 0..COLS {
                    e[i] += c[i] as u32;
                }
            }
        };
        if ph.prefix {
            for (_, &tid) in self
                .vocab
                .range(ph.term.clone()..)
                .take_while(|(t, _)| t.starts_with(&ph.term))
            {
                add(tid);
            }
        } else if let Some(&tid) = self.vocab.get(&ph.term) {
            add(tid);
        }
        out
    }

    fn rank(&self, score: HashMap<u32, f64>, project: Option<&str>, limit: usize) -> Vec<i64> {
        let mut v: Vec<(u32, f64)> = score
            .into_iter()
            .filter(|(s, _)| project_ok(self.doc(*s), project))
            .collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().take(limit).map(|(s, _)| s as i64 + 1).collect()
    }

    /// FTS5 `bm25(weights…)` over OR-ed phrases: best first, ties by id.
    pub fn search(&self, phrases: &[Phrase], w: [f64; COLS], project: Option<&str>, limit: usize) -> Vec<i64> {
        let n = self.live;
        if n == 0 || phrases.is_empty() {
            return vec![];
        }
        let avgdl = self.total_len as f64 / n as f64;
        let mut score: HashMap<u32, f64> = HashMap::new();
        for ph in phrases {
            let m = self.matches(ph);
            let idf = idf(n, m.len());
            for (slot, c) in m {
                let freq: f64 = (0..COLS).map(|i| w[i] * c[i] as f64).sum();
                *score.entry(slot).or_default() += idf * bm25_term(freq, self.doc(slot).len as f64, avgdl);
            }
        }
        self.rank(score, project, limit)
    }

    /// Case-insensitive substring search over bodies, ranked like an FTS5
    /// trigram table: each term is a phrase whose frequency is its number of
    /// (overlapping) occurrences; row size = trigram count.
    pub fn substring(&self, terms: &[String], project: Option<&str>, limit: usize) -> Vec<i64> {
        let n = self.live;
        if n == 0 {
            return vec![];
        }
        let avgdl = self.total_tri as f64 / n as f64;
        let mut score: HashMap<u32, f64> = HashMap::new();
        for t in terms
            .iter()
            .map(|t| t.to_lowercase())
            .filter(|t| t.chars().count() >= 3)
        {
            let hits: Vec<(u32, usize)> = self
                .docs
                .iter()
                .enumerate()
                .filter_map(|(slot, d)| d.as_ref().map(|d| (slot as u32, count_ci(&d.body, &t))))
                .filter(|(_, c)| *c > 0)
                .collect();
            let idf = idf(n, hits.len());
            for (slot, c) in hits {
                *score.entry(slot).or_default() += idf * bm25_term(c as f64, self.doc(slot).tri_len as f64, avgdl);
            }
        }
        self.rank(score, project, limit)
    }
}

/// Overlapping, case-insensitive occurrences of lower-cased `needle` in `hay`.
/// ASCII needles compare in place (no allocation); others lower-case `hay`.
fn count_ci(hay: &str, needle: &str) -> usize {
    if needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        if n.is_empty() || h.len() < n.len() {
            return 0;
        }
        return h.windows(n.len()).filter(|w| w.eq_ignore_ascii_case(n)).count();
    }
    count_overlapping(&hay.to_lowercase(), needle)
}

fn count_overlapping(hay: &str, needle: &str) -> usize {
    let mut n = 0;
    let mut start = 0;
    while let Some(i) = hay[start..].find(needle) {
        n += 1;
        start += i + hay[start + i..].chars().next().map_or(1, char::len_utf8);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ph(t: &str, prefix: bool) -> Phrase {
        Phrase { term: t.into(), prefix }
    }

    #[test]
    fn ranks_prefix_and_removes() {
        let mut f = Fts::default();
        let a = f.insert(
            "/n/a.md",
            "p",
            "build-cache",
            "",
            "",
            "the ci cache is keyed on the lockfile",
        );
        let b = f.insert(
            "/n/b.md",
            "p",
            "db",
            "",
            "",
            "migrations run before deploy; running jobs wait",
        );
        assert_eq!(f.search(&[ph("cach", true)], [4.0, 3.0, 1.0, 1.0], None, 5), vec![a]);
        assert_eq!(f.search(&[ph("run", true)], [4.0, 3.0, 1.0, 1.0], None, 5), vec![b]);
        assert_eq!(f.substring(&["LOCKFILE".into()], None, 5), vec![a]);
        let c = f.insert("/n/c.md", "p", "c", "", "", "lockfile notes");
        f.remove_id(c);
        assert_eq!(f.substring(&["lockfile".into()], None, 5), vec![a]);
        f.remove_path("/n/a.md");
        assert!(f.search(&[ph("cach", true)], [4.0, 3.0, 1.0, 1.0], None, 5).is_empty());
        assert!(f.get(a).is_none());
    }

    #[test]
    fn overlapping_counts() {
        assert_eq!(count_overlapping("aaaa", "aa"), 3);
        assert_eq!(count_overlapping("x192.0.2.53y", "2.53"), 1);
        assert_eq!(count_ci("Build CACHE, cache", "cache"), 2);
        assert_eq!(count_ci("Résumé RÉSUMÉ", "résumé"), 2);
    }
}
