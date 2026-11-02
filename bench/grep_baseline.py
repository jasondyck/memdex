#!/usr/bin/env python3
"""Baseline: what an agent gets from plain grep. Rank notes by how many query words
(minus stopwords) appear in them (case-insensitive, like `grep -il word` per word)."""
import sys, os, glob, re, time
STOP = set("a an and are as at be by do does for from how i in is it me my of on or that the this to was what when where which who why with you your we our can should about use used".split())
d = os.path.expanduser(sys.argv[2])
notes = {os.path.basename(f)[:-3]: open(f).read().lower() for f in glob.glob(d + "/*.md") if not f.endswith("MEMORY.md")}
n = t1 = t3 = t6 = 0; rr = 0.0; ms = []
for line in open(sys.argv[1]):
    if not line.strip() or line.startswith("#"): continue
    cols = line.rstrip("\n").split("\t"); q, want = cols[0], cols[1]; want = [w.strip() for w in want.split(",")]
    s = time.perf_counter()
    words = [w for w in re.split(r"[^a-z0-9.]+", q.lower()) if w and w not in STOP]
    ranked = sorted(notes, key=lambda k: -sum(w in notes[k] for w in words))
    ranked = [k for k in ranked if sum(w in notes[k] for w in words) > 0][:6]
    ms.append((time.perf_counter() - s) * 1000)
    n += 1
    r = next((i for i, k in enumerate(ranked) if k in want), None)
    if r is not None:
        rr += 1 / (r + 1); t1 += r == 0; t3 += r < 3; t6 += 1
print(f"grep-baseline cases {n} | top1 {100*t1/n:.1f}% | top3 {100*t3/n:.1f}% | top6 {100*t6/n:.1f}% | MRR {rr/n:.3f} | avg {sum(ms)/n:.2f} ms")
