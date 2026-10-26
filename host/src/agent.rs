use super::*;
use serde_json::{json, Value};
use std::{io::{Read, Write}, os::unix::process::CommandExt, process::{Command, Stdio}, thread, time::Duration};

fn api_key() -> Result<String> {
    env::var("ANTHROPIC_API_KEY").map_err(|_| err("set ANTHROPIC_API_KEY"))
}

fn client() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(90))).build().into()
}

fn model() -> Result<String> {
    if let Ok(model) = env::var("HEAPWORK_MODEL") {
        if !model.contains("haiku") && !model.contains("sonnet") {
            return Err(err("HEAPWORK_MODEL must be a Haiku or Sonnet model for the budget bound"));
        }
        return Ok(model);
    }
    let response: Value = client().get("https://api.anthropic.com/v1/models?limit=100")
        .header("x-api-key", &api_key()?).header("anthropic-version", "2023-06-01")
        .call().map_err(|_| err("model listing failed; check API credentials"))?.body_mut().read_json()?;
    let models = response["data"].as_array().ok_or_else(|| err("models response missing data"))?;
    for family in ["haiku", "sonnet"] {
        let latest = models.iter().filter(|m| m["id"].as_str().is_some_and(|id| id.contains(family)))
            .max_by_key(|m| m["created_at"].as_str().unwrap_or(""));
        if let Some(m) = latest { return Ok(m["id"].as_str().unwrap().to_owned()); }
    }
    Err(err("no Haiku or Sonnet model available"))
}

// Reserve before sending. Five dollars/M input and twenty-five/M output are deliberately
// conservative for Haiku/Sonnet; one UTF-8 byte per input token overestimates normal requests.
// Reserve persists even on HTTP failure: no automatic retries and no uncounted failed calls.
fn llm(dir: &Path, body: &Value) -> Result<String> {
    let budget_path = dir.join("budget.json");
    let mut budget: Value = serde_json::from_slice(&fs::read(&budget_path).unwrap_or_else(|_| b"{\"calls\":0,\"reserved_usd\":0}".to_vec()))?;
    let calls = budget["calls"].as_u64().unwrap();
    let encoded = body.to_string();
    let max_tokens = body["max_tokens"].as_u64().ok_or_else(|| err("missing max_tokens"))?;
    if max_tokens > 1024 { return Err(err("max_tokens exceeds 1024")); }
    let reserve = (encoded.len() + 4096) as f64 * 5.0 / 1_000_000.0 + max_tokens as f64 * 25.0 / 1_000_000.0;
    let total = budget["reserved_usd"].as_f64().unwrap() + reserve;
    if calls >= 6 || total > 0.50 { return Err(err("API safety limit reached (6 calls / $0.50 conservative reservation)")); }
    budget = json!({"calls":calls+1,"reserved_usd":total});
    write_atomic(&budget_path, budget.to_string().as_bytes())?;
    println!("  API call {}: reserved ${reserve:.4} before sending (${total:.4}/$0.50 total bound)", calls + 1);
    let response: Value = client().post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key()?).header("anthropic-version", "2023-06-01")
        .send_json(body).map_err(|_| err("Anthropic request failed (no retry; snapshot is unchanged)"))?
        .body_mut().read_json()?;
    println!("  API usage: {}", response["usage"]);
    let mut usage = fs::OpenOptions::new().create(true).append(true).open(dir.join("usage.jsonl"))?;
    writeln!(usage, "{}", json!({"model":body["model"],"usage":response["usage"]}))?;
    for block in response["content"].as_array().ok_or_else(|| err("response missing content"))? {
        if let Some(text) = block["text"].as_str() { println!("  assistant: {text}"); }
        if block["type"] == "tool_use" { println!("  requested tool {}({})", block["name"], block["input"]); }
    }
    if !matches!(response["stop_reason"].as_str(), Some("end_turn" | "tool_use")) {
        return Err(err(format!("model stopped with {}; turn not complete", response["stop_reason"])));
    }
    Ok(response.to_string())
}

fn workspace_path(root: &Path, name: &str, writing: bool) -> Result<PathBuf> {
    let relative = Path::new(name);
    if relative.is_absolute() || relative.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(err("path must stay inside the workspace"));
    }
    let path = root.join(relative);
    let check = if writing && path.symlink_metadata().is_err() { path.parent().unwrap_or(root) } else { &path };
    if !check.canonicalize()?.starts_with(root.canonicalize()?) { return Err(err("symlink escapes workspace")); }
    Ok(path)
}

fn bash(root: &Path, input: &Value) -> Result<String> {
    let command = input["command"].as_str().ok_or_else(|| err("missing command"))?;
    let timeout = input["timeout"].as_u64().unwrap_or(30).clamp(1, 120);
    // A file avoids pipe deadlock. The command gets no API key or inherited secrets.
    let path = root.parent().unwrap().join(format!("command-{}.out", process::id()));
    let output = fs::File::create(&path)?;
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", command]).current_dir(root).env_clear();
    for key in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(value) = env::var_os(key) { cmd.env(key, value); }
    }
    let mut child = cmd.stdin(Stdio::null()).stdout(output.try_clone()?).stderr(output).process_group(0).spawn()?;
    let t = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? { break (status, false); }
        if t.elapsed() >= Duration::from_secs(timeout) {
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut bytes = Vec::new();
    fs::File::open(&path)?.take(32_768).read_to_end(&mut bytes)?;
    fs::remove_file(path)?;
    let result = format!("exit={} timeout={}\n{}", status.code().map_or_else(|| "signal".into(), |c| c.to_string()), timed_out, String::from_utf8_lossy(&bytes));
    println!("  bash result: {result}");
    Ok(result)
}

fn tool(root: &Path, request: &Value) -> String {
    let input = &request["input"];
    let execute = || -> Result<String> {
        let path = || input["path"].as_str().ok_or_else(|| err("missing path"));
        match request["name"].as_str().unwrap_or("") {
            "read_file" => {
                let mut bytes = Vec::new();
                fs::File::open(workspace_path(root, path()?, false)?)?.take(32_769).read_to_end(&mut bytes)?;
                if bytes.len() > 32_768 { return Err(err("file exceeds 32 KiB read limit")); }
                Ok(String::from_utf8(bytes)?)
            }
            "write_file" => {
                let path = workspace_path(root, path()?, true)?;
                fs::write(path, input["content"].as_str().ok_or_else(|| err("missing content"))?)?;
                Ok("file written".into())
            }
            "list_dir" => {
                let mut names = fs::read_dir(workspace_path(root, path()?, false)?)?
                    .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned())).collect::<std::io::Result<Vec<_>>>()?;
                names.sort();
                Ok(names.join("\n"))
            }
            "bash" => bash(root, input),
            _ => Err(err("unknown tool")),
        }
    };
    match execute() {
        Ok(content) => json!({"content":content,"is_error":false}).to_string(),
        Err(error) => json!({"content":error.to_string(),"is_error":true}).to_string(),
    }
}

fn description(pending: &Option<String>) -> String {
    let Some(pending) = pending else { return "preempted at loop safepoint".into(); };
    let request: Value = serde_json::from_str(pending).unwrap_or(Value::Null);
    match request["op"].as_str() {
        Some("llm") => "parked on llm (before HTTP request)".into(),
        Some("prompt") => "waiting for next user prompt (turn complete)".into(),
        Some("tool") => format!("parked on tool {}({})", request["name"], request["input"]),
        _ => "unknown host call".into(),
    }
}

fn persist(g: &mut Guest, env: &Env, mut meta: Meta) -> Result<()> {
    let t = Instant::now();
    let result = g.run(meta.goal_ptr, meta.goal_len)?;
    meta.work_ms += t.elapsed().as_secs_f64() * 1e3;
    if let Some(history) = result {
        write_atomic(&env.dir.join("done.json"), history.as_bytes())?;
        fs::remove_file(env.dir.join("snapshot.bin"))?;
        println!("  session finished");
        return Ok(());
    }
    meta.pending = g.store.data_mut().pending.take();
    let t = Instant::now();
    let (snapshot, memory_size) = save(g, &meta)?;
    write_atomic(&env.dir.join("snapshot.bin"), &snapshot)?;
    let public = json!({"pending":meta.pending.as_ref().map(|p| serde_json::from_str::<Value>(p).unwrap()),"snapshot_bytes":snapshot.len()});
    write_atomic(&env.dir.join("status.json"), public.to_string().as_bytes())?;
    if meta.pending.as_ref().is_some_and(|p| p == "{\"op\":\"prompt\"}") {
        let ptr = g.call_i32("output_ptr", &[])?;
        let len = g.call_i32("output_len", &[])? as usize;
        write_atomic(&env.dir.join("transcript.json"), &g.read(ptr, len)?)?;
    }
    println!("  {}; snapshot={} bytes, memory={} bytes, save={:.2} ms", description(&meta.pending), snapshot.len(), memory_size, t.elapsed().as_secs_f64()*1e3);
    Ok(())
}

fn copy_workspace(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str == "target" || name_str == ".env" || name_str.starts_with(".env.") { continue; }
        let kind = entry.file_type()?;
        if kind.is_symlink() { return Err(err("workspace copy rejects symlinks")); }
        let dest = to.join(name);
        if kind.is_dir() { copy_workspace(&entry.path(), &dest)?; }
        else if kind.is_file() { fs::copy(entry.path(), dest)?; }
        else { return Err(err("workspace contains a non-regular file")); }
    }
    Ok(())
}

pub(super) fn cli(env: &Env, args: &[String]) -> Result<()> {
    if env::var_os("HEAPWORK_WASM").is_none() {
        env::set_var("HEAPWORK_WASM", concat!(env!("CARGO_MANIFEST_DIR"), "/../target/coder.async.wasm"));
    }
    match args.first().map(String::as_str) {
        Some("start") if args.len() == 3 => {
            if env.dir.exists() { return Err(err("state directory already exists; use a new HEAPWORK_STATE")); }
            let model = model()?;
            println!("  model: {model}");
            let source = Path::new(&args[1]).canonicalize()?;
            fs::create_dir_all(&env.dir)?;
            if env.dir.canonicalize()?.starts_with(&source) {
                fs::remove_dir(&env.dir)?;
                return Err(err("state directory must be outside the source workspace"));
            }
            copy_workspace(&source, &env.dir.join("workspace"))?;
            let config = json!({"model":model,"prompt":args[2]}).to_string();
            let mut g = Guest::new(0)?;
            let ptr = g.call_i32("alloc", &[Val::I32(config.len() as i32)])?;
            g.memory()?.write(&mut g.store, ptr as usize, config.as_bytes())?;
            g.store.data_mut().budget = env.budget;
            persist(&mut g, env, Meta { goal_ptr:ptr, goal_len:config.len() as i32, work_ms:0.0, pending:None })
        }
        Some("step") | Some("say") if args.len() == 1 || (args[0] == "say" && args.len() == 2) => {
            let snapshot = fs::read(env.dir.join("snapshot.bin"))?;
            let t = Instant::now();
            let mut g = Guest::new(0)?;
            let module_ms = t.elapsed().as_secs_f64()*1e3;
            let t = Instant::now();
            let meta = load(&mut g, &snapshot)?;
            println!("  handoff {} / {}: snapshot={} bytes, module={module_ms:.2} ms, restore={:.2} ms; transfer API tokens=0", env::consts::OS, env::consts::ARCH, snapshot.len(), t.elapsed().as_secs_f64()*1e3);
            if let Some(pending) = &meta.pending {
                let request: Value = serde_json::from_str(pending)?;
                let response = match request["op"].as_str() {
                    Some("prompt") if args[0] == "say" && args.len() == 2 => args[1].clone(),
                    Some("prompt") => return Err(err("turn complete; use agent say \"next prompt\"")),
                    _ if args[0] == "say" => return Err(err("agent is not waiting for a user prompt")),
                    Some("llm") => llm(&env.dir, &request["body"])?,
                    Some("tool") => tool(&env.dir.join("workspace").canonicalize()?, &request),
                    _ => return Err(err("unknown pending operation")),
                };
                g.store.data_mut().reply = Some(response);
            } else if args[0] == "say" { return Err(err("agent is preempted, not waiting for a prompt")); }
            g.store.data_mut().budget = env.budget;
            let buf = g.store.data().buf;
            g.call("asyncify_start_rewind", &[Val::I32(buf)])?;
            persist(&mut g, env, meta)
        }
        _ => Err(err("usage: heapwork agent start <workspace> <prompt> | agent step | agent say <prompt>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn workspace_paths_reject_parent_and_dangling_symlink_escapes() {
        let dir = env::temp_dir().join(format!("heapwork-paths-{}", process::id()));
        fs::create_dir(&dir).unwrap();
        let root = dir.join("workspace");
        fs::create_dir(&root).unwrap();
        symlink(dir.join("outside"), root.join("dangling")).unwrap();
        assert!(workspace_path(&root, "../outside", true).is_err());
        assert!(workspace_path(&root, "dangling", true).is_err());
        assert!(workspace_path(&root, "new-file", true).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }
}
