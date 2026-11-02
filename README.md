# memdex

Fast, lightweight shared memory for AI agents: full-text search over markdown memory
notes, plus a shared vault any agent can write to — served over MCP (stdio or HTTP).
No daemon needed, no LLM, no embeddings, no database file. Two crates: `rusqlite` (bundled
SQLite) and `serde_json`. Optional static embeddings with no extra crates.

## What it indexes
- Every Claude Code memory dir (`~/.claude/projects/*/memory/*.md`) — read-only.
- The shared vault `~/memory/` — written by `remember`, git-tracked (each write is a commit
  authored `memdex/<agent>`), plain markdown with Claude-memory-style frontmatter.
- `MEMORY.md` and `README.md` are skipped.

Index lives in RAM: SQLite FTS5 (porter stemming, BM25 weighted name 4 / heading 3 /
hidden note-description 1 / body 1) plus a trigram table for exact identifiers (IPs, hostnames).
Every call re-stats the files and re-indexes only what changed, so results are never stale.
Notes are chunked by heading, splitting only sections over 4000 chars; at most 2 sections per
note per result list. Snippets are query-focused: the lines with the most query words, in order.

## Tools
| Tool | |
|---|---|
| `search {query, limit?, project?, full?}` | top 3 notes as `[id] project/file › heading` + the lines matching the query (≤1200 chars each) |
| `read {id}` / `read {file, section?, project?}` | one section, a note, or just the parts mentioning `section` |
| `remember {topic, fact, description?, type?}` | append a dated, agent-tagged bullet to `~/memory/<topic>.md` (only when the agent has a write dir) |

`remember` refuses credential-looking text (known token prefixes, `password=`/`?token=` values,
PEM blocks, long mixed-case random strings, "password is …") and facts over 2000 chars.

## Per-agent configs (`--config FILE`)
```text
agent helper-bot          # name stamped on everything it remembers
claude-dirs               # all ~/.claude/projects/*/memory
dir ~/memory shared       # extra dir + label
write-dir ~/memory        # enables `remember`
exclude payroll           # hide a note (or label/note) from this agent entirely
token <48 hex>            # HTTP clients only; file must be chmod 600
read-only
```
No `--config` = Claude Code on this machine: all Claude dirs + `~/memory`, writable, agent `claude`.

## Run
```sh
memdex serve [--config F]                                    # stdio MCP (Claude Code, other agents)
memdex http --listen 127.0.0.1:8765 --client client.conf     # HTTP MCP, bearer token per client
memdex search ci cache key
memdex read build-cache gotcha
memdex remember build-cache "Cache is now keyed on the lockfile hash" --agent me
memdex bench bench/example/cases.tsv --dir bench/example/notes
```

## Benchmarks (8-core desktop, 2026-11)
Measured on a private corpus of ~80 real agent-memory notes. The question sets are not
published; `bench/example/` has a
small fictional set in the same format (`query<TAB>expected-note[,alt]<TAB>answer substring`).
- dev set (50): written by hand — half in the notes' own words, half paraphrased.
- held-out set (87): written by a separate agent from the notes alone, without running any
  search, ~70% paraphrased, each with an exact answer string. Split in two: 44 joined tuning,
  43 were scored once at the end.
- Metrics: top-k / MRR = right note ranked; **ans** = the answer string is actually inside the
  text the agent receives. Baseline = rank notes by how many query words they contain
  (`bench/grep_baseline.py`, roughly what grep gives an agent).

Tuning: 672 + 768-config grid sweeps (BM25 weights, prefix, stopwords, chunk size, per-note cap,
hidden context column, focused snippets, snippet length, result count) on dev + heldout-tune.
Lesson: the dev-set winner (whole-note chunks, first-N-chars snippets) collapsed answer delivery
on the independent set (36–44% vs 56%) — query-focused snippets are what made big chunks work.

Final, on the untouched 43 questions:

| | top-1 | top-3 | MRR | **ans** | output |
|---|---|---|---|---|---|
| v0.2 defaults (6 × 500 chars, section chunks) | 74% | 91% | 0.80 | 60% | 2.8k chars |
| **current** (3 × 1200 focused, note chunks, ctx column) | 70% | 91% | 0.80 | **81%** | 2.8k chars |
| grep baseline | 40% | 58% | 0.51 | – | – |

Remaining misses are vocabulary gaps (e.g. "make the build faster" vs a note that only says
"CI cache"). Closing them needs synonyms or static embeddings.

Speed & size (synthetic copies of the notes):

| notes | build at start | query p50 / p95 | RSS |
|---|---|---|---|
| 83 (today) | 49 ms | 0.7 / 2.0 ms | 12 MB |
| 830 | 0.63 s | 3.3 / 14 ms | 37 MB |
| 4,150 | 3.8 s | 15 / 68 ms | 69 MB |

Binary 3.2 MB; `remember` 7 ms including the git commit.
Run your own: `memdex bench CASES.tsv --dir YOUR/MEMORY/DIR`
(`--tune "w=4,3,1 wc=1 focus=1 chunk=4000,3000 per=2 stop=1" --limit 3 --snip 1200` = defaults).

## Static embeddings (opt-in, off by default)
`embed.rs` loads Model2Vec "potion" models (static token→vector tables, no neural net at
query time) with a from-scratch safetensors reader + BERT WordPiece — verified byte-identical to
the Python `model2vec` library (token ids and vectors). Enable per agent with `model DIR` in its
config or `--model DIR`; tune with `sem` (RRF weight), `rrf` (k), `pass` (passage chars),
`sfocus` (pick snippet lines by similarity).

Benchmarked 2026-11-02: 337-config sweep on the 94-question tuning set, then a fresh
**vocabulary-gap** set (60 Qs, 45 avoid the notes' words), written independently:

| on heldout2 | top-1 | top-3 | MRR | ans | RSS |
|---|---|---|---|---|---|
| **words only (default)** | 70% | 82% | 0.756 | 70% | 13 MB |
| potion-base-4M, snippet lines only | 70% | 82% | 0.756 | 68% | 32 MB |
| potion-base-8M hybrid (sem 0.2, k 5) | 72% | 82% | 0.764 | 68% | 47 MB |
| potion-retrieval-32M hybrid (best) | 73% | 83% | 0.778 | 68% | 145 MB |
| grep baseline | 33% | 57% | 0.473 | – | – |

All within ±2 questions of words-only; fusion slightly hurt on the tuning set. The remaining misses
need world knowledge (a question naming what an app *does* when the note only names the app),
which a static table can't supply — the calling LLM rewriting the question into
key nouns is the cheaper fix. Models (MIT) live in `~/.local/share/memdex/models/`.

## Install
Prebuilt static binaries (Linux x86_64 and aarch64, no dependencies) are attached to each
[GitHub release](https://github.com/jasondyck/memdex/releases):
```sh
curl -fsSL https://raw.githubusercontent.com/jasondyck/memdex/main/install.sh | sh
claude mcp add --scope user memdex -- ~/.local/bin/memdex serve
```
The script picks your architecture, verifies the SHA-256 checksum and installs to
`~/.local/bin` (`MEMDEX_VERSION=v0.4.1` pins a release, `MEMDEX_BIN_DIR` changes the location).
Prefer not to pipe to `sh`? Download `memdex-<arch>-unknown-linux-musl.tar.gz` and its `.sha256`
from the release page, run `sha256sum -c`, and copy `memdex` onto your PATH.

### From source
Needs Rust ≥ 1.89 and a C compiler (SQLite is compiled in). On Ubuntu the apt `cargo` is too old;
use rustup:
```sh
sudo apt install build-essential curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # then open a new shell
cargo build --release && install -Dm755 target/release/memdex ~/.local/bin/memdex
```
`deploy/` has a systemd user timer for nightly backups of `~/memory` and the Claude memory dirs
(`MEMDEX_BACKUP_DIR`), and a unit for the HTTP transport.

## Releasing
CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on every push. To release, bump
`version` in `Cargo.toml`, commit, then `git tag vX.Y.Z && git push --tags`:
`release.yml` checks the tag matches `Cargo.toml`, runs the tests on each architecture, builds
static musl binaries, and publishes them with checksums. It uses no third-party actions.
