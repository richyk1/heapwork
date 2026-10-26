//! A small agent loop in plain Rust, compiled to `wasm32-unknown-unknown`. Nothing in here
//! knows about snapshots: safepoints and stack unwinding are added to the compiled `.wasm`
//! afterwards (see `instrument/` and `demo.sh`).

use std::{cell::RefCell, ptr::addr_of_mut};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Durable host call: reads like a synchronous call; the host may unwind the whole stack
    /// here, exit, and answer it from another process. Returns the reply length in `out`.
    fn llm(prompt: *const u8, prompt_len: usize, out: *mut u8, out_cap: usize) -> usize;
}

thread_local! {
    /// The finished transcript, owned by the guest; the host reads it via `output_ptr/len`.
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// [step, i, of], read by the host between slices.
static mut PROGRESS: [u32; 3] = [0; 3];

const ASYNCIFY_BUF_LEN: usize = 16 * 1024;
/// Where Asyncify saves the call stack (locals + call positions) while unwound.
static mut ASYNCIFY_BUF: [u8; ASYNCIFY_BUF_LEN] = [0; ASYNCIFY_BUF_LEN];

fn set_progress(slot: usize, value: u32) {
    // SAFETY: single-threaded guest; volatile so the loop keeps its per-iteration store.
    unsafe { addr_of_mut!(PROGRESS[slot]).write_volatile(value) }
}

/// CPU-bound and synchronous (never yields): only preemption can pause it mid-loop.
fn think(seed: u64, n: u64) -> u64 {
    set_progress(2, n as u32);
    let mut h = seed;
    for i in 0..n {
        h = (h * 31 + i) % 1_000_003;
        set_progress(1, i as u32);
    }
    h
}

fn call_llm(prompt: &str) -> String {
    let mut out = vec![0u8; 4096];
    // SAFETY: both buffers are valid for the given lengths for the duration of the call.
    let n = unsafe { llm(prompt.as_ptr(), prompt.len(), out.as_mut_ptr(), out.len()) };
    String::from_utf8_lossy(&out[..n.min(out.len())]).into_owned()
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The conversation as pretty-printed JSON.
fn agent(goal: &str) -> String {
    let mut history: Vec<(&str, String)> = vec![("user", goal.to_string())];
    for step in 1..=3u64 {
        set_progress(0, step as u32);
        let digest = think(step, 2_000_000);
        let reply = call_llm(&format!("step {step}/3 of \"{goal}\" (digest {digest})"));
        history.push(("assistant", format!("{reply} [digest {digest}]")));
    }
    let turns: Vec<String> = history
        .iter()
        .map(|(role, content)| {
            format!(
                "  {{\n    \"role\": {},\n    \"content\": {}\n  }}",
                json_str(role),
                json_str(content)
            )
        })
        .collect();
    format!("[\n{}\n]", turns.join(",\n"))
}

/// Hands the host a buffer of `len` bytes; the host returns it with `dealloc`.
#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = std::mem::ManuallyDrop::new(Vec::<u8>::with_capacity(len));
    buf.as_mut_ptr()
}

/// Frees a buffer returned by `alloc(len)`.
#[no_mangle]
pub extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    // SAFETY: `ptr` came from `alloc(len)`, which created a Vec of capacity `len`.
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}

/// Runs the agent to completion; the transcript is then at `output_ptr()/output_len()`.
#[no_mangle]
pub extern "C" fn run(goal: *const u8, goal_len: usize) {
    // SAFETY: the host wrote `goal_len` UTF-8 bytes at `goal` (allocated with `alloc`).
    let goal = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(goal, goal_len)) };
    let transcript = agent(goal);
    OUTPUT.with_borrow_mut(|out| *out = transcript.into_bytes());
}

#[no_mangle]
pub extern "C" fn output_ptr() -> *const u8 {
    OUTPUT.with_borrow(|out| out.as_ptr())
}

#[no_mangle]
pub extern "C" fn output_len() -> usize {
    OUTPUT.with_borrow(|out| out.len())
}

#[no_mangle]
pub extern "C" fn progress() -> *const u32 {
    addr_of_mut!(PROGRESS) as *const u32
}

#[no_mangle]
pub extern "C" fn asyncify_buf() -> *mut u8 {
    addr_of_mut!(ASYNCIFY_BUF) as *mut u8
}

#[no_mangle]
pub extern "C" fn asyncify_buf_len() -> usize {
    ASYNCIFY_BUF_LEN
}
