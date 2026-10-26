#!/usr/bin/env bash
# heapwork demo: a Rust agent compiled to WebAssembly is paused (even mid-loop), its heap is
# serialized, and a different OS process picks the session up. Every [pid N] line is a new process.
set -euo pipefail
cd "$(dirname "$0")"

command -v wasm-opt >/dev/null || { echo "needs wasm-opt (brew install binaryen)"; exit 1; }
rustup target list --installed | grep -q wasm32-unknown-unknown \
  || { echo "needs: rustup target add wasm32-unknown-unknown"; exit 1; }

echo "=== 0. build: Rust agent -> wasm -> safepoints in every loop -> Asyncify"
cargo build --release --quiet -p guest --target wasm32-unknown-unknown
cargo build --release --quiet -p instrument -p host
./target/release/instrument target/wasm32-unknown-unknown/release/guest.wasm target/agent.inst.wasm
wasm-opt target/agent.inst.wasm \
  --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-mutable-globals --enable-multivalue --enable-reference-types \
  --asyncify --pass-arg=asyncify-imports@env.safepoint,env.llm -O2 \
  -o target/agent.async.wasm
ls -l target/wasm32-unknown-unknown/release/guest.wasm target/agent.async.wasm | awk '{print "  " $5 " bytes  " $9}'
BIN=target/release/heapwork

# Run one slice per process until the agent finishes; answer llm() calls as "<tag> reply N".
drive() {
  local tag=$1 n=0 out
  until [ -f "$HEAPWORK_STATE/done.json" ]; do
    out=$($BIN step)
    echo "$out"
    if grep -q "waiting on llm" <<<"$out"; then
      n=$((n + 1))
      $BIN step "$tag reply $n"
    fi
  done
}

rm -rf /tmp/heapwork-a /tmp/heapwork-b /tmp/heapwork-ref

echo
echo "=== 1. start a session; it is preempted mid-loop, serialized, and the process exits"
export HEAPWORK_STATE=/tmp/heapwork-a
$BIN start "plan a trip"

echo
echo "=== 2. hand the session off from process to process until it parks on llm()"
while true; do
  out=$($BIN step)
  echo "$out"
  grep -q "parked on llm" <<<"$out" && break
done

echo
echo "=== 3. fork: copy the parked session (one file) to a second agent"
cp -R /tmp/heapwork-a /tmp/heapwork-b
ls -l /tmp/heapwork-b | awk 'NR>1 {print "  " $5 " bytes  " $9}'

echo
echo "=== 4. finish session A"
drive A >/dev/null
cat /tmp/heapwork-a/done.json

echo
echo "=== 5. finish session B with different llm replies (same past, different future)"
HEAPWORK_STATE=/tmp/heapwork-b drive B >/dev/null
cat /tmp/heapwork-b/done.json

echo
echo "=== 6. reference: one process per llm() reply, no preemption; must match session A exactly"
HEAPWORK_STATE=/tmp/heapwork-ref HEAPWORK_SLICE=0 bash -c "$BIN start 'plan a trip' >/dev/null"
HEAPWORK_STATE=/tmp/heapwork-ref HEAPWORK_SLICE=0 drive A >/dev/null
diff /tmp/heapwork-a/done.json /tmp/heapwork-ref/done.json >/dev/null \
  && echo "sliced run == uninterrupted run: identical transcripts" \
  || { echo "MISMATCH"; exit 1; }
