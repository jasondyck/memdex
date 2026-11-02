//! memdex — fast, lightweight memory for AI agents: full-text search over
//! markdown memory notes plus a shared, agent-writable vault, served over MCP.
//!
//! Usage:
//!   memdex serve [OPTS]                         MCP over stdio (Claude Code, other MCP agents)
//!   memdex http --listen ADDR --client CONF...  MCP over HTTP for remote agents
//!   memdex search QUERY [OPTS] [--limit N]      search from the shell
//!   memdex read FILE [SECTION] [OPTS]           print a note (or matching parts)
//!   memdex remember TOPIC FACT [OPTS]           add a fact to the shared vault
//!
//! OPTS: --config FILE | --agent NAME | --dir DIR | --write-dir DIR | --read-only
//! Without --config: all ~/.claude/projects/*/memory dirs + ~/memory (writable).

mod config;
mod embed;
mod http;
mod index;
mod remember;

use config::Config;
use index::{read_file, Hit, Index, Tuning};
use std::sync::Arc;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Per-hit snippet budget (query-focused lines); the full section is one `read {id}` away.
/// 3 hits x 1200 chars delivered the answer in 81% of held-out questions vs 60% for 6 x 500,
/// at the same ~2.8k chars of output.
const SNIPPET_CHARS: usize = 1200;
const DEFAULT_LIMIT: u64 = 3;
/// Cap for whole-file reads, so one call can't flood the context.
const READ_CAP: usize = 24_000;

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let cut: String = s.chars().take(n).collect();
    format!("{cut}…")
}

/// Render hits for an agent. With `query`, snippets come from `Index::snippet`
/// (query-focused lines); `full` returns whole sections.
pub fn format_hits_n(idx: &Index, hits: &[Hit], full: bool, snip: usize, query: Option<&str>) -> String {
    if hits.is_empty() {
        return "No matches. Try fewer or different key words (service, host, error text).".into();
    }
    let mut out = String::new();
    for h in hits {
        let heading = if h.heading.is_empty() { String::new() } else { format!(" › {}", h.heading) };
        out.push_str(&format!("[{}] {}/{}{}\n", h.id, h.project, h.file, heading));
        let body = match (full, query) {
            (true, _) => h.body.clone(),
            (false, Some(q)) => idx.snippet(&h.body, q, snip),
            (false, None) => truncate(&h.body, snip),
        };
        out.push_str(&body);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// One agent's view: its index plus its config (identity, write access).
pub struct Server {
    pub index: Index,
    pub cfg: Config,
}

impl Server {
    fn instructions(&self) -> String {
        let mut s = String::from(
            "Long-term memory shared by the user's AI agents: notes on their setup (hosts, services, ports, IPs), \
             projects, past fixes, gotchas, decisions and preferences. Before answering or acting on anything \
             about their setup or projects, call `search` with the key nouns (service, host, error text, port); \
             use `read` for more. Results are past notes: verify live state (ports, paths, IPs) before acting.",
        );
        if self.cfg.write_dir.is_some() {
            s.push_str(
                " When you learn something durable (a fix that worked and why, a gotcha, a decision, a changed \
                 host/port/service, a preference or correction), call `remember` with ONE self-contained fact, \
                 reusing an existing topic name from search results when one fits. Never store secrets \
                 (passwords, tokens, keys) — record where they live instead.",
            );
        }
        s
    }

    fn tool_defs(&self) -> Value {
        let mut tools = vec![
            json!({
                "name": "search",
                "description": "Search the memory notes. Returns the best-matching notes as `[id] project/file › heading` plus the lines that match your query (up to 1200 chars each). Use key nouns: service, host, port, error text. Exact identifiers (IPs, hostnames) work too.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Key words, e.g. `ci cache key` or `203.0.113.7`"},
                        "limit": {"type": "integer", "description": "Max results (default 3, max 20)"},
                        "project": {"type": "string", "description": "Only notes whose project label contains this, e.g. `home`, `shared`, `projects-webshop`"},
                        "full": {"type": "boolean", "description": "Return whole sections instead of focused snippets"}
                    },
                    "required": ["query"]
                }
            }),
            json!({
                "name": "read",
                "description": "Read memory: `id` from a search hit returns that whole section; `file` (note name without .md) returns the note, optionally only the headings/paragraphs containing `section`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"},
                        "file": {"type": "string", "description": "e.g. `release-checklist`"},
                        "section": {"type": "string", "description": "Word or heading text, e.g. `gotcha`"},
                        "project": {"type": "string", "description": "Disambiguate a note name that exists in several projects"}
                    }
                }
            }),
        ];
        if self.cfg.write_dir.is_some() {
            tools.push(json!({
                "name": "remember",
                "description": "Save ONE durable fact to shared memory (visible to all agents). Appends a dated bullet to the topic's note, creating it if new. Refuses anything that looks like a credential.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "topic": {"type": "string", "description": "Note name, e.g. `release-checklist`. Reuse an existing one from search when it fits."},
                        "fact": {"type": "string", "description": "Self-contained fact with context (host, path, date, why)."},
                        "description": {"type": "string", "description": "One-line summary; used only when the topic is new."},
                        "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"], "description": "Default `project`."}
                    },
                    "required": ["topic", "fact"]
                }
            }));
        }
        Value::Array(tools)
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<String, String> {
        let s = |k: &str| args.get(k).and_then(Value::as_str);
        match name {
            "search" => {
                let q = s("query").ok_or("`query` is required")?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(DEFAULT_LIMIT).clamp(1, 20) as usize;
                let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
                let hits = self.index.search(q, limit, s("project")).map_err(|e| e.to_string())?;
                Ok(format_hits_n(&self.index, &hits, full, SNIPPET_CHARS, Some(q)))
            }
            "read" => {
                if let Some(id) = args.get("id").and_then(Value::as_i64) {
                    return match self.index.chunk(id).map_err(|e| e.to_string())? {
                        Some(h) => Ok(format_hits_n(&self.index, &[h], true, 0, None)),
                        None => Err(format!("no section with id {id} (ids change when a note is edited; search again)")),
                    };
                }
                let file = s("file").ok_or("pass `id` or `file`")?;
                self.index.refresh().map_err(|e| e.to_string())?;
                let path = self.index.find_file(file, s("project"))?;
                let text = read_file(&path, s("section"))?;
                Ok(if text.chars().count() > READ_CAP {
                    format!("{}\n\n[truncated at {READ_CAP} chars; pass `section` to read a part]", truncate(&text, READ_CAP))
                } else {
                    text
                })
            }
            "remember" => {
                let vault = self.cfg.write_dir.clone().ok_or("this agent is read-only")?;
                let msg = remember::remember(
                    &vault,
                    &self.cfg.agent,
                    s("topic").ok_or("`topic` is required")?,
                    s("fact").ok_or("`fact` is required")?,
                    s("description"),
                    s("type"),
                )?;
                // Make the new fact searchable immediately.
                self.index.refresh().map_err(|e| e.to_string())?;
                Ok(msg)
            }
            _ => Err(format!("unknown tool `{name}`")),
        }
    }

    /// One JSON-RPC message in, at most one response out (none for notifications).
    pub fn handle(&mut self, req: &Value) -> Option<Value> {
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "memdex", "version": VERSION},
                "instructions": self.instructions()
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": self.tool_defs()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match self.call_tool(name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        let id = id?;
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        })
    }
}

fn serve_stdio(mut srv: Server) -> io::Result<()> {
    let stdin = io::stdin();
    let mut out = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => srv.handle(&req),
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        if let Some(r) = resp {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("memdex: {msg}");
    std::process::exit(1);
}

/// Pull `--flag VALUE` occurrences out of `args`.
fn take_all(args: &mut Vec<String>, flag: &str) -> Vec<String> {
    let mut out = Vec::new();
    while let Some(i) = args.iter().position(|a| a == flag) {
        args.remove(i);
        if i < args.len() {
            out.push(args.remove(i));
        } else {
            die(format!("{flag} needs a value"));
        }
    }
    out
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> bool {
    let before = args.len();
    args.retain(|a| a != flag);
    args.len() != before
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let configs = take_all(&mut args, "--config");
    let clients = take_all(&mut args, "--client");
    let dirs = take_all(&mut args, "--dir");
    let write_dirs = take_all(&mut args, "--write-dir");
    let agent = take_all(&mut args, "--agent").pop();
    let listen = take_all(&mut args, "--listen").pop().unwrap_or_else(|| "127.0.0.1:8765".into());
    let limit: usize = take_all(&mut args, "--limit").pop().and_then(|l| l.parse().ok()).unwrap_or(DEFAULT_LIMIT as usize);
    let read_only = take_flag(&mut args, "--read-only");
    let quiet = take_flag(&mut args, "--quiet");
    let no_model = take_flag(&mut args, "--no-model");
    let model_arg = take_all(&mut args, "--model").pop();
    let snip: usize = take_all(&mut args, "--snip").pop().and_then(|l| l.parse().ok()).unwrap_or(SNIPPET_CHARS);
    let tune = match take_all(&mut args, "--tune").pop() {
        Some(t) => Tuning::parse(&t).unwrap_or_else(|e| die(e)),
        None => Tuning::default(),
    };
    let cmd = args.first().cloned().unwrap_or_else(|| "serve".into());

    if cmd == "http" {
        if clients.is_empty() {
            die("http needs at least one --client CONFIG (each with its own token)");
        }
        let cfgs: Vec<Config> =
            clients.iter().map(|p| Config::load(&config::expand(p)).unwrap_or_else(|e| die(e))).collect();
        http::run(&listen, cfgs).unwrap_or_else(|e| die(e));
        return;
    }

    let mut cfg = match configs.last() {
        Some(p) => Config::load(&config::expand(p)).unwrap_or_else(|e| die(e)),
        None if !dirs.is_empty() || !write_dirs.is_empty() => {
            let mut c = Config::parse("").unwrap();
            c.agent = "claude".into();
            c
        }
        None => Config::default_local(),
    };
    for d in dirs {
        cfg.add_dir(config::expand(&d), None);
    }
    if let Some(w) = write_dirs.last() {
        cfg.set_write_dir(config::expand(w));
    }
    if let Some(a) = agent {
        cfg.agent = a;
    }
    if read_only {
        cfg.write_dir = None;
    }

    let t0 = Instant::now();
    let model = if no_model { None } else { model_arg.as_deref().map(config::expand).or(cfg.model.clone()) };
    let emb = model.map(|dir| Arc::new(embed::Embedder::load(&dir).unwrap_or_else(|e| die(format!("model: {e}")))));
    let mut index = Index::with_model(cfg.dirs.clone(), cfg.exclude.clone(), tune, emb)
        .unwrap_or_else(|e| die(format!("index failed: {e}")));
    index.watch_claude = cfg.watch_claude;
    let built = t0.elapsed();
    let mut srv = Server { index, cfg };
    match cmd.as_str() {
        "serve" => {
            eprintln!(
                "memdex {VERSION}: agent `{}`, {} notes indexed in {built:?}, write={}",
                srv.cfg.agent,
                srv.index.file_count(),
                srv.cfg.write_dir.is_some()
            );
            if let Err(e) = serve_stdio(srv) {
                die(e);
            }
        }
        "search" => {
            let q = args[1..].join(" ");
            let t = Instant::now();
            let hits = srv.index.search(&q, limit, None).unwrap_or_else(|e| die(e));
            println!("{}", format_hits_n(&srv.index, &hits, false, snip, Some(q.as_str())));
            eprintln!("-- {} notes indexed in {built:?}; query took {:?}", srv.index.file_count(), t.elapsed());
        }
        "read" => {
            let res = srv
                .index
                .find_file(args.get(1).map(String::as_str).unwrap_or(""), None)
                .and_then(|p| read_file(&p, args.get(2).map(String::as_str)));
            match res {
                Ok(t) => println!("{t}"),
                Err(e) => die(e),
            }
        }
        "remember" => {
            let (Some(topic), Some(fact)) = (args.get(1), args.get(2)) else {
                die("usage: memdex remember TOPIC FACT");
            };
            let vault = srv.cfg.write_dir.clone().unwrap_or_else(|| die("read-only: no write dir"));
            match remember::remember(&vault, &srv.cfg.agent, topic, fact, None, None) {
                Ok(m) => println!("{m}"),
                Err(e) => die(e),
            }
        }
        "tokens" => {
            // Debug: token ids + first 5 vector values, to compare with Python model2vec.
            let dir = model_arg.as_deref().map(config::expand).or(config::default_model()).unwrap_or_else(|| die("no model"));
            let e = embed::Embedder::load(&dir).unwrap_or_else(|e| die(e));
            let t = args[1..].join(" ");
            let ids = e.token_ids(&t);
            println!("RS {} {}", ids.len(), ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(" "));
            if let Some(v) = e.embed(&t) {
                println!("RSV {}", v[..5].iter().map(|x| format!("{x:.5}")).collect::<Vec<_>>().join(" "));
            }
        }
        "bench" => bench(&mut srv, args.get(1).map(PathBuf::from), limit, snip, quiet, built),
        _ => die("usage: memdex [serve|http|search|read|remember|bench] … (see source header)"),
    }
}

/// `memdex bench CASES.tsv`: accuracy + latency + output size over labelled queries.
/// Each line: `query<TAB>expected-note[,alt…][<TAB>answer substring]`.
/// "ans" = the answer substring appears in what the agent actually receives.
fn bench(srv: &mut Server, cases: Option<PathBuf>, limit: usize, snip: usize, quiet: bool, built: std::time::Duration) {
    let path = cases.unwrap_or_else(|| die("usage: memdex bench CASES.tsv"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| die(e));
    let (mut n, mut top1, mut top3, mut topk, mut rr, mut bytes) = (0, 0, 0, 0, 0.0f64, 0usize);
    let (mut n_ans, mut ans) = (0, 0);
    let mut times = Vec::new();
    let mut misses = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        let (q, want) = (cols[0], cols[1].split(',').map(str::trim).collect::<Vec<_>>());
        let t = Instant::now();
        let hits = srv.index.search(q, limit, None).unwrap_or_default();
        times.push(t.elapsed().as_secs_f64() * 1000.0);
        let shown = format_hits_n(&srv.index, &hits, false, snip, Some(q));
        bytes += shown.len();
        if let Some(a) = cols.get(2).filter(|a| !a.trim().is_empty()) {
            n_ans += 1;
            ans += shown.to_lowercase().contains(&a.trim().to_lowercase()) as usize;
        }
        let rank = hits.iter().position(|h| want.contains(&h.file.as_str()));
        n += 1;
        match rank {
            Some(r) => {
                rr += 1.0 / (r as f64 + 1.0);
                top1 += (r == 0) as usize;
                top3 += (r < 3) as usize;
                topk += 1;
            }
            None => misses.push(format!("  MISS {q:?} want {want:?} got {:?}", hits.iter().take(3).map(|h| &h.file).collect::<Vec<_>>())),
        }
    }
    if n == 0 {
        die("no cases");
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| times[((times.len() as f64 - 1.0) * p).round() as usize];
    let f = |k: usize| 100.0 * k as f64 / n as f64;
    let ans_s = if n_ans > 0 { format!(" | ans {:.1}%", 100.0 * ans as f64 / n_ans as f64) } else { String::new() };
    println!(
        "n {n} | top1 {:.1}% | top3 {:.1}% | top{limit} {:.1}% | MRR {:.3}{ans_s} | p50 {:.2}ms p95 {:.2}ms | out {} ch | build {:.0}ms",
        f(top1), f(top3), f(topk), rr / n as f64, pct(0.5), pct(0.95), bytes / n, built.as_secs_f64() * 1000.0
    );
    if !quiet {
        for m in misses {
            println!("{m}");
        }
    }
}
