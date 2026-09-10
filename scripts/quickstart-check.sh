#!/bin/bash
# scripts/quickstart-check.sh — the README quickstart, RE-EXECUTED AS WRITTEN and ASSERTED (rule 23 and its
# 2026-09-09 corollary: an executed procedure is a test only if it asserts the observable the user came for).
# The 2026-08-24 quickstart "passed" on HTTP 200 over a zero-token stream. This runner asserts, in order:
#   (1) the stream carried tokens            — text events > 0
#   (2) the stream said why it ended         — an `event: finish` is present
#   (3) it ended at the model's EOS          — the finish reason is `stop` (not `length`, not absent)
#   (4) without the token the API answers 401
# Usage: scripts/quickstart-check.sh [pairing-dir]   (default ~/.hydra; a pre-existing dir is moved aside)
set -u
DIR="${1:-$HOME/.hydra}"; HERE="$(cd "$(dirname "$0")/.." && pwd)"; cd "$HERE"
OUT="${QUICKSTART_OUT:-$(mktemp -d)}"; echo "quickstart-check: logs in $OUT"
echo "=== QUICKSTART $(date -u +%FT%TZ) tree=$(git rev-parse --short HEAD) dirty=$(git status --porcelain | wc -l | tr -d ' ')"
if [ -e "$DIR" ]; then mv "$DIR" "$DIR.bak-$(date -u +%Y%m%dT%H%M%SZ)"; echo "moved a pre-existing $DIR aside"; fi
echo "--- 1. Build"; cargo build --release > "$OUT/build.log" 2>&1; echo "cargo build --release exit=$?"
export PATH="$PWD/target/release:$PATH"
echo "--- 2a. pair"; hydra-cli pair --out "$DIR" > "$OUT/pair.log" 2>&1; echo "pair exit=$?"; grep -E "PIN:|API token" "$OUT/pair.log"
echo "--- 2b. provision"; hydra-cli provision --pairing-dir "$DIR" --model models/qwen2.5-0.5b-instruct-fp16.gguf --stages worker-s1=127.0.0.1:9001,worker-s2=127.0.0.1:9002 > "$OUT/provision.log" 2>&1; echo "provision exit=$?"
echo "--- 3. workers + coordinator"
hydra-worker "$DIR/worker-s1.boot" > "$OUT/worker-s1.log" 2>&1 & W1=$!
hydra-worker "$DIR/worker-s2.boot" > "$OUT/worker-s2.log" 2>&1 & W2=$!
sleep 3
hydra-coordinator --pairing-dir "$DIR" --api-addr 127.0.0.1:8443 --data-dir "$DIR/data" > "$OUT/coordinator.log" 2>&1 & C=$!
for i in $(seq 1 90); do grep -q "API listening" "$OUT/coordinator.log" 2>/dev/null && break; sleep 1; done
grep "API listening" "$OUT/coordinator.log" || echo "coordinator never listened"
echo "--- 4. status"; hydra-cli status --data-dir "$DIR/data"; echo "status exit=$?"
echo "--- 5. talk to it $(date -u +%FT%TZ)"
curl -sS --max-time 180 --cacert "$DIR/cluster-ca.pem" https://127.0.0.1:8443/v1/chat/completions \
  -H "authorization: Bearer $(cat "$DIR/api-token")" -H 'content-type: application/json' \
  -d '{"messages":[{"role":"user","content":"hello"}],"stream":true}' -w '\nHTTP %{http_code}\n' > "$OUT/curl.out" 2>&1
echo "curl exit=$? $(date -u +%FT%TZ)"
HTTP=$(grep -E '^HTTP [0-9]+$' "$OUT/curl.out" | tail -1 | awk '{print $2}')
# SSE: a finish event is a block with `event: finish`; text events are the rest. A bare-newline text arrives as
# two empty data lines; count blocks, not lines.
python3 - "$OUT/curl.out" > "$OUT/sse.json" <<'PY'
import sys, json
raw=open(sys.argv[1], errors='replace').read().replace('\r\n','\n')
# curl writes the bare SSE body (no -i): every blank-line-separated block is one event; the
# trailing 'HTTP <code>' line is the -w footer. The first block is an event, not a header.
body=raw.rsplit('\nHTTP ',1)[0] if '\nHTTP ' in raw else raw
text_events=0; finish=None; text=''
for block in body.replace('\r\n','\n').split('\n\n'):
    ev=None; data=[]
    for line in block.split('\n'):
        if line.startswith('event:'): ev=line[6:].strip()
        elif line.startswith('data:'):
            v=line[5:]; data.append(v[1:] if v.startswith(' ') else v)   # ONE space after the colon is the SSE separator, not text
    if not data and ev is None: continue
    if ev=='finish': finish='\n'.join(data)
    elif data: text_events+=1; text+='\n'.join(data)
print(json.dumps({'text_events':text_events,'finish':finish,'text':text}))
PY
cat "$OUT/sse.json"
echo "--- 5b. without the token (README: 401)"
HTTP2=$(curl -sS --cacert "$DIR/cluster-ca.pem" https://127.0.0.1:8443/v1/chat/completions -H 'content-type: application/json' -d '{"messages":[{"role":"user","content":"hello"}],"stream":true}' -o /dev/null -w '%{http_code}')
echo "HTTP $HTTP2"
echo "--- teardown"; kill $C $W1 $W2 2>/dev/null; wait 2>/dev/null
# ---- the assertions ----
EVENTS=$(python3 -c "import json;print(json.load(open('$OUT/sse.json'))['text_events'])")
FINISH=$(python3 -c "import json;print(json.load(open('$OUT/sse.json'))['finish'] or 'ABSENT')")
FAIL=0
[ "$HTTP" = "200" ] || { echo "ASSERT FAILED: HTTP $HTTP (wanted 200)"; FAIL=1; }
[ "$EVENTS" -gt 0 ] 2>/dev/null || { echo "ASSERT FAILED: text events = $EVENTS (wanted > 0) — a 200 over an empty stream is the 2026-08-24 shape"; FAIL=1; }
[ "$FINISH" != "ABSENT" ] || { echo "ASSERT FAILED: no finish event"; FAIL=1; }
[ "$FINISH" = "stop" ] || { echo "ASSERT FAILED: finish_reason=$FINISH (wanted stop: the stream must end at the model's EOS)"; FAIL=1; }
[ "$HTTP2" = "401" ] || { echo "ASSERT FAILED: without the token HTTP $HTTP2 (wanted 401)"; FAIL=1; }
if [ "$FAIL" = 0 ]; then echo "verdict=GREEN (quickstart: HTTP 200, $EVENTS text events, finish_reason=stop, 401 without the token)"; else echo "verdict=RED (quickstart assertions failed — see above)"; fi
echo "=== END $(date -u +%FT%TZ)"; exit $FAIL
