//! Conversation, tool dispatch, and the live call stack all stay in WebAssembly memory.
use serde_json::{json, Value};
use std::{cell::RefCell, ptr::addr_of_mut};

#[link(wasm_import_module = "env")]
extern "C" {
    fn host_call(request: *const u8, len: usize) -> usize;
    fn host_result(out: *mut u8, len: usize);
}

thread_local! {
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}
const ASYNCIFY_BUF_LEN: usize = 128 * 1024;
static mut ASYNCIFY_BUF: [u8; ASYNCIFY_BUF_LEN] = [0; ASYNCIFY_BUF_LEN];

fn call(request: Value) -> String {
    let request = request.to_string();
    // The request and all callers remain live while host_call is parked.
    let len = unsafe { host_call(request.as_ptr(), request.len()) };
    let mut result = vec![0; len];
    unsafe { host_result(result.as_mut_ptr(), len) };
    String::from_utf8(result).expect("host returned non-UTF-8")
}

fn tools() -> Value {
    json!([
        {"name":"read_file","description":"Read a UTF-8 file in the workspace.","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},
        {"name":"write_file","description":"Write the complete UTF-8 contents of a workspace file.","input_schema":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}},
        {"name":"list_dir","description":"List a workspace directory.","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}},
        {"name":"bash","description":"Execute a shell command in the workspace. Output is bounded; timeout defaults to 30 seconds (maximum 120).","input_schema":{"type":"object","properties":{"command":{"type":"string"},"timeout":{"type":"integer"}},"required":["command"]}}
    ])
}

#[no_mangle]
pub extern "C" fn run(goal: *const u8, goal_len: usize) {
    let config: Value = serde_json::from_slice(unsafe { std::slice::from_raw_parts(goal, goal_len) }).unwrap();
    let model = config["model"].as_str().unwrap();
    let mut prompt = config["prompt"].as_str().unwrap().to_owned();
    let mut messages: Vec<Value> = Vec::new();
    loop {
        if prompt == "/exit" { break; }
        messages.push(json!({"role":"user","content":prompt}));
        loop {
            let response: Value = serde_json::from_str(&call(json!({
                "op":"llm", "body":{
                    "model":model, "max_tokens":1024,
                    "system":"You are a concise coding agent. Inspect the workspace, make the requested change, and run its tests. Use tools directly. Do not claim tests passed unless you ran them. Keep final replies under 100 words.",
                    "tools":tools(), "messages":messages
                }
            }))).expect("invalid model response");
            let content = response["content"].as_array().expect("model response has no content");
            messages.push(json!({"role":"assistant","content":content}));
            let mut results = Vec::new();
            for block in content {
                if block["type"] == "tool_use" {
                    let result = call(json!({"op":"tool","name":block["name"],"input":block["input"]}));
                    let result: Value = serde_json::from_str(&result).expect("invalid tool result");
                    results.push(json!({"type":"tool_result","tool_use_id":block["id"],"content":result["content"],"is_error":result["is_error"]}));
                }
            }
            if results.is_empty() { break; }
            messages.push(json!({"role":"user","content":results}));
        }
        OUTPUT.with_borrow_mut(|out| *out = serde_json::to_vec_pretty(&messages).unwrap());
        prompt = call(json!({"op":"prompt"}));
    }
}

#[no_mangle]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = std::mem::ManuallyDrop::new(Vec::<u8>::with_capacity(len));
    buf.as_mut_ptr()
}
#[no_mangle]
pub extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len) });
}
#[no_mangle]
pub extern "C" fn output_ptr() -> *const u8 { OUTPUT.with_borrow(|out| out.as_ptr()) }
#[no_mangle]
pub extern "C" fn output_len() -> usize { OUTPUT.with_borrow(|out| out.len()) }
#[no_mangle]
pub extern "C" fn asyncify_buf() -> *mut u8 { addr_of_mut!(ASYNCIFY_BUF) as *mut u8 }
#[no_mangle]
pub extern "C" fn asyncify_buf_len() -> usize { ASYNCIFY_BUF_LEN }
