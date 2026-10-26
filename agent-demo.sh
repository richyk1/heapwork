#!/usr/bin/env bash
set +x
set -euo pipefail
cd "$(dirname "$0")"
# A local .env is never sent to the guest, workspace bundle, or Docker build context.
if [ -f .env ]; then set -a; source .env; set +a; fi
: "${ANTHROPIC_API_KEY:?set ANTHROPIC_API_KEY or put it in .env}"
command -v wasm-opt >/dev/null
command -v docker >/dev/null
PLATFORM=${HEAPWORK_DOCKER_PLATFORM:-linux/amd64}
IMAGE="heapwork-agent:${PLATFORM##*/}"
ROOT=$(mktemp -d /tmp/heapwork-agent.XXXXXX)
CONTAINER="heapwork-agent-$$"
cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo '=== build: Rust coding agent -> instrumented WASM -> Asyncify'
cargo build --release --quiet --locked -p agent --target wasm32-unknown-unknown
cargo build --release --quiet --locked -p host -p instrument
target/release/instrument target/wasm32-unknown-unknown/release/agent.wasm target/coder.inst.wasm
wasm-opt target/coder.inst.wasm \
  --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-mutable-globals --enable-multivalue --enable-reference-types \
  --asyncify --pass-arg=asyncify-imports@env.safepoint,env.host_call -O2 \
  -o target/coder.async.wasm
docker build --platform "$PLATFORM" -f agent.Dockerfile -t "$IMAGE" .
BIN="$PWD/target/release/heapwork"
export HEAPWORK_WASM="$PWD/target/coder.async.wasm"
export HEAPWORK_STATE="$ROOT/mac-before"
mkdir -p "$ROOT/source/src"
cat >"$ROOT/source/Cargo.toml" <<'EOF'
[package]
name = "duration-parser"
version = "0.1.0"
edition = "2021"
EOF
cat >"$ROOT/source/src/lib.rs" <<'EOF'
pub fn minutes(input: &str) -> Result<u32, String> {
    let (number, unit) = input.split_at(input.len().saturating_sub(1));
    let n: u32 = number.parse().map_err(|_| "bad number".to_string())?;
    match unit {
        "m" => Ok(n),
        "h" => Ok(n),
        _ => Err("bad unit".to_string()),
    }
}

#[test]
fn hours_to_minutes() {
    assert_eq!(minutes("2h"), Ok(120));
}
EOF
pending() {
  python3 -c 'import json,sys; p=json.load(open(sys.argv[1]))["pending"] or {}; print(p.get("op", "compute")+":"+p.get("name", ""))' "$HEAPWORK_STATE/status.json"
}
echo '=== start: real model; every step is a new process (API cap: 6 calls / $0.50)'
"$BIN" agent start "$ROOT/source" 'Fix the failing duration conversion test. Work only in src/lib.rs. For efficiency, first request read_file("src/lib.rs") and bash("cargo test") together. After inspecting their results, request write_file and bash("cargo test") together in that order, then give a short final reply. Tools execute in listed order.'
# Reach an actual mid-turn tool boundary, not a between-turn prompt boundary.
for ((i=0; i<40; i++)); do
  [ "$(pending)" = 'tool:bash' ] && break
  [ "$(pending)" != 'prompt:' ] || { echo 'turn finished before a bash handoff'; exit 1; }
  "$BIN" agent step
done
[ "$(pending)" = 'tool:bash' ] || { echo 'no bash boundary reached'; exit 1; }
echo "=== macOS $(uname -m) -> $PLATFORM: pending bash has not executed"
python3 handoff.py pack "$HEAPWORK_STATE" "$ROOT/outbound.tar.gz"
docker create --platform "$PLATFORM" --name "$CONTAINER" --entrypoint /bin/sh "$IMAGE" -c 'sleep infinity' >/dev/null
docker start "$CONTAINER" >/dev/null
docker cp handoff.py "$CONTAINER:/heapwork/handoff.py" >/dev/null
docker cp target/coder.async.wasm "$CONTAINER:/heapwork/coder.async.wasm" >/dev/null
docker cp "$ROOT/outbound.tar.gz" "$CONTAINER:/outbound.tar.gz" >/dev/null
docker exec "$CONTAINER" python3 /heapwork/handoff.py unpack /outbound.tar.gz /session
docker exec "$CONTAINER" uname -sm
# This process executes the real pending command, with no API key in the container.
docker exec "$CONTAINER" /heapwork/bin/heapwork agent step
echo '=== Linux -> macOS: workspace and live guest stack return together'
docker exec "$CONTAINER" python3 /heapwork/handoff.py pack /session /return.tar.gz
docker cp "$CONTAINER:/return.tar.gz" "$ROOT/return.tar.gz" >/dev/null
export HEAPWORK_STATE="$ROOT/mac-after"
python3 handoff.py unpack "$ROOT/return.tar.gz" "$HEAPWORK_STATE"
for ((i=0; i<40; i++)); do
  [ "$(pending)" = 'prompt:' ] && break
  "$BIN" agent step
done
[ "$(pending)" = 'prompt:' ] || { echo 'first turn did not complete'; exit 1; }
echo '=== second user turn: prior conversation stays in guest memory'
"$BIN" agent say 'In the same file, add a regression test for the same conversion bug using 0h. Use the function and file from our previous turn; do not reread them. Request write_file and bash("cargo test") together, then give a short final reply.'
for ((i=0; i<40; i++)); do
  [ "$(pending)" = 'prompt:' ] && break
  "$BIN" agent step
done
[ "$(pending)" = 'prompt:' ] || { echo 'second turn did not complete'; exit 1; }
# Independent acceptance: both the bug and the new context-dependent regression test are present.
(cd "$HEAPWORK_STATE/workspace" && cargo test --quiet)
python3 - "$HEAPWORK_STATE" <<'PY'
import json, pathlib, sys
state = pathlib.Path(sys.argv[1])
source = (state / 'workspace/src/lib.rs').read_text()
assert '0h' in source, 'second-turn regression test missing'
usage = [json.loads(line) for line in (state / 'usage.jsonl').read_text().splitlines()]
print('=== measured API usage (transfer/summarization requests: 0)')
print('model calls:', len(usage))
print('input tokens:', sum(row['usage']['input_tokens'] for row in usage))
print('output tokens:', sum(row['usage']['output_tokens'] for row in usage))
print('conservative pre-request budget:', (state / 'budget.json').read_text())
print('final snapshot bytes:', (state / 'snapshot.bin').stat().st_size)
print('artifacts:', state.parent)
PY
