//! Project App Server notifications onto fctl's transcript event shape.

use serde_json::{Value, json};
use std::collections::HashSet;

pub enum Update {
    Messages {
        journal: Vec<Value>,
        stream: Vec<Value>,
    },
    Finished {
        current: bool,
        result: Value,
    },
    Usage {
        window: Option<u64>,
        call: Option<Value>,
    },
}

#[derive(Default)]
pub struct Events {
    tools: HashSet<String>,
    live_text: Option<String>,
}

impl Events {
    pub fn project(&mut self, value: &Value, active: Option<&str>) -> Option<Update> {
        let method = text(value, "method")?;
        let p = value.get("params")?;
        let messages = match method {
            "item/agentMessage/delta" => {
                let (id, delta) = (text(p, "itemId")?, text(p, "delta")?);
                let mut stream = Vec::new();
                if self.live_text.as_deref() != Some(id) {
                    self.live_text = Some(id.to_owned());
                    stream.push(live(
                        json!({"type": "message_start", "message": {"id": id}}),
                    ));
                    stream.push(live(json!({
                        "type": "content_block_start", "index": 0,
                        "content_block": {"type": "text", "text": ""}
                    })));
                }
                stream.push(live(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": delta}
                })));
                (Vec::new(), stream)
            }
            "item/started" | "item/completed" => {
                let item = p.get("item")?;
                let id = text(item, "id")?;
                let kind = text(item, "type")?;
                let completed = method == "item/completed";
                if let Some((call, result)) = tool(item, id, kind) {
                    let mut journal = Vec::new();
                    if self.tools.insert(id.to_owned()) {
                        journal.push(call);
                    }
                    if completed {
                        self.tools.remove(id);
                        journal.push(result);
                    }
                    (journal, Vec::new())
                } else if completed {
                    let block = match kind {
                        "agentMessage" | "plan" => {
                            json!({"type": "text", "text": text(item, "text")?})
                        }
                        "reasoning" => {
                            let summary = item
                                .get("summary")?
                                .as_array()?
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n");
                            json!({"type": "thinking", "thinking": summary})
                        }
                        "contextCompaction" => {
                            return Some(Update::Messages {
                                journal: vec![
                                    json!({"type": "system", "subtype": "compact_boundary"}),
                                ],
                                stream: Vec::new(),
                            });
                        }
                        _ => return None,
                    };
                    let mut stream = Vec::new();
                    if self.live_text.as_deref() == Some(id) {
                        self.live_text = None;
                        stream.push(live(json!({"type": "content_block_stop", "index": 0})));
                    }
                    (vec![assistant(id, block)], stream)
                } else {
                    return None;
                }
            }
            "turn/completed" => {
                let turn = p.get("turn")?;
                let id = text(turn, "id")?;
                let status = text(turn, "status")?;
                let (subtype, errors) = match status {
                    "completed" => ("success", Vec::new()),
                    "interrupted" => ("interrupted", vec!["Codex turn interrupted"]),
                    _ => (
                        "error_during_execution",
                        vec!["Codex could not complete the turn"],
                    ),
                };
                let current = Some(id) == active;
                if current {
                    self.live_text = None;
                    self.tools.clear();
                }
                return Some(Update::Finished {
                    current,
                    result: json!({
                        "type": "result", "subtype": subtype, "is_error": status != "completed",
                        "errors": errors, "duration_ms": turn.get("durationMs"),
                        "queued_turn_count": usize::from(!current && active.is_some()),
                        "codex_turn_id": id
                    }),
                });
            }
            "thread/tokenUsage/updated" => {
                let usage = p.get("tokenUsage")?;
                return Some(Update::Usage {
                    window: usage.get("modelContextWindow").and_then(Value::as_u64),
                    call: usage_call(p, usage),
                });
            }
            _ => return None,
        };
        Some(Update::Messages {
            journal: messages.0,
            stream: messages.1,
        })
    }
}

fn usage_call(params: &Value, usage: &Value) -> Option<Value> {
    let thread = text(params, "threadId")?;
    let total = usage.pointer("/total/totalTokens")?.as_u64()?;
    let last = usage.get("last")?;
    let input = last.get("inputTokens")?.as_u64()?;
    let cached = last.get("cachedInputTokens")?.as_u64()?;
    let output = last.get("outputTokens")?.as_u64()?;
    Some(json!({
        "type": "system", "subtype": "codex_usage", "call_id": format!("{thread}:{total}"),
        "usage": {
            "input_tokens": input.saturating_sub(cached), "cache_read_input_tokens": cached,
            "output_tokens": output
        }
    }))
}

fn tool(item: &Value, id: &str, kind: &str) -> Option<(Value, Value)> {
    let (name, input, output) = match kind {
        "commandExecution" => (
            "Bash",
            json!({"command": text(item, "command")?, "cwd": item.get("cwd")}),
            text(item, "aggregatedOutput")
                .unwrap_or_default()
                .to_owned(),
        ),
        "fileChange" => (
            "Edit",
            json!({"file_path": item.pointer("/changes/0/path"), "changes": item.get("changes")}),
            item.get("changes")?
                .as_array()?
                .iter()
                .filter_map(|v| text(v, "path"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "mcpToolCall" | "dynamicToolCall" => (
            text(item, "tool")?,
            item.get("arguments").cloned().unwrap_or_else(|| json!({})),
            if let Some(result) = item
                .get("contentItems")
                .or_else(|| item.get("result"))
                .filter(|v| !v.is_null())
            {
                result.to_string()
            } else {
                item.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            },
        ),
        "collabAgentToolCall" => (
            text(item, "tool")?,
            json!({"prompt": item.get("prompt"), "agent_threads": item.get("receiverThreadIds")}),
            item.get("agentsStates")
                .map(Value::to_string)
                .unwrap_or_default(),
        ),
        "webSearch" => (
            "WebSearch",
            json!({"query": item.get("query")}),
            String::new(),
        ),
        "imageView" => (
            "ViewImage",
            json!({"file_path": item.get("path")}),
            String::new(),
        ),
        _ => return None,
    };
    let call = assistant(
        id,
        json!({"type": "tool_use", "id": id, "name": name, "input": input}),
    );
    let is_error = matches!(text(item, "status"), Some("failed" | "declined"))
        || item.get("success").and_then(Value::as_bool) == Some(false)
        || item
            .get("exitCode")
            .and_then(Value::as_i64)
            .is_some_and(|n| n != 0);
    let result = json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": output, "is_error": is_error}
        ]}
    });
    Some((call, result))
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}

fn live(event: Value) -> Value {
    json!({"type": "stream_event", "event": event, "agent_kind": "codex"})
}

fn assistant(id: &str, block: Value) -> Value {
    json!({"type": "assistant", "message": {"id": id, "role": "assistant", "content": [block]}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_a_repeated_usage_update_once() -> anyhow::Result<()> {
        let mut events = Events::default();
        let update = json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": "test-thread", "turnId": "test-turn",
                "tokenUsage": {
                    "modelContextWindow": 100000,
                    "total": { "totalTokens": 103 },
                    "last": { "inputTokens": 100, "cachedInputTokens": 70, "outputTokens": 3 }
                }
            }
        });
        let Some(Update::Usage {
            window: Some(100_000),
            call: Some(event),
        }) = events.project(&update, Some("test-turn"))
        else {
            anyhow::bail!("Expected a usage event and context window");
        };
        let mut stats = fctl_proto::session::SessionStats::default();
        let mut counted = fctl_domain::events::Counted::default();
        for time in [1, 2] {
            fctl_domain::events::apply_stats(&mut stats, &mut counted, &event, time, true);
        }
        assert_eq!(stats.input_tokens, 30);
        assert_eq!(stats.cache_read_tokens, 70);
        assert_eq!(stats.output_tokens, 3);
        assert_eq!(stats.context_tokens, Some(100));
        Ok(())
    }

    #[test]
    fn streams_text_and_emits_one_tool_call_with_its_result() -> anyhow::Result<()> {
        let mut events = Events::default();
        let delta = json!({"method": "item/agentMessage/delta", "params": {"itemId": "a", "delta": "Hello"}});
        let Some(Update::Messages { stream, journal }) = events.project(&delta, Some("turn"))
        else {
            anyhow::bail!("Expected text deltas");
        };
        assert!(journal.is_empty());
        assert_eq!(stream.len(), 3);
        assert_eq!(stream[2]["event"]["delta"]["text"], "Hello");

        let mut command = json!({
            "method": "item/started", "params": {"item": {
                "id": "c", "type": "commandExecution", "command": "gh pr create"
            }}
        });
        let Some(Update::Messages { journal, .. }) = events.project(&command, Some("turn")) else {
            anyhow::bail!("Expected command");
        };
        assert_eq!(journal[0]["message"]["content"][0]["name"], "Bash");
        command["method"] = json!("item/completed");
        command["params"]["item"]["aggregatedOutput"] = json!("https://github.com/org/repo/pull/2");
        let Some(Update::Messages { journal, .. }) = events.project(&command, Some("turn")) else {
            anyhow::bail!("Expected command result");
        };
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0]["message"]["content"][0]["tool_use_id"], "c");
        assert_eq!(
            journal[0]["message"]["content"][0]["content"],
            "https://github.com/org/repo/pull/2"
        );
        Ok(())
    }
}
