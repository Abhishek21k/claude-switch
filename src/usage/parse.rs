use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub key: String,
    pub message_id: Option<String>,
    pub time: DateTime<Utc>,
    pub profile: String,
    pub session: String,
    pub agent_id: Option<String>,
    pub model: String,
    pub input: u64,
    pub output: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub cache_read: u64,
    pub web_searches: u64,
    pub web_fetches: u64,
    pub speed: Option<String>,
    pub service_tier: Option<String>,
    pub inference_geo: Option<String>,
    pub version: Option<String>,
    pub cwd: Option<String>,
    pub touched: Vec<String>,
    #[serde(default)]
    pub cwd_project: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub touched_projects: Vec<String>,
    pub sidechain: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CostState {
    pub time: DateTime<Utc>,
    pub total_usd: Option<f64>,
    pub models: BTreeMap<String, ModelCost>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelCost {
    pub usd: f64,
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

#[derive(Debug)]
pub struct ParsedLine {
    pub session: String,
    pub time: DateTime<Utc>,
    pub title: Option<(String, String)>,
    pub cost: Option<CostState>,
    pub requests: Vec<Request>,
}

fn number(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

fn string<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

fn paths_from_input(input: &Value, found: &mut Vec<String>) {
    if let Some(object) = input.as_object() {
        for (key, value) in object {
            if matches!(key.as_str(), "file_path" | "path" | "notebook_path") {
                if let Some(path) = value.as_str() {
                    found.push(path.to_string());
                }
            } else if value.is_object() || value.is_array() {
                paths_from_input(value, found);
            }
        }
    } else if let Some(array) = input.as_array() {
        for value in array {
            paths_from_input(value, found);
        }
    }
}

fn touched_paths(message: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        for block in blocks {
            if string(block, "type") == Some("tool_use")
                && let Some(input) = block.get("input")
            {
                paths_from_input(input, &mut paths);
            }
        }
    }
    paths
}

struct RequestContext<'a> {
    row: &'a Value,
    profile: &'a str,
    session: &'a str,
    time: DateTime<Utc>,
}

fn request(
    context: &RequestContext<'_>,
    usage: &Value,
    model: &str,
    key: String,
    message_id: Option<String>,
    touched: Vec<String>,
) -> Request {
    let cache = &usage["cache_creation"];
    let tools = &usage["server_tool_use"];
    Request {
        key,
        message_id,
        time: context.time,
        profile: context.profile.to_string(),
        session: context.session.to_string(),
        agent_id: string(context.row, "agentId").map(str::to_string),
        model: model.to_string(),
        input: number(usage, "input_tokens"),
        output: number(usage, "output_tokens"),
        cache_write_5m: number(cache, "ephemeral_5m_input_tokens"),
        cache_write_1h: number(cache, "ephemeral_1h_input_tokens"),
        cache_read: number(usage, "cache_read_input_tokens"),
        web_searches: number(tools, "web_search_requests"),
        web_fetches: number(tools, "web_fetch_requests"),
        speed: string(usage, "speed").map(str::to_string),
        service_tier: string(usage, "service_tier").map(str::to_string),
        inference_geo: string(usage, "inference_geo").map(str::to_string),
        version: string(context.row, "version").map(str::to_string),
        cwd: string(context.row, "cwd").map(str::to_string),
        touched,
        cwd_project: None,
        workspace: None,
        touched_projects: Vec::new(),
        sidechain: context.row.get("isSidechain").and_then(Value::as_bool) == Some(true),
    }
}

fn title(row: &Value, message: &Value) -> Option<(String, String)> {
    let kind = string(row, "type")?;
    match kind {
        "custom-title" => string(row, "customTitle")
            .or_else(|| string(row, "title"))
            .map(|value| (value.to_string(), "rename".to_string())),
        "system" if string(row, "subtype") == Some("ai-title") => string(row, "title")
            .or_else(|| string(row, "aiTitle"))
            .map(|value| (value.to_string(), "ai-title".to_string())),
        "user" => {
            let content = message.get("content")?.as_str()?;
            content
                .strip_prefix("/rename ")
                .map(|value| (value.trim().to_string(), "rename".to_string()))
        }
        _ => None,
    }
}

fn cost(row: &Value, time: DateTime<Utc>) -> Option<CostState> {
    let kind = string(row, "type");
    let subtype = string(row, "subtype");
    if kind != Some("cost-state") && subtype != Some("cost-state") {
        return None;
    }
    let value = row.get("cost").unwrap_or(row);
    let model_usage = value.get("modelUsage")?.as_object()?;
    let mut models = BTreeMap::new();
    for (model, usage) in model_usage {
        let token = |camel: &str, snake: &str| {
            usage
                .get(camel)
                .or_else(|| usage.get(snake))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        if let Some(usd) = usage.get("costUSD").and_then(Value::as_f64) {
            models.insert(
                model.clone(),
                ModelCost {
                    usd,
                    input: token("inputTokens", "input_tokens"),
                    output: token("outputTokens", "output_tokens"),
                    cache_write: token("cacheCreationInputTokens", "cache_creation_input_tokens"),
                    cache_read: token("cacheReadInputTokens", "cache_read_input_tokens"),
                },
            );
        }
    }
    Some(CostState {
        time,
        total_usd: value.get("totalCostUSD").and_then(Value::as_f64),
        models,
    })
}

/// Parse one complete JSONL row. Only selected counters and path keys leave this function.
pub fn parse(line: &[u8], profile: &str) -> Result<Option<ParsedLine>, ()> {
    // Known-bad: a substring match on bare `"model":"sonnet"` inside tool input
    // must not decide the request model; only parsed `message.model` does.
    let row: Value = serde_json::from_slice(line).map_err(|_| ())?;
    let Some(session) = string(&row, "sessionId") else {
        return Ok(None);
    };
    let Some(time) = string(&row, "timestamp")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
    else {
        return Ok(None);
    };
    let message = &row["message"];
    let mut requests = Vec::new();
    if string(&row, "type") == Some("assistant") {
        let usage = &message["usage"];
        if usage.is_object()
            && let Some(model) = string(message, "model")
            && model != "<synthetic>"
        {
            let context = RequestContext {
                row: &row,
                profile,
                session,
                time,
            };
            let id = string(message, "id").unwrap_or("");
            let key = if let Some(request_id) = string(&row, "requestId") {
                serde_json::to_string(&("request", id, request_id)).unwrap()
            } else {
                serde_json::to_string(&("fallback", id, session, time.to_rfc3339())).unwrap()
            };
            let touched = touched_paths(message);
            requests.push(request(
                &context,
                usage,
                model,
                key.clone(),
                (!id.is_empty()).then(|| id.to_string()),
                touched,
            ));
            if let Some(iterations) = usage.get("iterations").and_then(Value::as_array) {
                for (index, iteration) in iterations.iter().enumerate() {
                    if string(iteration, "type") == Some("advisor_message")
                        && let Some(advisor_model) = string(iteration, "model")
                    {
                        requests.push(request(
                            &context,
                            iteration.get("usage").unwrap_or(iteration),
                            advisor_model,
                            format!("{key}:advisor:{index}"),
                            None,
                            Vec::new(),
                        ));
                    }
                }
            }
        }
    }
    Ok(Some(ParsedLine {
        session: session.to_string(),
        time,
        title: title(&row, message),
        cost: cost(&row, time),
        requests,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn advisor_usage_counts_once_and_synthetic_model_is_skipped() {
        // Known-bad: adding `message` iterations repeats top-level usage;
        // ignoring `advisor_message` loses a separately billed request.
        let mut row = json!({"type":"assistant","timestamp":"2030-01-01T12:00:00Z","sessionId":"sample",
        "requestId":"req-1","message":{"id":"msg-1","model":"claude-opus-5","usage":{
            "input_tokens":10,"output_tokens":20,"iterations":[
                {"type":"message","model":"claude-opus-5","input_tokens":10,"output_tokens":20},
                {"type":"advisor_message","model":"claude-haiku-4-5-20251001","usage":{"input_tokens":3,"output_tokens":4}}
            ]}}});
        let parsed = parse(row.to_string().as_bytes(), "sample")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.requests.len(), 2);
        assert_eq!(
            (parsed.requests[0].input, parsed.requests[0].output),
            (10, 20)
        );
        assert_eq!(
            (parsed.requests[1].model.as_str(), parsed.requests[1].input),
            ("claude-haiku-4-5-20251001", 3)
        );
        row["message"]["model"] = json!("<synthetic>");
        assert!(
            parse(row.to_string().as_bytes(), "sample")
                .unwrap()
                .unwrap()
                .requests
                .is_empty()
        );
    }
}
