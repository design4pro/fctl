use super::{out, uuid};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Turn {
    id: String,
    task: String,
    ends: Instant,
}

fn response(id: &Value, value: Value) {
    out(&json!({"id": id, "result": value}));
}

fn error(id: &Value, message: &str) {
    out(&json!({"id": id, "error": {"code": -32602, "message": message}}));
}

fn notify(method: &str, params: Value) {
    out(&json!({"method": method, "params": params}));
}

fn rollout(home: &Path, thread: &str) -> PathBuf {
    home.join("sessions/2026/11/01")
        .join(format!("rollout-2026-11-01T00-00-00-{thread}.jsonl"))
}

fn append(file: &Path, value: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(file.parent().unwrap_or_else(|| Path::new(".")))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    writeln!(file, "{value}")
}

fn task(params: &Value) -> String {
    params["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn finish(home: &Path, thread: &str, turn: Turn, complete: bool) -> std::io::Result<()> {
    if complete {
        let history = rollout(home, thread);
        append(
            &history,
            &json!({"type": "event_msg", "payload": {"type": "token_count"}}),
        )?;
        let calls = std::io::BufReader::new(std::fs::File::open(history)?)
            .lines()
            .map_while(Result::ok)
            .filter(|line| line.contains("\"type\":\"token_count\""))
            .count();
        let item = format!("{}-reply", turn.id);
        let text = format!("Fake Codex finished: {}", turn.task);
        notify(
            "item/agentMessage/delta",
            json!({"threadId":thread,"turnId":turn.id,"itemId":item,"delta":text}),
        );
        notify(
            "item/completed",
            json!({"threadId":thread,"turnId":turn.id,"item":{"type":"agentMessage","id":item,"text":text}}),
        );
        notify(
            "thread/tokenUsage/updated",
            json!({"threadId":thread,"turnId":turn.id,
            "tokenUsage":{"total":{"totalTokens":22*calls,"inputTokens":16*calls,"cachedInputTokens":0,"outputTokens":6*calls,"reasoningOutputTokens":0},
            "last":{"totalTokens":22,"inputTokens":16,"cachedInputTokens":0,"outputTokens":6,"reasoningOutputTokens":0},
            "modelContextWindow":200000}}),
        );
    }
    notify(
        "turn/completed",
        json!({"threadId":thread,"turn":{"id":turn.id,"status":if complete {"completed"} else {"interrupted"},"items":[],"error":null}}),
    );
    Ok(())
}

pub(crate) fn run(args: &[String]) -> std::io::Result<()> {
    if args.iter().any(|arg| arg == "--version" || arg == "-v") {
        println!("codex-cli 0.172.0");
        return Ok(());
    }
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "CODEX_HOME is required")
        })?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut thread = String::new();
    let mut turn: Option<Turn> = None;
    loop {
        let wait = turn.as_ref().map_or(Duration::from_secs(60), |t| {
            t.ends.saturating_duration_since(Instant::now())
        });
        let line = match rx.recv_timeout(wait) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(turn) = turn.take() {
                    finish(&home, &thread, turn, true)?;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = &value["id"];
        let params = &value["params"];
        match value["method"].as_str() {
            Some("initialize") => response(
                id,
                json!({"userAgent":"fctl/0.172.0","platformFamily":"unix","platformOs":std::env::consts::OS}),
            ),
            Some("initialized") => {}
            Some("thread/start" | "thread/resume") => {
                if let Some(previous) = params["threadId"].as_str() {
                    if previous.len() != 36
                        || !previous.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
                    {
                        error(id, "bad thread ID");
                        continue;
                    }
                    thread = previous.to_owned();
                    if !rollout(&home, &thread).is_file() {
                        error(id, "thread not found");
                        continue;
                    }
                } else {
                    thread = uuid();
                    append(
                        &rollout(&home, &thread),
                        &json!({"type":"session_meta","payload":{"id":thread}}),
                    )?;
                }
                response(
                    id,
                    json!({"thread":{"id":thread,"path":rollout(&home,&thread)},
                    "model":params["model"].as_str().unwrap_or("gpt-fake-codex"),"modelProvider":"fctl_chatgpt"}),
                );
            }
            Some("turn/start") => {
                let next = uuid();
                let task = task(params);
                let wait = if task.contains("slow") {
                    Duration::from_secs(3)
                } else {
                    Duration::from_millis(15)
                };
                append(
                    &rollout(&home, &thread),
                    &json!({"type":"event_msg","payload":{"type":"item_completed","item":{
                        "type":"UserMessage","message":task,"client_id":params["clientUserMessageId"]
                    }}}),
                )?;
                response(
                    id,
                    json!({"turn":{"id":next,"status":"inProgress","items":[],"error":null}}),
                );
                notify(
                    "turn/started",
                    json!({"threadId":thread,"turn":{"id":next,"status":"inProgress"}}),
                );
                turn = Some(Turn {
                    id: next,
                    task,
                    ends: Instant::now() + wait,
                });
            }
            Some("turn/steer") => {
                if let Some(turn) = turn.as_mut().filter(|t| params["expectedTurnId"] == t.id) {
                    let text = task(params);
                    append(
                        &rollout(&home, &thread),
                        &json!({"type":"event_msg","payload":{"type":"item_completed","item":{
                            "type":"UserMessage","message":text,"client_id":params["clientUserMessageId"]
                        }}}),
                    )?;
                    turn.task.push('\n');
                    turn.task.push_str(&text);
                    response(id, json!({"turnId":turn.id}));
                } else {
                    error(id, "no active turn to steer");
                }
            }
            Some("turn/interrupt") => {
                response(id, json!({}));
                if let Some(turn) = turn.take() {
                    finish(&home, &thread, turn, false)?;
                }
            }
            _ if !id.is_null() => error(id, "unsupported fake Codex request"),
            _ => {}
        }
    }
}
