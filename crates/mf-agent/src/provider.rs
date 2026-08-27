use crate::config::{ProviderConfig, ProviderKind};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 一次工具调用
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON 字符串参数
    pub arguments: String,
}

/// 对话消息(兼容两种协议的中间表示)
#[derive(Clone, Debug)]
pub struct ChatMessage {
    pub role: String, // system | user | assistant | tool
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: text.into(),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }

    pub fn assistant(text: String, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: text,
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(call_id: String, text: String) -> Self {
        Self {
            role: "tool".into(),
            content: text,
            tool_calls: vec![],
            tool_call_id: Some(call_id),
        }
    }
}

/// 工具定义(发给模型的 schema)
#[derive(Clone, Debug)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// 一次模型回复的内容块
#[derive(Clone, Debug)]
pub enum AssistantBlock {
    Text(String),
    ToolUse(ToolCall),
}

/// 调用提供方,返回一轮回复(阻塞,worker 线程中调用)
pub fn complete(
    provider: &ProviderConfig,
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<Vec<AssistantBlock>> {
    match provider.kind {
        ProviderKind::Mock => mock_complete(messages),
        ProviderKind::Openai => openai_complete(provider, messages, tools),
        ProviderKind::Anthropic => anthropic_complete(provider, messages, tools),
    }
}

// ---------- OpenAI 兼容 ----------

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    tool_choice: &'static str,
}

fn openai_complete(
    provider: &ProviderConfig,
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<Vec<AssistantBlock>> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let mut msgs = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "system" => msgs.push(serde_json::json!({"role": "system", "content": m.content})),
            "user" => msgs.push(serde_json::json!({"role": "user", "content": m.content})),
            "assistant" => {
                let tool_calls: Vec<serde_json::Value> = m
                    .tool_calls
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "id": c.id,
                            "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments}
                        })
                    })
                    .collect();
                msgs.push(serde_json::json!({
                    "role": "assistant",
                    "content": if m.content.is_empty() { serde_json::Value::Null } else { serde_json::json!(m.content) },
                    "tool_calls": tool_calls,
                }));
            }
            "tool" => msgs.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                "content": m.content,
            })),
            _ => {}
        }
    }
    let tool_defs: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect();
    let req = OpenAiRequest {
        model: provider.model.clone(),
        messages: msgs,
        tools: tool_defs,
        tool_choice: "auto",
    };
    let resp = http_post_json(&url, &provider.api_key, &req)?;
    let choice = resp
        .pointer("/choices/0/message")
        .cloned()
        .ok_or_else(|| anyhow!("openai: missing choices[0].message"))?;
    let mut blocks = Vec::new();
    if let Some(text) = choice.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            blocks.push(AssistantBlock::Text(text.to_string()));
        }
    }
    if let Some(calls) = choice.get("tool_calls").and_then(|c| c.as_array()) {
        for c in calls {
            let id = c
                .pointer("/id")
                .and_then(|v| v.as_str())
                .unwrap_or("call")
                .to_string();
            let name = c
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = c
                .pointer("/function/arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("{}")
                .to_string();
            if !name.is_empty() {
                blocks.push(AssistantBlock::ToolUse(ToolCall { id, name, arguments: args }));
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(AssistantBlock::Text("(空回复)".into()));
    }
    Ok(blocks)
}

// ---------- Anthropic ----------

fn anthropic_complete(
    provider: &ProviderConfig,
    messages: &[ChatMessage],
    tools: &[ToolDef],
) -> Result<Vec<AssistantBlock>> {
    let url = format!("{}/v1/messages", provider.base_url.trim_end_matches('/'));
    let mut system = String::new();
    let mut msgs: Vec<serde_json::Value> = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "system" => {
                if !system.is_empty() {
                    system.push('\n');
                }
                system.push_str(&m.content);
            }
            "user" => msgs.push(serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": m.content}]
            })),
            "assistant" => {
                let mut content = Vec::new();
                if !m.content.is_empty() {
                    content.push(serde_json::json!({"type": "text", "text": m.content}));
                }
                for c in &m.tool_calls {
                    let input: serde_json::Value = serde_json::from_str(&c.arguments).unwrap_or_default();
                    content.push(serde_json::json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": input
                    }));
                }
                msgs.push(serde_json::json!({"role": "assistant", "content": content}));
            }
            "tool" => msgs.push(serde_json::json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": m.tool_call_id.clone().unwrap_or_default(), "content": m.content}]
            })),
            _ => {}
        }
    }
    // 合并相邻同角色消息
    let mut merged: Vec<serde_json::Value> = Vec::new();
    for m in msgs {
        if let Some(last) = merged.last_mut() {
            if last.get("role") == m.get("role") {
                let mut content = last["content"].as_array().cloned().unwrap_or_default();
                content.extend(m["content"].as_array().cloned().unwrap_or_default());
                last["content"] = serde_json::Value::Array(content);
                continue;
            }
        }
        merged.push(m);
    }
    let tool_defs: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect();
    let body = serde_json::json!({
        "model": provider.model,
        "max_tokens": 8192,
        "system": system,
        "messages": merged,
        "tools": tool_defs,
    });
    let resp = http_post_json(&url, &provider.api_key, &body)?;
    let content = resp
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .ok_or_else(|| anyhow!("anthropic: missing content"))?;
    let mut blocks = Vec::new();
    for b in content {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    blocks.push(AssistantBlock::Text(t.to_string()));
                }
            }
            Some("tool_use") => {
                let id = b.get("id").and_then(|v| v.as_str()).unwrap_or("t").to_string();
                let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let input = b.get("input").cloned().unwrap_or_default();
                blocks.push(AssistantBlock::ToolUse(ToolCall {
                    id,
                    name,
                    arguments: input.to_string(),
                }));
            }
            _ => {}
        }
    }
    if blocks.is_empty() {
        blocks.push(AssistantBlock::Text("(空回复)".into()));
    }
    Ok(blocks)
}

// ---------- HTTP ----------

/// 连接测试:GET {base_url}/models,返回模型数(设置页"测试连接"用)
pub fn test_connection(base_url: &str, api_key: &str) -> Result<usize> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = ureq::get(&url)
        .timeout(Duration::from_secs(10))
        .set("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", api_key));
    }
    let resp = req.call().map_err(|e| anyhow!("http: {}", e))?;
    let status = resp.status();
    let text = resp.into_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        let cut = &text[..text.len().min(160)];
        anyhow::bail!("HTTP {}: {}", status, cut);
    }
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    let n = v
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    Ok(n)
}

fn http_post_json(url: &str, api_key: &str, body: &impl Serialize) -> Result<serde_json::Value> {
    let mut req = ureq::post(url)
        .timeout(Duration::from_secs(300))
        .set("Content-Type", "application/json");
    if !api_key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {}", api_key));
    }
    let resp = req
        .send_json(serde_json::to_value(body)?)
        .map_err(|e| anyhow!("http: {}", e))?;
    let status = resp.status();
    let text = resp
        .into_string()
        .context("read response body")?;
    if !(200..300).contains(&status) {
        // 尽量提取错误信息
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| v.get("message").and_then(|m| m.as_str()).map(|s| s.to_string()))
            })
            .unwrap_or_else(|| text.chars().take(300).collect());
        anyhow::bail!("HTTP {}: {}", status, detail);
    }
    serde_json::from_str(&text).with_context(|| format!("parse json: {}", &text[..text.len().min(200)]))
}

// ---------- Mock(无网络,演示任务流转) ----------

fn mock_complete(messages: &[ChatMessage]) -> Result<Vec<AssistantBlock>> {
    // 依据已用过的工具推断下一步
    let used: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter_map(|m| {
            messages
                .iter()
                .find(|x| x.role == "assistant" && x.tool_calls.iter().any(|c| Some(&c.id) == m.tool_call_id.as_ref()))
                .and_then(|a| a.tool_calls.first().map(|c| c.name.as_str()))
        })
        .collect();
    let user_text = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    std::thread::sleep(Duration::from_millis(500));

    // 规划者:产出两个任务(第二个依赖第一个),然后收尾
    if user_text.contains("PLANNER_OBJECTIVE:") {
        if !used.contains(&"create_task") {
            let obj = user_text
                .split("PLANNER_OBJECTIVE:")
                .nth(1)
                .unwrap_or("目标")
                .lines()
                .next()
                .unwrap_or("目标")
                .chars()
                .take(60)
                .collect::<String>();
            return Ok(vec![AssistantBlock::ToolUse(ToolCall {
                id: "mock-1".into(),
                name: "create_task".into(),
                arguments: serde_json::json!({
                    "spec": format!("[mock 规划] 分析并梳理:「{}」\n梳理工作区文件结构,确认涉及的模块与改动点。", obj.trim())
                })
                .to_string(),
            })]);
        }
        if used.iter().filter(|t| **t == "create_task").count() == 1 {
            // 从上一条 create_task 的工具结果解析出任务 id,建立依赖
            let first_id = messages
                .iter()
                .rev()
                .find(|m| m.role == "tool" && m.content.contains("id="))
                .and_then(|m| {
                    m.content
                        .split("id=")
                        .nth(1)
                        .and_then(|s| s.trim().parse::<i64>().ok())
                })
                .unwrap_or(1);
            return Ok(vec![AssistantBlock::ToolUse(ToolCall {
                id: "mock-2".into(),
                name: "create_task".into(),
                arguments: serde_json::json!({
                    "spec": "[mock 规划] 执行主体改动并记录结果到 .mf-agent/ 目录",
                    "deps": [first_id]
                })
                .to_string(),
            })]);
        }
        if used.iter().filter(|t| **t == "create_task").count() == 2 {
            return Ok(vec![AssistantBlock::ToolUse(ToolCall {
                id: "mock-3".into(),
                name: "finalize_plan".into(),
                arguments: "{}".into(),
            })]);
        }
    }

    // 工作者:先写文件,再汇报
    if !used.contains(&"fs_write") {
        let task_id = messages
            .iter()
            .find(|m| m.role == "user" && m.content.contains("TASK_ID:"))
            .and_then(|m| {
                m.content
                    .split("TASK_ID:")
                    .nth(1)
                    .and_then(|s| s.lines().next())
                    .and_then(|s| s.trim().parse::<i64>().ok())
            })
            .unwrap_or(0);
        return Ok(vec![AssistantBlock::ToolUse(ToolCall {
            id: "mock-w1".into(),
            name: "fs_write".into(),
            arguments: serde_json::json!({
                "path": format!(".mf-agent/task-{}.md", task_id),
                "content": format!(
                    "# mock 任务 {}\n\n由 MonkeyFence mock 工作者生成于 {:?}。\n\n(接入真实 LLM 提供方后,这里会是实际的工作产出。)\n",
                    task_id, std::time::SystemTime::now()
                )
            })
            .to_string(),
        })]);
    }
    if !used.contains(&"complete_task") {
        return Ok(vec![AssistantBlock::ToolUse(ToolCall {
            id: "mock-w2".into(),
            name: "complete_task".into(),
            arguments: serde_json::json!({
                "summary": "mock 工作者已完成:写入 .mf-agent/task-*.md 并自检通过。"
            })
            .to_string(),
        })]);
    }
    Ok(vec![AssistantBlock::Text("done".into())])
}
