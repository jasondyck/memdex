# memdex

Fast, lightweight shared memory for AI agents: full-text search over markdown memory
notes, plus a shared vault any agent can write to — served over MCP (stdio or HTTP).
No daemon needed, no LLM, no embeddings, no database file, no C code: pure Rust with one
dependency (`serde_json`). Optional static embeddings with no extra crates.

## What it indexes
- Every Claude Code memory dir (`~/.claude/projects/*/memory/*.md`) — read-only.
- The shared vault `~/memory/` — written by `remember`, git-tracked (each write is a commit
  authored `memdex/<agent>`), plain markdown with Claude-memory-style frontmatter.
- `MEMORY.md` and `README.md` are skipped.
- Optionally, **Claude Code session transcripts** (`~/.claude/projects/*/*.jsonl`) as a separate
  "history" index — see below.

Index lives in RAM: a small inverted index (`src/fts.rs`) that reproduces SQLite FTS5's
`porter unicode61` tokenizer and BM25 exactly (weighted name 4 / heading 3 / hidden
note-description 1 / body 1), plus case-insensitive substring search for exact identifiers.
Queries containing an identifier (IP, `host.domain`, `:port`, `user@host`) rank exact matches
first, since the word index would split `192.0.2.10` into tokens that match everywhere. It
replaced SQLite in 0.5; see "Pure-Rust index" below for the parity checks.
Every call re-stats the files and re-indexes only what changed, so results are never stale.
Notes are chunked by heading, splitting only sections over 4000 chars; at most 2 sections per
note per result list. Snippets are query-focused: the lines with the most query words, in order.

## Tools
| Tool | |
|---|---|
| `search {query, limit?, project?, full?, source?}` | top 3 notes as `[id] project/file › heading` + the lines matching the query (≤1200 chars each) |
| `read {id}` / `read {file, section?, project?}` | one section, a note, or just the parts mentioning `section` |
| `remember {topic, fact, description?, type?}` | append a dated, agent-tagged bullet to `~/memory/<topic>.md` (only when the agent has a write dir) |

`remember` refuses credential-looking text (known token prefixes, `password=`/`?token=` values,
PEM blocks, long mixed-case random strings, "password is …") and facts over 2000 chars.

## Past conversations (transcripts)
`search` with `source: "history"` searches Claude Code's session transcripts; `"all"` merges them
with the notes (reciprocal rank), `"notes"` is the default. Only the conversation is indexed —
your prompts and Claude's text replies, about 3% of a transcript; tool calls, tool output, thinking,
sidechains and injected system text are skipped. Each prompt plus its replies is one section,
labelled `project/session-<id> › date · prompt`.

- Built on the first history search (≈0.6 s for ~560 MB of transcripts here), then incremental:
  files are append-only, so only new lines are read and only the still-open exchange is re-indexed.
  ~12 MB extra RSS for ~300 sessions; queries ~1–2 ms.
- On by default only without `--config` (Claude itself); other agents need the `transcripts`
  directive, because transcripts contain everything, including work sessions. `--no-transcripts`
  turns it off.
- Claude Code deletes transcripts after `cleanupPeriodDays` (default 30), so history is a rolling
  window. Durable facts belong in notes (`remember`), which do not expire.
- Notes stay the default because they are curated: on the note question sets, `source: all`
  keeps the same top-1 but costs 1–5 points of top-3 (transcript exchanges take the extra slots).

## Per-agent configs (`--config FILE`)
```text
agent helper-bot          # name stamped on everything it remembers
claude-dirs               # all ~/.claude/projects/*/memory
dir ~/memory shared       # extra dir + label
write-dir ~/memory        # enables `remember`
exclude payroll           # hide a note (or label/note) from this agent entirely
token <48 hex>            # HTTP clients only; file must be chmod 600
transcripts               # also search Claude Code session transcripts (source: history|all)
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
memdex update [--check] [--version vX.Y.Z]
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
| 83 | 28 ms | 0.22 / 0.32 ms | 6 MB |
| 830 | 0.23 s | 1.9 / 2.6 ms | 16 MB |
| 4,150 | 1.1 s | 10.5 / 14.6 ms | 61 MB |

Static binary ~1 MB; `remember` 7 ms including the git commit.
Run your own: `memdex bench CASES.tsv --dir YOUR/MEMORY/DIR`
(`--tune "w=4,3,1 wc=1 focus=1 chunk=4000,3000 per=2 stop=1" --limit 3 --snip 1200` = defaults).

## Pure-Rust index (0.5)
SQLite FTS5 was replaced by `src/fts.rs` + `src/porter.rs` after three checks against the SQLite
version on the same private corpus:
1. **Tokens:** identical vocabulary (7,459 terms) and per-term counts (83,923 tokens) to
   `porter unicode61 remove_diacritics 2`, over all notes plus an accented / non-Latin stress text.
2. **Rankings:** 352 queries (word and identifier) return the same top-6 lists; the only
   difference was one exact score tie ordered differently.
3. **Benchmarks:** identical accuracy on both held-out sets; versus SQLite, queries are ~4.5×
   faster (p50 0.97 → 0.22 ms), startup ~2× faster, RSS 12.4 → 6 MB, binary 3.1 → 1 MB, and a clean
   release build takes 3.4 s instead of 46 s. Static musl binaries link with Rust's bundled
   `rust-lld` (`.cargo/config.toml`), so no musl toolchain is needed and aarch64 cross-builds from x86.

Also in 0.5: identifier-first ranking. On 216 lookups generated from every IP / hostname / port in
the corpus, top-1 rose from 85% to 95% and top-3 from 97% to 100%, with no change on the other
question sets (an earlier, broader rule that also caught versions like `7.1` and paths cost 3
questions there and was narrowed).

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
Prebuilt binaries for Linux and Windows (x86_64 and ARM64, no dependencies) are attached to
each [GitHub release](https://github.com/jasondyck/memdex/releases).

**Packages** (from the release page):
- Windows: `memdex-<version>-x64-setup.exe` / `-arm64-setup.exe` — per-user installer, no admin
  rights, adds memdex to your PATH, uninstall from *Apps & features*. The installer is not
  code-signed, so SmartScreen will warn ("More info" → "Run anyway").
- Debian / Ubuntu: `sudo apt install ./memdex_<version>_amd64.deb` (or `_arm64.deb`); installs
  `/usr/bin/memdex`, remove with `sudo apt remove memdex`.

**Windows** (PowerShell script, no SmartScreen prompt):
```powershell
irm https://raw.githubusercontent.com/jasondyck/memdex/main/install.ps1 | iex
claude mcp add --scope user memdex -- "$env:LOCALAPPDATA\Programs\memdex\memdex.exe" serve
```
Installs to `%LOCALAPPDATA%\Programs\memdex` (added to your user PATH; `MEMDEX_NO_PATH=1` to skip)
after verifying the SHA-256. Memory lives in the same places as on Linux: Claude Code's
`%USERPROFILE%\.claude\projects\*\memory` and the shared vault `%USERPROFILE%\memory`.

**Linux:**
```sh
curl -fsSL https://raw.githubusercontent.com/jasondyck/memdex/main/install.sh | sh
claude mcp add --scope user memdex -- ~/.local/bin/memdex serve
```
The script picks your architecture, verifies the SHA-256 checksum and installs to
`~/.local/bin` (`MEMDEX_VERSION=v0.4.1` pins a release, `MEMDEX_BIN_DIR` changes the location).
Prefer not to pipe to `sh`? Download `memdex-<arch>-unknown-linux-musl.tar.gz` and its `.sha256`
from the release page, run `sha256sum -c`, and copy `memdex` onto your PATH.

### Updating
```sh
memdex update            # install the latest release (verifies the SHA-256 first)
memdex update --check    # only report whether a newer release exists
memdex update --version v0.5.2   # install a specific release (also allows downgrades)
```
It respects how memdex was installed: a plain binary (install.sh / install.ps1 / archive) is
swapped in place — atomically on Linux, so running MCP sessions keep the old copy until they
restart; on Windows the running exe is renamed aside and removed on the next start. A Windows
`setup.exe` install is updated by running the new installer silently; a `.deb` (`/usr/bin`) is
left to the package manager and the command prints the `apt install` line. Uses the system
`curl` and `tar` (both ship with Windows 10+). Without `--version` it never downgrades.

### From source
Needs Rust ≥ 1.89. No C code is compiled, but like most Rust projects the build uses the
system linker (`cc`). On Ubuntu the apt `cargo` is too old; use rustup:
```sh
sudo apt install gcc curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # then open a new shell
cargo build --release && install -Dm755 target/release/memdex ~/.local/bin/memdex
```
Windows: the same, with the MSVC toolchain (`rustup` default on Windows); the binary links the C
runtime statically, so it needs no VC++ redistributable. `deploy/` (Linux only) has a systemd user
timer for nightly backups of `~/memory` and the Claude memory dirs
(`MEMDEX_BACKUP_DIR`), and a unit for the HTTP transport.

## Releasing
CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on Linux and Windows on every push, and
on Windows also builds a release-style zip and runs `install.ps1` against it, and compiles the
Inno Setup installer and installs / uninstalls it silently (checking files and the PATH entry). To release, bump
`version` in `Cargo.toml`, commit, then `git tag vX.Y.Z && git push --tags`:
`release.yml` checks the tag matches `Cargo.toml`, runs the tests on each architecture, builds
static binaries (Linux musl, Windows MSVC with static CRT), .deb packages and Windows
setup.exe installers, and publishes them all with checksums. It uses no third-party actions.

## License
Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
