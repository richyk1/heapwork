# heapwork

Pause a running Rust program, even partway through a loop, save its WebAssembly execution state, and continue in another process.

The toy demo produces ~20 KB snapshots. The coding-agent demo also saves its conversation and live tool-dispatch stack. Neither transfer asks a model to summarize.

## Run it

```sh
rustup target add wasm32-unknown-unknown
brew install binaryen        # for wasm-opt
./demo.sh
```

Once the demo has built everything, you can drive a session by hand. Every command is a fresh process:

```sh
export HEAPWORK_STATE=/tmp/session
target/release/heapwork start "plan a trip"
target/release/heapwork step              # repeat; each run continues where the last stopped
target/release/heapwork step "a reply"    # answers the agent when it is waiting on llm()
```

## What the demo shows

1. **Handoff mid-loop.** The agent runs a 2,000,000-iteration loop that never yields. The host interrupts it, saves it, and exits; the next process picks the loop up at the same iteration.
2. **Handoff mid-call.** The agent calls `llm()` like an ordinary function. The process saves the paused call and exits; a later process supplies the reply, and the call returns.
3. **Fork.** Copying the snapshot file gives two sessions with the same history that diverge from there.
4. **No drift.** The final transcript is byte-identical to a run that was never interrupted.

Measured on an M-series Mac:

| | |
|---|---|
| Snapshot, paused in a loop | 20,610 bytes |
| Snapshot, waiting on `llm()` | 24,755 bytes |
| Save | ~0.08 ms |
| Resume (load cached module + restore state) | ~0.6 ms |

## Real coding-agent demo

```sh
export ANTHROPIC_API_KEY=...  # or use a git-ignored .env
./agent-demo.sh              # requires Docker; builds a Linux amd64 host inside rust:1.97-bookworm
```

`agent/` holds Anthropic Messages history in WASM memory and uses real `read_file`, `write_file`, `list_dir`, and timed `bash` tools. Each command restores a fresh instance:

```sh
export HEAPWORK_STATE=/tmp/coding-session
target/release/heapwork agent start /path/to/workspace "Fix the failing test"
target/release/heapwork agent step          # execute one pending host operation, then snapshot and exit
target/release/heapwork agent say "Add a regression test for that fix"
```

`start` copies the workspace into `$HEAPWORK_STATE/workspace`; work happens there. It refuses an existing state directory and excludes `.git`, `target`, `.env`, and `.env.*`. The snapshot carries guest state and its pending request, not workspace files. `handoff.py pack/unpack` moves both together and rejects links, traversal paths, special files, and existing restore destinations.

The script hands a pending test command from macOS arm64 to an emulated Linux x86-64 process, then returns the workspace and snapshot to macOS. A second user turn uses the previous conversation. It queries available models once, preferring the newest Haiku. `HEAPWORK_MODEL` overrides selection. Every Messages request reserves a conservative token-cost bound before sending; the session stops at six requests or a $0.50 bound. There are no automatic HTTP retries.

Transfer uses **zero extra model requests**, not free future inference: normal model calls still pay for their context. The script prints usage for every model call. Snapshot size grows with conversation and guest memory.

Measured with `claude-haiku-5-5`: two turns used five model calls, 9,320 input tokens and 1,185 output tokens. The conservative pre-request reservation was $0.356085 (not a billing measurement).

| Coding-agent state | Measured |
|---|---|
| Snapshot at outbound tool boundary | 41,174 bytes |
| Outbound bundle, including workspace | 7,738 bytes |
| Linux x86-64 replay: cold module / state restore | 692.59 ms / 1.15 ms |
| macOS cached module / state restore | ~1.1 ms / 0.01–0.02 ms |
| Final snapshot after two turns | 98,529 bytes |

The full run passed both final workspace tests. Its first Linux command exposed a stale cached binary and a rustup configuration error. After fixing build freshness, replaying the same saved pending command on Linux produced the expected failing assertion (`Ok(2)` versus `Ok(120)`), without another model call.

## How it works

| Piece | Role |
|---|---|
| `guest/`, `agent/` | The toy program and real coding-agent loop, respectively; plain Rust compiled to `wasm32-unknown-unknown`. |
| `instrument/` | Adds a check to every loop so the host can interrupt it, and exports the globals a snapshot needs (such as the stack pointer). |
| `wasm-opt --asyncify` | Rewrites the module so its call stack can be unwound into linear memory and rebuilt later. |
| `host/` | Runs the module on [wasmtime](https://wasmtime.dev). To pause, it unwinds the stack and writes the 4 KiB memory pages that changed since startup, plus the globals. To resume, it writes them into a fresh instance and rewinds. |

WebAssembly is what makes this possible. Every pointer in the program is an offset into a single block of memory, so that memory can be copied out and loaded at any address, in any process. The call stack, which would normally live in CPU registers and the native stack, ends up in that memory too, so a single file holds the whole paused program.

## Limits

- A snapshot resumes only with the exact same `.wasm` build and compatible snapshot format.
- The toy `./demo.sh` uses canned replies; `./agent-demo.sh` uses the real Anthropic API.
- Parking happens before external effects. The guest stack survives; in-flight HTTP connections, provider streams, model/KV-cache state, and running subprocesses do not.
- `bash` starts in the workspace but is **not a security sandbox**. Use trusted prompts/workspaces. Its environment excludes the API key; filesystem access still has the host process's permissions.
- Completed side effects are not transactionally coupled to snapshot writes. A crash after a tool runs but before the next snapshot can repeat that tool on retry.
