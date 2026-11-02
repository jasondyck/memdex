//! Minimal MCP Streamable-HTTP transport (JSON responses, no SSE) for agents on
//! other hosts. Each client config carries its own bearer token, which selects
//! that client's index (its allowed dirs / exclusions / write access).
//!
//! Bind to 127.0.0.1 and publish with `tailscale serve`; TLS comes from there.

use crate::Server;
use crate::config::Config;
use crate::index::Index;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_HEADERS: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;

pub struct Client {
    token: String,
    server: Mutex<Server>,
}

pub fn run(listen: &str, configs: Vec<Config>) -> Result<(), String> {
    let mut clients = Vec::new();
    for cfg in configs {
        let token = cfg
            .token
            .clone()
            .ok_or(format!("client `{}` has no token", cfg.agent))?;
        let emb = match &cfg.model {
            Some(d) => Some(std::sync::Arc::new(crate::embed::Embedder::load(d)?)),
            None => None,
        };
        let mut index = Index::with_model(
            cfg.dirs.clone(),
            cfg.exclude.clone(),
            crate::index::Tuning::default(),
            emb,
        )
        .map_err(|e| e.to_string())?;
        index.watch_claude = cfg.watch_claude;
        if cfg.transcripts {
            index.hist = Some(crate::transcripts::Transcripts::default());
        }
        eprintln!(
            "memdex http: client `{}` → {} notes, write={}",
            cfg.agent,
            index.file_count(),
            cfg.write_dir.is_some()
        );
        clients.push(Client {
            token,
            server: Mutex::new(Server { index, cfg }),
        });
    }
    let clients = Arc::new(clients);
    let listener = TcpListener::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    eprintln!("memdex http: listening on {listen} (POST /mcp)");
    for stream in listener.incoming().flatten() {
        let clients = Arc::clone(&clients);
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            if let Err(e) = handle_conn(stream, &clients) {
                eprintln!("memdex http: {e}");
            }
        });
    }
    Ok(())
}

/// Constant-time comparison so token checks don't leak length-prefix timing.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, body: &str, extra: &str) -> std::io::Result<()> {
    write!(
        s,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

fn handle_conn(mut stream: TcpStream, clients: &[Client]) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).map_err(|e| e.to_string())?;
    let mut parts = request_line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));

    let mut len = 0usize;
    let mut auth = String::new();
    let mut seen = request_line.len();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
        seen += n;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if seen > MAX_HEADERS {
            return respond(&mut stream, "431 Request Header Fields Too Large", "text/plain", "", "")
                .map_err(|e| e.to_string());
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().unwrap_or(0),
                "authorization" => auth = v.trim().to_string(),
                _ => {}
            }
        }
    }

    let path = path.split('?').next().unwrap_or("");
    if path == "/health" {
        return respond(&mut stream, "200 OK", "text/plain", "ok\n", "").map_err(|e| e.to_string());
    }
    if path != "/mcp" && path != "/mcp/" {
        return respond(&mut stream, "404 Not Found", "text/plain", "not found\n", "").map_err(|e| e.to_string());
    }
    let token = auth.strip_prefix("Bearer ").unwrap_or("");
    let Some(client) = clients.iter().find(|c| same(&c.token, token)) else {
        return respond(
            &mut stream,
            "401 Unauthorized",
            "text/plain",
            "unauthorized\n",
            "WWW-Authenticate: Bearer\r\n",
        )
        .map_err(|e| e.to_string());
    };
    if method != "POST" {
        // No server-initiated stream; clients fall back to plain request/response.
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            "",
            "Allow: POST\r\n",
        )
        .map_err(|e| e.to_string());
    }
    if len > MAX_BODY {
        return respond(&mut stream, "413 Payload Too Large", "text/plain", "", "").map_err(|e| e.to_string());
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;

    let reply: Option<Value> = match serde_json::from_slice::<Value>(&body) {
        Ok(Value::Array(batch)) => {
            let mut srv = client.server.lock().map_err(|_| "poisoned lock")?;
            let out: Vec<Value> = batch.iter().filter_map(|r| srv.handle(r)).collect();
            (!out.is_empty()).then_some(Value::Array(out))
        }
        Ok(req) => client.server.lock().map_err(|_| "poisoned lock")?.handle(&req),
        Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
    };
    match reply {
        Some(v) => respond(&mut stream, "200 OK", "application/json", &v.to_string(), ""),
        None => respond(&mut stream, "202 Accepted", "text/plain", "", ""),
    }
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::same;

    #[test]
    fn token_compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert!(!same("", "x"));
    }
}
