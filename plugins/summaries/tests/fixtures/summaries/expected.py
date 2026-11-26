"""Runs claude-summarizer's own Python (github.com/ndbroadbent/claude-summarizer, ~/code/claude-summarizer) over the
fixtures and writes expected.json: the events it reads from each transcript and the condensed log it would send, so
the Rust port is tested against the original's behaviour. Run from this directory:
    CLAUDE_SUMMARIZER_SRC=~/code/claude-summarizer TZ=Pacific/Auckland python3 expected.py"""
import json, os, sys, time
sys.path.insert(0, os.path.expanduser(os.environ.get('CLAUDE_SUMMARIZER_SRC', '~/code/claude-summarizer')))
time.tzset()
from claude_summarizer import config, transcripts

config.HOME = '/Users/sam'
here = os.path.dirname(os.path.abspath(__file__))
config.CLAUDE_PROJECTS = os.path.join(here, 'home/.claude/projects')
config.CODEX_SESSIONS = os.path.join(here, 'home/.codex/sessions')
out = {}
for source in sorted(transcripts.discover(max_age_days=100000), key=lambda s: s.path):
    res = transcripts.read(source, 0)
    rel = os.path.relpath(source.path, here)
    out[rel] = {
        'agent': source.agent,
        'session_id': res.session_id,
        'cwd': res.cwd,
        'skip': res.skip,
        'end': res.end,
        'events': [{'kind': e.kind, 'text': e.text, 'end': e.end} for e in res.events],
        'is_real': transcripts.is_real(res.events),
        'condensed': transcripts.condense(res.events) if res.events else '',
        'condensed_small': transcripts.condense(res.events, budget=400) if res.events else '',
    }
with open(os.path.join(here, 'expected.json'), 'w') as f:
    json.dump(out, f, indent=1, ensure_ascii=False)
    f.write('\n')
print(f'{len(out)} transcripts')
