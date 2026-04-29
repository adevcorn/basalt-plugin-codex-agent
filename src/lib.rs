//! codex-agent — Basalt plugin providing the OpenAI Codex agent launcher.
//!
//! Provides: agent-launcher:codex
//! Parses:   codex exec --json   (NDJSON, one JSON object per line)

use basalt_plugin_sdk::prelude::*;

basalt_plugin_meta! {
    name:              "codex-agent",
    version:           env!("CARGO_PKG_VERSION"),
    hook_flags:        CAP_AGENT_LAUNCHER,
    provides:          "agent-launcher:codex",
    requires:          "",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

// ---------------------------------------------------------------------------
// agent_metadata
// ---------------------------------------------------------------------------

#[basalt_plugin]
fn agent_metadata() -> AgentMetadata {
    AgentMetadata {
        name: "OpenAI Codex".into(),
        executable: "/usr/local/bin/codex".into(),
        args: vec![
            "exec".into(),
            "-a".into(),
            "never".into(),
            "-s".into(),
            "workspace-write".into(),
            "--skip-git-repo-check".into(),
            "--json".into(),
        ],
        // New session with prompt
        resume_new_args: vec![
            "exec".into(),
            "--full-auto".into(),
            "-s".into(),
            "workspace-write".into(),
            "--skip-git-repo-check".into(),
            "--json".into(),
            "{prompt}".into(),
        ],
        // Resume prior session
        resume_cont_args: vec![
            "exec".into(),
            "resume".into(),
            "--full-auto".into(),
            "-s".into(),
            "workspace-write".into(),
            "--skip-git-repo-check".into(),
            "--json".into(),
            "{session_id}".into(),
            "{prompt}".into(),
        ],
        execution_tier: AgentExecutionTier::StructuredDirect,
        workspace_capabilities: vec![
            "speculative-edits".into(),
            "approval-required".into(),
            "utf8-text".into(),
            "create".into(),
            "delete".into(),
            "rename".into(),
            "materialized-copy".into(),
        ],
    }
}

// ---------------------------------------------------------------------------
// Parser state — set of item IDs that have been opened via item.started
// ---------------------------------------------------------------------------
/// State blob: `[count: u16 LE]` then per entry `[key_len: u16 LE][key bytes]`

struct ParseState {
    open_items: Vec<String>,
    open_message: bool,
}

impl ParseState {
    fn decode(state: &[u8]) -> Self {
        if state.len() < 2 {
            return Self { open_items: vec![], open_message: false };
        }
        let count = u16::from_le_bytes([state[0], state[1]]) as usize;
        let mut items = Vec::with_capacity(count);
        let mut cur = 2usize;
        for _ in 0..count {
            if cur + 2 > state.len() {
                break;
            }
            let klen = u16::from_le_bytes([state[cur], state[cur + 1]]) as usize;
            cur += 2;
            if cur + klen > state.len() {
                break;
            }
            if let Ok(s) = std::str::from_utf8(&state[cur..cur + klen]) {
                items.push(s.to_string());
            }
            cur += klen;
        }
        let open_message = state.get(cur).copied().unwrap_or(0) != 0;
        Self { open_items: items, open_message }
    }

    fn encode(&self) -> Vec<u8> {
        let count = self.open_items.len().min(0xFFFF) as u16;
        let mut out = Vec::new();
        out.extend_from_slice(&count.to_le_bytes());
        for k in &self.open_items[..count as usize] {
            let bytes = k.as_bytes();
            let klen = bytes.len().min(0xFFFF) as u16;
            out.extend_from_slice(&klen.to_le_bytes());
            out.extend_from_slice(&bytes[..klen as usize]);
        }
        out.push(self.open_message as u8);
        out
    }

    fn contains(&self, id: &str) -> bool {
        self.open_items.iter().any(|s| s == id)
    }

    fn insert(&mut self, id: String) {
        if !self.contains(&id) {
            self.open_items.push(id);
        }
    }

    fn remove(&mut self, id: &str) -> bool {
        if let Some(pos) = self.open_items.iter().position(|s| s == id) {
            self.open_items.remove(pos);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// agent_parse_line
// ---------------------------------------------------------------------------

#[basalt_plugin]
fn agent_parse_line(line: &[u8], state: &[u8]) -> (Vec<u8>, Vec<AgentEvent>) {
    let Ok(line_str) = std::str::from_utf8(line) else {
        return (state.to_vec(), vec![]);
    };
    let line_str = line_str.trim();
    if line_str.is_empty() {
        return (state.to_vec(), vec![]);
    }
    let mut ps = ParseState::decode(state);
    let events = parse_codex_line(line_str, &mut ps);
    (ps.encode(), events)
}

fn parse_codex_line(line: &str, ps: &mut ParseState) -> Vec<AgentEvent> {
    let type_val = match json_str(line, "type") {
        Some(t) => t,
        None => return vec![],
    };

    match type_val.as_str() {
        "item.started" => {
            let item_raw = match json_object_raw(line, "item") {
                Some(r) => r,
                None => return vec![],
            };
            let item_id = json_str(&item_raw, "id").unwrap_or_default();
            let item_type = json_str(&item_raw, "type").unwrap_or_default();
            if item_type != "command_execution" || item_id.is_empty() {
                return vec![];
            }
            let cmd = json_str(&item_raw, "command").unwrap_or_default();
            let (tool, category) = codex_classify(&cmd);
            let file_paths = codex_file_paths(&cmd);
            ps.insert(item_id.clone());
            vec![AgentEvent::NewEntry {
                vendor_id: item_id,
                tool,
                category,
                raw_cmd: cmd,
                file_paths,
            }]
        }

        "item.completed" => {
            let item_raw = match json_object_raw(line, "item") {
                Some(r) => r,
                None => return vec![],
            };
            let item_id = json_str(&item_raw, "id").unwrap_or_default();
            let item_type = json_str(&item_raw, "type").unwrap_or_default();
            if item_id.is_empty() {
                return vec![];
            }

            match item_type.as_str() {
                "command_execution" => {
                    let cmd = json_str(&item_raw, "command").unwrap_or_default();
                    let exit_code = json_int(&item_raw, "exit_code").unwrap_or(0) as i32;
                    let output = json_str(&item_raw, "aggregated_output").unwrap_or_default();
                    let lines: Vec<String> = output
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| l.to_string())
                        .collect();

                    if ps.remove(&item_id) {
                        // Was opened via item.started — just close it.
                        vec![AgentEvent::CloseEntry {
                            vendor_id: item_id,
                            exit_code,
                            output_lines: lines,
                        }]
                    } else {
                        // No prior item.started — open and immediately close.
                        let (tool, category) = codex_classify(&cmd);
                        let file_paths = codex_file_paths(&cmd);
                        let vid = item_id.clone();
                        vec![
                            AgentEvent::NewEntry {
                                vendor_id: vid.clone(),
                                tool,
                                category,
                                raw_cmd: cmd,
                                file_paths,
                            },
                            AgentEvent::CloseEntry {
                                vendor_id: vid,
                                exit_code,
                                output_lines: lines,
                            },
                        ]
                    }
                }

                "function_call" => {
                    let name = json_str(&item_raw, "name").unwrap_or_else(|| "tool".into());
                    let args_raw = json_str(&item_raw, "arguments").unwrap_or_default();
                    let (tool, category, file_paths) = classify_function_call(&name, &args_raw);
                    let raw_cmd = extract_cmd_from_args(&args_raw, &name);
                    ps.insert(item_id.clone());
                    vec![AgentEvent::NewEntry {
                        vendor_id: item_id,
                        tool,
                        category,
                        raw_cmd,
                        file_paths,
                    }]
                }

                "function_call_output" => {
                    let call_id = json_str(&item_raw, "call_id").unwrap_or_else(|| item_id.clone());
                    if !ps.remove(&call_id) {
                        return vec![];
                    }
                    let output = json_str(&item_raw, "output").unwrap_or_default();
                    let lines: Vec<String> = output
                        .lines()
                        .filter(|l| !l.is_empty())
                        .map(|l| l.to_string())
                        .collect();
                    vec![AgentEvent::CloseEntry {
                        vendor_id: call_id,
                        exit_code: 0,
                        output_lines: lines,
                    }]
                }

                "agent_message" => parse_agent_message(&item_raw, ps),

                "error" => {
                    let msg =
                        json_str(&item_raw, "message").unwrap_or_else(|| "unknown error".into());
                    let label: String = format!("Error: {}", &msg[..msg.len().min(60)]);
                    let vid = format!("err:{}", item_id);
                    vec![
                        AgentEvent::NewEntry {
                            vendor_id: vid.clone(),
                            tool: label,
                            category: "error".into(),
                            raw_cmd: msg,
                            file_paths: vec![],
                        },
                        AgentEvent::CloseEntry {
                            vendor_id: vid,
                            exit_code: 1,
                            output_lines: vec![],
                        },
                    ]
                }

                _ => vec![],
            }
        }

        "turn.completed" => {
            ps.open_message = false;
            vec![AgentEvent::SessionEnded { success: true }]
        },

        "thread.started" => {
            if let Some(tid) = json_str(line, "thread_id") {
                vec![AgentEvent::SessionIDAvailable(tid)]
            } else {
                vec![]
            }
        }

        _ => vec![],
    }
}

fn parse_agent_message(item_raw: &str, ps: &mut ParseState) -> Vec<AgentEvent> {
    let text = json_str(item_raw, "content")
        .or_else(|| json_str(item_raw, "text"))
        .or_else(|| json_str(item_raw, "message"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return vec![];
    }

    if !ps.open_message {
        ps.open_message = true;
        return vec![AgentEvent::NewEntry {
            vendor_id: "codex-message".into(),
            tool: text,
            category: "message".into(),
            raw_cmd: String::new(),
            file_paths: vec![],
        }];
    }

    vec![AgentEvent::AppendToEntry {
        vendor_id: "codex-message".into(),
        text,
    }]
}

fn classify_function_call(name: &str, args_raw: &str) -> (String, String, Vec<String>) {
    match name {
        "bash" | "run_command" | "execute_command" => {
            let cmd = json_str(
                &format!("{{{}}}", args_raw.trim_matches(|c| c == '{' || c == '}')),
                "command",
            )
            .unwrap_or_else(|| args_raw.chars().take(80).collect());
            let (tool, category) = codex_classify(&cmd);
            let paths = codex_file_paths(&cmd);
            (tool, category, paths)
        }
        "read_file" | "get_file_content" | "view" => {
            let inner = args_raw.trim_matches(|c| c == '{' || c == '}');
            let path = json_str(&format!("{{{}}}", inner), "path")
                .or_else(|| json_str(&format!("{{{}}}", inner), "file_path"))
                .unwrap_or_default();
            let name_part = path.rsplit('/').next().unwrap_or(&path).to_string();
            (
                format!("Read {}", name_part),
                "read".into(),
                if path.is_empty() { vec![] } else { vec![path] },
            )
        }
        "write_file" | "create_file" | "update_file" => {
            let inner = args_raw.trim_matches(|c| c == '{' || c == '}');
            let path = json_str(&format!("{{{}}}", inner), "path")
                .or_else(|| json_str(&format!("{{{}}}", inner), "file_path"))
                .unwrap_or_default();
            let name_part = path.rsplit('/').next().unwrap_or(&path).to_string();
            (
                format!("Write {}", name_part),
                "write".into(),
                if path.is_empty() { vec![] } else { vec![path] },
            )
        }
        _ => {
            let display: String = name
                .split('_')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            (display, "tool".into(), vec![])
        }
    }
}

fn extract_cmd_from_args(args_raw: &str, tool_name: &str) -> String {
    let inner = args_raw.trim_matches(|c| c == '{' || c == '}');
    json_str(&format!("{{{}}}", inner), "command")
        .or_else(|| {
            json_str(&format!("{{{}}}", inner), "path").map(|p| format!("{} {}", tool_name, p))
        })
        .unwrap_or_else(|| args_raw.chars().take(120).collect())
}

fn codex_classify(cmd: &str) -> (String, String) {
    // cmd is typically "bash -lc 'inner'" — extract inner if possible.
    let inner = extract_inner(cmd);
    let first = inner.split_whitespace().next().unwrap_or("").to_lowercase();
    match first.as_str() {
        "ls" | "find" | "cat" | "head" | "tail" | "grep" | "rg" | "fd" | "stat" => {
            (format!("Read {}", first), "read".into())
        }
        "cp" | "mv" | "mkdir" | "touch" | "rm" | "tee" | "sed" | "awk" => {
            (format!("Write {}", first), "write".into())
        }
        "git" => {
            let sub = inner.split_whitespace().nth(1).unwrap_or("").to_string();
            (format!("Git {}", sub).trim().to_string(), "git".into())
        }
        "cargo" | "swift" | "xcodebuild" | "make" | "npm" | "yarn" | "pnpm" => {
            (format!("Build {}", first), "build".into())
        }
        "curl" | "wget" => (format!("Fetch {}", first), "web".into()),
        _ => {
            let display = if first.is_empty() {
                "Shell".to_string()
            } else {
                let mut c = first.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            };
            (display, "run".into())
        }
    }
}

fn extract_inner(cmd: &str) -> String {
    // "bash -lc 'inner'" → inner
    if let Some(start) = cmd.find('\'') {
        if let Some(end) = cmd[start + 1..].rfind('\'') {
            return cmd[start + 1..start + 1 + end].to_string();
        }
    }
    cmd.to_string()
}

fn codex_file_paths(cmd: &str) -> Vec<String> {
    let inner = extract_inner(cmd);
    let shell_kw = [
        "if", "then", "else", "fi", "do", "done", "for", "while", "in", "echo", "export", "cd",
        "||", "&&", "|", ";", ">", ">>", "<", "2>",
    ];
    inner
        .split_whitespace()
        .filter(|tok| {
            if tok.starts_with('-') {
                return false;
            }
            if shell_kw.contains(tok) {
                return false;
            }
            if tok.contains('/') {
                return true;
            }
            let ext = tok.rsplit('.').next().unwrap_or("");
            [
                "swift", "rs", "ts", "js", "py", "go", "c", "cpp", "h", "toml", "json", "yaml",
                "yml", "md", "txt",
            ]
            .contains(&ext)
        })
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Minimal JSON helpers (shared with gemini-agent but duplicated to keep
// each plugin crate self-contained)
// ---------------------------------------------------------------------------

fn json_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after_key = &json[pos + needle.len()..];
    let colon = after_key.find(':')? + 1;
    let rest = after_key[colon..].trim_start();
    if rest.starts_with('"') {
        parse_json_string(&rest[1..])
    } else {
        None
    }
}

fn json_int(json: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after = &json[pos + needle.len()..];
    let colon = after.find(':')? + 1;
    let rest = after[colon..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_object_raw(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after_key = &json[pos + needle.len()..];
    let colon = after_key.find(':')? + 1;
    let rest = after_key[colon..].trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut end = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

fn parse_json_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}
