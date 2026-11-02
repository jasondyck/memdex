//! Static embeddings (Model2Vec "potion" models): a token → vector lookup table.
//! A text's embedding is the mean of its token vectors, L2-normalised — no neural
//! network runs at query time, so embedding a sentence takes microseconds.
//!
//! Implemented from scratch (safetensors header + BERT WordPiece) to keep memdex at
//! two dependencies. Matches model2vec's `encode`: no special tokens, unknown
//! tokens dropped, 512-token cap, mean pooling, normalisation.

use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

const MAX_TOKENS: usize = 512;

pub struct Embedder {
    pub dim: usize,
    vocab: HashMap<String, u32>,
    unk: Option<u32>,
    table: Vec<f32>,
    lowercase: bool,
    prefix: String,
    max_word_chars: usize,
    pub name: String,
}

impl Embedder {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let err = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
        // --- safetensors: u64 header length, JSON header, raw little-endian data.
        let bytes = fs::read(dir.join("model.safetensors")).map_err(|e| err(&e))?;
        if bytes.len() < 8 {
            return Err(err(&"model.safetensors too short"));
        }
        let hlen = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
        let header: Value =
            serde_json::from_slice(bytes.get(8..8 + hlen).ok_or(err(&"bad header"))?).map_err(|e| err(&e))?;
        let t = header.get("embeddings").ok_or(err(&"no `embeddings` tensor"))?;
        if t["dtype"] != "F32" {
            return Err(err(&format!("unsupported dtype {}", t["dtype"])));
        }
        let shape: Vec<usize> = t["shape"]
            .as_array()
            .ok_or(err(&"no shape"))?
            .iter()
            .filter_map(Value::as_u64)
            .map(|x| x as usize)
            .collect();
        let off: Vec<usize> = t["data_offsets"]
            .as_array()
            .ok_or(err(&"no offsets"))?
            .iter()
            .filter_map(Value::as_u64)
            .map(|x| x as usize)
            .collect();
        let (rows, dim) = (shape[0], shape[1]);
        let data = bytes
            .get(8 + hlen + off[0]..8 + hlen + off[1])
            .ok_or(err(&"data out of range"))?;
        if data.len() != rows * dim * 4 {
            return Err(err(&"tensor size mismatch"));
        }
        let table: Vec<f32> = data.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();

        // --- tokenizer.json (HF tokenizers, WordPiece + BertNormalizer).
        let tok: Value =
            serde_json::from_slice(&fs::read(dir.join("tokenizer.json")).map_err(|e| err(&e))?).map_err(|e| err(&e))?;
        let model = &tok["model"];
        if model["type"] != "WordPiece" {
            return Err(err(&"only WordPiece tokenizers are supported"));
        }
        let vocab: HashMap<String, u32> = model["vocab"]
            .as_object()
            .ok_or(err(&"no vocab"))?
            .iter()
            .filter_map(|(k, v)| v.as_u64().map(|i| (k.clone(), i as u32)))
            .filter(|(_, i)| (*i as usize) < rows)
            .collect();
        let unk = model["unk_token"].as_str().and_then(|u| vocab.get(u).copied());
        let lowercase = tok["normalizer"]["lowercase"].as_bool().unwrap_or(true);
        let prefix = model["continuing_subword_prefix"].as_str().unwrap_or("##").to_string();
        let max_word_chars = model["max_input_chars_per_word"].as_u64().unwrap_or(100) as usize;
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Embedder {
            dim,
            vocab,
            unk,
            table,
            lowercase,
            prefix,
            max_word_chars,
            name,
        })
    }

    /// BERT basic tokenisation: clean, lowercase, split on whitespace and punctuation.
    fn words(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        for c in text.chars() {
            if c.is_whitespace() || c.is_control() {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            } else if c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_ascii()) {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(c.to_string());
            } else if self.lowercase {
                cur.extend(c.to_lowercase());
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// Greedy longest-match WordPiece; a word with no full segmentation is unknown.
    fn wordpiece(&self, word: &str, out: &mut Vec<u32>) {
        let chars: Vec<char> = word.chars().collect();
        if chars.len() > self.max_word_chars {
            out.extend(self.unk);
            return;
        }
        let mut pieces = Vec::new();
        let mut start = 0;
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while start < end {
                let mut sub: String = chars[start..end].iter().collect();
                if start > 0 {
                    sub = format!("{}{sub}", self.prefix);
                }
                if let Some(&id) = self.vocab.get(&sub) {
                    found = Some(id);
                    break;
                }
                end -= 1;
            }
            match found {
                Some(id) => {
                    pieces.push(id);
                    start = end;
                }
                None => {
                    out.extend(self.unk);
                    return;
                }
            }
        }
        out.extend(pieces);
    }

    pub fn token_ids(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        for w in self.words(text) {
            self.wordpiece(&w, &mut ids);
            if ids.len() >= MAX_TOKENS {
                break;
            }
        }
        ids.truncate(MAX_TOKENS);
        ids.retain(|i| Some(*i) != self.unk);
        ids
    }

    /// Mean of token vectors, L2-normalised. `None` if no known tokens.
    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let ids = self.token_ids(text);
        if ids.is_empty() {
            return None;
        }
        let mut v = vec![0f32; self.dim];
        for id in &ids {
            let row = &self.table[*id as usize * self.dim..(*id as usize + 1) * self.dim];
            for (a, b) in v.iter_mut().zip(row) {
                *a += b;
            }
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if n == 0.0 {
            return None;
        }
        v.iter_mut().for_each(|x| *x /= n);
        Some(v)
    }
}

pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Option<Embedder> {
        let d = crate::config::home().join(".local/share/memdex/models/potion-base-8M");
        d.exists().then(|| Embedder::load(&d).unwrap())
    }

    #[test]
    fn tokenises_like_bert() {
        let Some(m) = model() else { return };
        // "unaffable" is the classic WordPiece example; punctuation splits off.
        let ids = m.token_ids("Hello, world!");
        assert_eq!(ids.len(), 4);
        assert!(m.token_ids("tailscale").len() >= 2, "subword split expected");
    }

    #[test]
    fn similar_meanings_score_higher() {
        let Some(m) = model() else { return };
        let q = m.embed("how do I make the build pipeline faster").unwrap();
        let cache = m.embed("CI cache keyed on the lockfile cuts build time").unwrap();
        let other = m.embed("the upstream API allows 100 requests per minute").unwrap();
        assert!(dot(&q, &cache) > dot(&q, &other));
        assert!((dot(&q, &q) - 1.0).abs() < 1e-4);
    }
}
