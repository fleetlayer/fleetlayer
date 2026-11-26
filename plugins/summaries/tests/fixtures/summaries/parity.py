"""Compares the Rust port with claude-summarizer over every real transcript on this machine (nothing is written to the
repo). First the Rust side:
    SUMMARIES_PARITY_OUT=/tmp/rust.jsonl cargo test -p quietdeck-node parity_dump -- --ignored
then:
    CLAUDE_SUMMARIZER_SRC=~/code/claude-summarizer python3 parity.py /tmp/rust.jsonl"""
import json, os, sys, time
sys.path.insert(0, os.path.expanduser(os.environ.get('CLAUDE_SUMMARIZER_SRC', '~/code/claude-summarizer')))
from claude_summarizer import transcripts

rust = {}
with open(sys.argv[1]) as f:
    for line in f:
        r = json.loads(line)
        rust[r['path']] = r
sources = {s.path: s for s in transcripts.discover()}
same = differ = missing = 0
events_total = 0
problems = []
for path, src in sorted(sources.items()):
    if path not in rust:
        missing += 1
        problems.append(f'{path}: not read by Rust')
        continue
    res = transcripts.read(src, 0)
    r = rust[path]
    py = [[e.kind, e.text, e.end] for e in res.events]
    # Rust's output went to the file before this ran: compare only up to the shorter end (a live transcript grows).
    end = min(res.end, r['end'])
    py = [e for e in py if e[2] <= end]
    rs = [e for e in r['events'] if e[2] <= end]
    events_total += len(py)
    ok = py == rs and (res.skip or None) == r['skip'] and res.session_id == r['session_id'] and res.cwd == r['cwd']
    if ok:
        same += 1
    else:
        differ += 1
        for i, (a, b) in enumerate(zip(py, rs)):
            if a != b:
                problems.append(f'{path}: event {i}: python {a!r:.200} rust {b!r:.200}')
                break
        else:
            problems.append(f'{path}: python {len(py)} events skip={res.skip!r}, rust {len(rs)} events skip={r["skip"]!r}')
extra = set(rust) - set(sources)
print(f'{len(sources)} transcripts: {same} identical, {differ} differ, {missing} missing in Rust, {len(extra)} only in Rust; '
      f'{events_total} events compared; Rust also kept {sum(r["peers"] for r in rust.values())} messages from other sessions')
for p in problems[:20]:
    print(' ', p)
