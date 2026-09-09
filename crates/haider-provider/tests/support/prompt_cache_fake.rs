#![allow(clippy::expect_used)] // This module contains only synthetic test infrastructure.
//! Synthetic HTTP cache oracle. No response is a canned cache hit: every read
//! requires an exact previously written provider prefix and the same account.
//! Token units are deterministic byte/4 estimates, not a provider tokenizer.
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
pub struct PrefixCache {
    entries: HashMap<String, u64>,
}

fn neutral_block(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("cache_control");
    }
    value
}

pub fn neutral(value: &Value) -> Value {
    let mut body = value.clone();
    for section in ["tools", "system"] {
        if let Some(Value::Array(blocks)) = body.get_mut(section) {
            for block in blocks {
                *block = neutral_block(block);
            }
        }
    }
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        for message in messages {
            if let Some(Value::Array(blocks)) = message.get_mut("content") {
                for block in blocks {
                    *block = neutral_block(block);
                }
            }
        }
    }
    // API-key marking changes only the system envelope from text to blocks.
    if let Some(Value::String(text)) = body.get("system") {
        body["system"] = json!([{"type":"text", "text":text}]);
    }
    body
}

pub fn hash(value: &Value) -> String {
    blake3::hash(&serde_json::to_vec(value).expect("synthetic JSON"))
        .to_hex()
        .to_string()
}

impl PrefixCache {
    pub fn observe(&mut self, body: &Value, account: &str, controls_valid: bool) -> Value {
        let mut prefix = Vec::new();
        let mut candidates = Vec::new();
        for section in ["tools", "system", "messages"] {
            let blocks = match body.get(section) {
                Some(Value::Array(blocks)) => blocks.clone(),
                Some(Value::String(text)) => vec![json!({"type":"text", "text":text})],
                _ => Vec::new(),
            };
            for block in blocks {
                if section == "messages" {
                    for content in block["content"].as_array().expect("content blocks") {
                        prefix.push(json!({"section":section, "role":block["role"], "block":neutral_block(content)}));
                        if content.get("cache_control").is_some() {
                            candidates.push((prefix.clone(), content["cache_control"].clone()));
                        }
                    }
                } else {
                    prefix.push(json!({"section":section, "block":neutral_block(&block)}));
                    if block.get("cache_control").is_some() {
                        candidates.push((prefix.clone(), block["cache_control"].clone()));
                    }
                }
            }
        }
        let units = |prefix: &[Value]| {
            (serde_json::to_vec(prefix).expect("prefix").len() as u64).div_ceil(4)
        };
        let logical = units(&prefix);
        let mut read = 0;
        let mut written_end = 0;
        let mut hashes = Vec::new();
        let mut one_hour = false;
        if controls_valid && candidates.len() <= 4 {
            for (prefix, control) in candidates {
                if control["type"] != "ephemeral"
                    || !matches!(control["ttl"].as_str(), Some("5m" | "1h"))
                {
                    continue;
                }
                let tokens = units(&prefix);
                if tokens < 512 {
                    continue;
                }
                let key = hash(&json!({"account":account,"model":body["model"],"prefix":prefix}));
                if let Some(cached) = self.entries.get(&key) {
                    read = read.max(*cached);
                }
                hashes.push(key.clone());
                self.entries.insert(key, tokens);
                written_end = written_end.max(tokens);
                one_hour = control["ttl"] == "1h";
            }
        }
        let write = written_end.saturating_sub(read);
        json!({"logical":logical,"read":read,"write":write,"uncached":logical - read,"fresh":logical - read - write,"write_5m":if one_hour {0} else {write},"write_1h":if one_hour {write} else {0},"prefix_hashes":hashes})
    }
}

pub struct RecordingCacheHttp {
    pub url: String,
    pub records: Arc<Mutex<Vec<Value>>>,
    pub task: tokio::task::JoinHandle<()>,
}

impl RecordingCacheHttp {
    pub async fn start(requests: usize, tool_loop: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fake bind");
        let url = format!(
            "http://{}/v1/messages",
            listener.local_addr().expect("address")
        );
        let records = Arc::new(Mutex::new(Vec::new()));
        let saved = records.clone();
        let task = tokio::spawn(async move {
            let mut cache = PrefixCache::default();
            for ordinal in 0..requests {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut bytes = Vec::new();
                let split = loop {
                    let mut buffer = [0; 8192];
                    let count = socket.read(&mut buffer).await.expect("read headers");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        break end + 4;
                    }
                };
                let headers: BTreeMap<String, String> = String::from_utf8_lossy(&bytes[..split])
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
                    .collect();
                let length: usize = headers["content-length"].parse().expect("length");
                while bytes.len() - split < length {
                    let mut buffer = [0; 8192];
                    let count = socket.read(&mut buffer).await.expect("read body");
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let body: Value =
                    serde_json::from_slice(&bytes[split..split + length]).expect("JSON request");
                let account = headers
                    .get("authorization")
                    .or_else(|| headers.get("x-api-key"))
                    .expect("synthetic auth");
                let valid = headers
                    .get("anthropic-version")
                    .is_some_and(|value| value == "2023-06-01")
                    && (!headers.contains_key("authorization")
                        || [
                            "oauth-2025-04-20",
                            "prompt-caching-scope-2026-01-05",
                            "extended-cache-ttl-2025-04-11",
                        ]
                        .iter()
                        .all(|beta| {
                            headers
                                .get("anthropic-beta")
                                .is_some_and(|value| value.contains(beta))
                        }));
                let usage = cache.observe(&body, account, valid);
                let sanitized: BTreeMap<_, _> = headers
                    .into_iter()
                    .filter(|(key, _)| {
                        matches!(
                            key.as_str(),
                            "anthropic-version"
                                | "anthropic-beta"
                                | "content-type"
                                | "x-haider-turn"
                                | "x-haider-request-kind"
                        )
                    })
                    .collect();
                saved
                    .lock()
                    .expect("records")
                    .push(json!({"ordinal":ordinal,"headers":sanitized,"body":body,"usage":usage}));
                let call = tool_loop && ordinal % 2 == 0;
                let content = if call {
                    json!({"type":"tool_use","id":format!("cache-call-{ordinal}"),"name":"request_input","input":{}})
                } else {
                    json!({"type":"text","text":"fixture done"})
                };
                let start = json!({"type":"message_start","message":{"id":format!("msg-{ordinal}"),"type":"message","role":"assistant","model":body["model"],"content":[],"usage":{"input_tokens":usage["fresh"],"output_tokens":0,"cache_read_input_tokens":usage["read"],"cache_creation_input_tokens":usage["write"],"cache_creation":{"ephemeral_5m_input_tokens":usage["write_5m"],"ephemeral_1h_input_tokens":usage["write_1h"]}}}});
                let content_start =
                    json!({"type":"content_block_start","index":0,"content_block":content});
                let content_stop = json!({"type":"content_block_stop","index":0});
                let delta = json!({"type":"message_delta","delta":{"stop_reason":if call {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":3}});
                let stop = json!({"type":"message_stop"});
                let mut events = vec![start, content_start];
                if call {
                    let args = json!({"kind":"choice","title":"Fixture","options":[{"key":"ok","label":"OK"}],"default":"ok"});
                    events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args.to_string()}}));
                }
                events.extend([content_stop, delta, stop]);
                let response = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().expect("event type")
                        )
                    })
                    .collect::<String>();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.expect("write response");
            }
        });
        Self { url, records, task }
    }
}
