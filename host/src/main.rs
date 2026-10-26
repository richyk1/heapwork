//! heapwork: pause a Rust program compiled to WebAssembly anywhere (even mid-loop), save it as a
//! few KB, and resume it in another process: a session handoff by serializing the heap.
//!
//!   heapwork start "<goal>"   instantiate, start run(goal), run one slice
//!   heapwork step [answer]    restore the snapshot, run one slice
//!
//! A snapshot is the guest's linear memory plus its mutable globals. The call stack is in there
//! too: Asyncify unwinds it into linear memory (the guest's `ASYNCIFY_BUF`) before we copy.
//! Only the 4 KiB pages that differ from a freshly instantiated module are written: the module's
//! data segments are a base every machine can rebuild, so a snapshot is just the delta.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process,
    time::Instant,
};

use wasmtime::{AsContextMut, Caller, Engine, Extern, Func, Instance, Linker, Module, Store, Val};

mod agent;

const MAGIC: &[u8; 8] = b"HWWASM1\n";
const PAGE: usize = 4096;
const WASM_PAGE: usize = 65536;
/// `asyncify_get_state()` values.
const UNWINDING: i32 = 1;
const REWINDING: i32 = 2;

type Result<T, E = wasmtime::Error> = std::result::Result<T, E>;

fn err(msg: impl std::fmt::Display) -> wasmtime::Error {
    wasmtime::Error::msg(msg.to_string())
}

/// Host-side state of one process. Nothing here outlives the process except what `save` writes.
#[derive(Default)]
struct Host {
    budget: u32,
    ticks: u32,
    buf: i32,
    pending: Option<String>,
    reply: Option<String>,
}

fn invoke(mut ctx: impl AsContextMut<Data = Host>, f: &Func, args: &[Val]) -> Result<Option<Val>> {
    let mut out = vec![Val::I32(0); f.ty(&ctx).results().len()];
    f.call(&mut ctx, args, &mut out)?;
    Ok(out.into_iter().next())
}

fn guest_call(c: &mut Caller<'_, Host>, name: &str, args: &[Val]) -> Result<Option<Val>> {
    let f = c.get_export(name).and_then(Extern::into_func).ok_or_else(|| err(format!("no export {name}")))?;
    invoke(c, &f, args)
}

fn asyncify_state(c: &mut Caller<'_, Host>) -> Result<i32> {
    guest_call(c, "asyncify_get_state", &[])?.and_then(|v| v.i32()).ok_or_else(|| err("bad asyncify state"))
}

fn start_unwind(c: &mut Caller<'_, Host>) -> Result<()> {
    let buf = c.data().buf;
    guest_call(c, "asyncify_start_unwind", &[Val::I32(buf)]).map(drop)
}

fn linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    // Injected at every loop header, checked every 10k iterations. Preempts by unwinding the
    // whole stack once the slice budget is used up.
    linker.func_wrap("env", "safepoint", |mut c: Caller<'_, Host>| -> Result<()> {
        if asyncify_state(&mut c)? == REWINDING {
            // The stack is rebuilt up to the call that unwound: continue from here.
            return guest_call(&mut c, "asyncify_stop_rewind", &[]).map(drop);
        }
        // host_result must copy a resumed response before another snapshot can be taken.
        if c.data().reply.is_some() { return Ok(()); }
        let h = c.data_mut();
        h.ticks += 1;
        if h.budget > 0 && h.ticks >= h.budget {
            start_unwind(&mut c)?;
        }
        Ok(())
    })?;
    // Durable host call: the first call records the prompt and unwinds; after a restore, the
    // same call is replayed by the rewind and returns the reply.
    linker.func_wrap(
        "env",
        "llm",
        |mut c: Caller<'_, Host>, prompt: i32, prompt_len: i32, out: i32, out_cap: i32| -> Result<i32> {
            let mem = c.get_export("memory").and_then(Extern::into_memory).ok_or_else(|| err("no memory"))?;
            if asyncify_state(&mut c)? == REWINDING {
                guest_call(&mut c, "asyncify_stop_rewind", &[])?;
                let reply = c.data_mut().reply.take().ok_or_else(|| err("llm() resumed without a reply"))?;
                let n = reply.len().min(out_cap as usize);
                mem.write(&mut c, out as usize, &reply.as_bytes()[..n])?;
                return Ok(n as i32);
            }
            let mut bytes = vec![0u8; prompt_len as usize];
            mem.read(&c, prompt as usize, &mut bytes)?;
            c.data_mut().pending = Some(String::from_utf8_lossy(&bytes).into_owned());
            start_unwind(&mut c)?;
            Ok(0)
        },
    )?;
    linker.func_wrap("env", "host_call", |mut c: Caller<'_, Host>, ptr: i32, len: i32| -> Result<i32> {
        if asyncify_state(&mut c)? == REWINDING {
            guest_call(&mut c, "asyncify_stop_rewind", &[])?;
            return c.data().reply.as_ref().map(|s| s.len() as i32).ok_or_else(|| err("host call resumed without response"));
        }
        let mem = c.get_export("memory").and_then(Extern::into_memory).ok_or_else(|| err("no memory"))?;
        let mut request = vec![0; len as usize];
        mem.read(&c, ptr as usize, &mut request)?;
        c.data_mut().pending = Some(String::from_utf8(request)?);
        start_unwind(&mut c)?;
        Ok(0)
    })?;
    linker.func_wrap("env", "host_result", |mut c: Caller<'_, Host>, ptr: i32, len: i32| -> Result<()> {
        let response = c.data_mut().reply.take().ok_or_else(|| err("no host response"))?;
        if response.len() != len as usize { return Err(err("response buffer length mismatch")); }
        let mem = c.get_export("memory").and_then(Extern::into_memory).ok_or_else(|| err("no memory"))?;
        mem.write(&mut c, ptr as usize, response.as_bytes())?;
        Ok(())
    })?;
    Ok(linker)
}

struct Guest {
    store: Store<Host>,
    instance: Instance,
    /// Linear memory right after instantiation: the base the deltas are taken against.
    base: Vec<u8>,
    module_hash: u64,
    /// Whether the machine code came from the on-disk cache instead of Cranelift.
    cached: bool,
}

fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ *b as u64).wrapping_mul(0x100_0000_01b3))
}

fn wasm_path() -> PathBuf {
    env::var_os("HEAPWORK_WASM").map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/agent.async.wasm")),
        PathBuf::from,
    )
}

/// Compiles the module, or loads machine code compiled earlier for this exact `.wasm`.
fn load_module(engine: &Engine, wasm: &[u8], hash: u64) -> Result<(Module, bool)> {
    let cache = wasm_path().with_extension(format!("{hash:016x}-{}-{}.cwasm", env::consts::OS, env::consts::ARCH));
    if let Ok(bytes) = fs::read(&cache) {
        // SAFETY: the file is our own `Module::serialize` output for this wasm (hash in its
        // name); wasmtime rejects artifacts from another wasmtime version or engine config.
        if let Ok(module) = unsafe { Module::deserialize(engine, &bytes) } {
            return Ok((module, true));
        }
    }
    let module = Module::new(engine, wasm)?;
    write_atomic(&cache, &module.serialize()?)?;
    Ok((module, false))
}

impl Guest {
    fn new(budget: u32) -> Result<Self> {
        let wasm = fs::read(wasm_path())?;
        let module_hash = fnv64(&wasm);
        let engine = Engine::default();
        let (module, cached) = load_module(&engine, &wasm, module_hash)?;
        let mut store = Store::new(&engine, Host { budget, ..Host::default() });
        let instance = linker(&engine)?.instantiate(&mut store, &module)?;
        let mut guest = Guest { store, instance, base: Vec::new(), module_hash, cached };
        guest.base = guest.memory()?.data(&guest.store).to_vec();
        let buf = guest.call_i32("asyncify_buf", &[])?;
        let len = guest.call_i32("asyncify_buf_len", &[])?;
        // Asyncify's data header: [current position, end] of the save area that follows it.
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&(buf + 8).to_le_bytes());
        header[4..].copy_from_slice(&(buf + len).to_le_bytes());
        guest.memory()?.write(&mut guest.store, buf as usize, &header)?;
        guest.store.data_mut().buf = buf;
        Ok(guest)
    }

    fn memory(&mut self) -> Result<wasmtime::Memory> {
        self.instance.get_memory(&mut self.store, "memory").ok_or_else(|| err("no memory"))
    }

    fn call(&mut self, name: &str, args: &[Val]) -> Result<Option<Val>> {
        let f = self.instance.get_func(&mut self.store, name).ok_or_else(|| err(format!("no export {name}")))?;
        invoke(&mut self.store, &f, args)
    }

    fn call_i32(&mut self, name: &str, args: &[Val]) -> Result<i32> {
        self.call(name, args)?.and_then(|v| v.i32()).ok_or_else(|| err(format!("{name}: no i32")))
    }

    fn read(&mut self, ptr: i32, len: usize) -> Result<Vec<u8>> {
        let mut out = vec![0u8; len];
        self.memory()?.read(&self.store, ptr as usize, &mut out)?;
        Ok(out)
    }

    /// Exported mutable globals (`__stack_pointer`, the safepoint counter), by name.
    fn globals(&mut self) -> Vec<(String, wasmtime::Global)> {
        let names: Vec<String> = self
            .instance
            .exports(&mut self.store)
            .filter(|e| e.name().starts_with("heapwork_global_"))
            .map(|e| e.name().to_string())
            .collect();
        names
            .into_iter()
            .filter_map(|n| self.instance.get_global(&mut self.store, &n).map(|g| (n, g)))
            .collect()
    }

    /// Runs (or, after a restore, rewinds into) `run(goal)`. `Some(transcript)` when finished.
    fn run(&mut self, goal_ptr: i32, goal_len: i32) -> Result<Option<String>> {
        self.call("run", &[Val::I32(goal_ptr), Val::I32(goal_len)])?;
        if self.call_i32("asyncify_get_state", &[])? == UNWINDING {
            self.call("asyncify_stop_unwind", &[])?;
            return Ok(None);
        }
        let ptr = self.call_i32("output_ptr", &[])?;
        let len = self.call_i32("output_len", &[])? as usize;
        Ok(Some(String::from_utf8_lossy(&self.read(ptr, len)?).into_owned()))
    }

    fn progress(&mut self) -> Result<[u32; 3]> {
        let ptr = self.call_i32("progress", &[])?;
        let b = self.read(ptr, 12)?;
        Ok([0, 1, 2].map(|i| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())))
    }
}

/// What a snapshot holds besides memory and globals.
struct Meta {
    goal_ptr: i32,
    goal_len: i32,
    /// Guest compute spent so far: what a resume from the snapshot does not redo.
    work_ms: f64,
    pending: Option<String>,
}

#[derive(Default)]
struct Writer(Vec<u8>);
impl Writer {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.0.len() < n {
            return Err(err("truncated snapshot"));
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
}

/// Encodes memory pages that differ from the base, the mutable globals and `meta`.
/// Returns (snapshot, full memory size).
fn save(g: &mut Guest, meta: &Meta) -> Result<(Vec<u8>, usize)> {
    let mut w = Writer::default();
    w.0.extend_from_slice(MAGIC);
    w.u64(g.module_hash);
    w.u32(meta.goal_ptr as u32);
    w.u32(meta.goal_len as u32);
    w.u64(meta.work_ms.to_bits());
    match &meta.pending {
        Some(p) => {
            w.u32(1);
            w.bytes(p.as_bytes());
        }
        None => w.u32(0),
    }
    let globals = g.globals();
    w.u32(globals.len() as u32);
    for (name, global) in globals {
        w.bytes(name.as_bytes());
        let bits = match global.get(&mut g.store) {
            Val::I32(v) => v as u32 as u64,
            Val::I64(v) => v as u64,
            Val::F32(v) => v as u64,
            Val::F64(v) => v,
            other => return Err(err(format!("unsupported global {other:?}"))),
        };
        w.u64(bits);
    }
    let mem = g.memory()?;
    let data = mem.data(&g.store);
    // Pages past the base were grown at runtime; a restored instance grows them zeroed.
    let pages: Vec<usize> = (0..data.len() / PAGE)
        .filter(|&i| {
            let page = &data[i * PAGE..(i + 1) * PAGE];
            match g.base.get(i * PAGE..(i + 1) * PAGE) {
                Some(base) => base != page,
                None => page.iter().any(|&b| b != 0),
            }
        })
        .collect();
    w.u64(data.len() as u64);
    w.u32(pages.len() as u32);
    for i in pages {
        w.u32(i as u32);
        w.0.extend_from_slice(&data[i * PAGE..(i + 1) * PAGE]);
    }
    Ok((w.0, data.len()))
}

/// Writes the snapshot's memory pages and globals back into `g`, a fresh instance.
fn load(g: &mut Guest, snapshot: &[u8]) -> Result<Meta> {
    let mut r = Reader(snapshot);
    if r.take(8)? != MAGIC {
        return Err(err("not a heapwork snapshot"));
    }
    if r.u64()? != g.module_hash {
        return Err(err("snapshot was written by a different agent.async.wasm build"));
    }
    let goal_ptr = r.u32()? as i32;
    let goal_len = r.u32()? as i32;
    let work_ms = f64::from_bits(r.u64()?);
    let pending = match r.u32()? {
        0 => None,
        _ => Some(String::from_utf8(r.bytes()?)?),
    };
    for _ in 0..r.u32()? {
        let name = String::from_utf8(r.bytes()?)?;
        let bits = r.u64()?;
        let global = g.instance.get_global(&mut g.store, &name).ok_or_else(|| err(format!("no global {name}")))?;
        let val = match global.get(&mut g.store) {
            Val::I32(_) => Val::I32(bits as u32 as i32),
            Val::I64(_) => Val::I64(bits as i64),
            Val::F32(_) => Val::F32(bits as u32),
            _ => Val::F64(bits),
        };
        global.set(&mut g.store, val)?;
    }
    let size = r.u64()? as usize;
    let mem = g.memory()?;
    let have = mem.data_size(&g.store);
    if size > have {
        mem.grow(&mut g.store, ((size - have) / WASM_PAGE) as u64)?;
    }
    for _ in 0..r.u32()? {
        let i = r.u32()? as usize;
        mem.write(&mut g.store, i * PAGE, r.take(PAGE)?)?;
    }
    if !r.0.is_empty() {
        return Err(err("trailing bytes in snapshot"));
    }
    Ok(Meta { goal_ptr, goal_len, work_ms, pending })
}

struct Env {
    dir: PathBuf,
    budget: u32,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(tmp, path)
}

/// Runs one slice and turns its result into the next on-disk state.
fn slice(g: &mut Guest, env: &Env, goal_ptr: i32, goal_len: i32, work_before_ms: f64) -> Result<()> {
    let t = Instant::now();
    let ran = g.run(goal_ptr, goal_len)?;
    let slice_ms = t.elapsed().as_secs_f64() * 1e3;
    let work_ms = work_before_ms + slice_ms;
    match ran {
        Some(history) => {
            g.call("dealloc", &[Val::I32(goal_ptr), Val::I32(goal_len)])?;
            fs::write(env.dir.join("done.json"), &history)?;
            let _ = fs::remove_file(env.dir.join("snapshot.bin"));
            println!("  finished in {slice_ms:.1} ms ({work_ms:.1} ms of compute in total); transcript:\n{history}");
        }
        None => {
            let pending = g.store.data_mut().pending.take();
            let t = Instant::now();
            let meta = Meta { goal_ptr, goal_len, work_ms, pending: pending.clone() };
            let (snap, mem_size) = save(g, &meta)?;
            let write_ms = t.elapsed().as_secs_f64() * 1e3;
            write_atomic(&env.dir.join("snapshot.bin"), &snap)?;
            let [step, i, of] = g.progress()?;
            let why = match &pending {
                Some(p) => format!("parked on llm({p:?})"),
                None => "preempted mid-loop".to_string(),
            };
            println!("  ran {slice_ms:.1} ms, {why}");
            println!("  progress {{\"step\":{step},\"i\":{i},\"of\":{of}}}, {work_ms:.1} ms of compute so far");
            println!(
                "  wrote snapshot.bin: {} bytes (linear memory {} bytes, changed 4 KiB pages only) in {write_ms:.2} ms",
                snap.len(),
                mem_size
            );
        }
    }
    Ok(())
}

fn start(env: &Env, goal: &str) -> Result<()> {
    fs::create_dir_all(&env.dir)?;
    for stale in ["snapshot.bin", "done.json"] {
        let _ = fs::remove_file(env.dir.join(stale));
    }
    let t = Instant::now();
    let mut g = Guest::new(env.budget)?;
    println!(
        "  {} agent.async.wasm in {:.1} ms (base = its data segments, {} bytes of memory)",
        if g.cached { "loaded cached machine code for" } else { "compiled" },
        t.elapsed().as_secs_f64() * 1e3,
        g.base.len()
    );
    let ptr = g.call_i32("alloc", &[Val::I32(goal.len() as i32)])?;
    g.memory()?.write(&mut g.store, ptr as usize, goal.as_bytes())?;
    slice(&mut g, env, ptr, goal.len() as i32, 0.0)
}

fn step(env: &Env, answer: Option<&str>) -> Result<()> {
    if let Ok(done) = fs::read_to_string(env.dir.join("done.json")) {
        println!("  already finished:\n{done}");
        return Ok(());
    }
    let snap = fs::read(env.dir.join("snapshot.bin"))?;
    let t = Instant::now();
    let mut g = Guest::new(env.budget)?;
    let module_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let meta = load(&mut g, &snap)?;
    println!(
        "  handoff: module {} in {module_ms:.1} ms, state restored from snapshot.bin ({} bytes) in {:.2} ms, skipping {:.1} ms of compute",
        if g.cached { "loaded from cache" } else { "compiled" },
        snap.len(),
        t.elapsed().as_secs_f64() * 1e3,
        meta.work_ms
    );
    match (&meta.pending, answer) {
        (Some(prompt), None) => {
            println!("  waiting on llm({prompt:?}); pass the reply: heapwork step \"<answer>\"");
            return Ok(());
        }
        (Some(_), Some(reply)) => g.store.data_mut().reply = Some(reply.to_string()),
        (None, Some(_)) => {
            return Err(err("the agent is not waiting on llm(); run `step` without an answer"));
        }
        (None, None) => {}
    }
    // Replay the saved call stack: run() re-enters each frame and stops at the call that unwound.
    let buf = g.store.data().buf;
    g.call("asyncify_start_rewind", &[Val::I32(buf)])?;
    slice(&mut g, env, meta.goal_ptr, meta.goal_len, meta.work_ms)
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let env = Env {
        dir: env::var_os("HEAPWORK_STATE").map_or_else(|| PathBuf::from("state"), PathBuf::from),
        budget: env::var("HEAPWORK_SLICE").ok().and_then(|s| s.parse().ok()).unwrap_or(30),
    };
    println!("[pid {}] heapwork {}", process::id(), args.join(" "));
    let res = match args.first().map(String::as_str) {
        Some("agent") => agent::cli(&env, &args[1..]),
        Some("start") if args.len() == 2 => start(&env, &args[1]),
        Some("step") if args.len() <= 2 => step(&env, args.get(1).map(String::as_str)),
        _ => {
            eprintln!("usage: heapwork start \"<goal>\" | heapwork step [answer]");
            process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e:#}");
        process::exit(1);
    }
}
