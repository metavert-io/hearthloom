//! The OpenClaw bridge (governance depth: dispatch-only). Wakes an
//! OpenClaw agent turn through its gateway webhook (`POST {url}/hooks/agent`,
//! bearer = the agent's sealed hook token — OpenClaw webhooks are off
//! until the owner enables them in OpenClaw's config). OpenClaw runs the
//! turn on its own; the task tells it how to report back
//! (`POST /v1/agents/tasks/{id}/report` with a one-task token), and the
//! task stays "running" until that report arrives or the time limit ends.
//!
//! Hearthloom decides only whether the task is SENT. What OpenClaw does
//! with its own skills is governed by OpenClaw's approval settings.

use anyhow::{anyhow, Result};
use hearthloom_core::store::{AgentRow, AgentTaskRow};
use serde_json::{json, Value};

use crate::AppState;

pub fn hook_body(agent: &AgentRow, task: &AgentTaskRow, report_url: &str, token: &str) -> Value {
    let message = format!(
        "{}{}\n\nWhen you are done, report the outcome with one HTTP request:\n\
         POST {report_url}\nAuthorization: Bearer {token}\nContent-Type: application/json\n\
         {{\"ok\": true, \"result\": \"<your short summary>\"}}\n\
         (use \"ok\": false and describe the problem if you could not do it).",
        super::preamble(agent),
        task.brief
    );
    let mut body = json!({
        "message": message,
        "name": "Hearthloom",
        "wakeMode": "now",
        "deliver": false,
    });
    if let Some(id) = agent.config["agent_id"].as_str().filter(|a| !a.trim().is_empty()) {
        body["agentId"] = json!(id);
    }
    body
}

pub async fn run(_state: &AppState, agent: &AgentRow, task: &AgentTaskRow, token: &str) -> Result<String> {
    let url = agent.config["url"]
        .as_str()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow!("set the address of {}'s gateway on the Agents page", agent.name))?;
    let hook_token = super::secret(&agent.id, "TOKEN")
        .ok_or_else(|| anyhow!("add {}'s webhook token on the Agents page", agent.name))?;
    // Where OpenClaw can reach this node: the node's public address when
    // it has one, else loopback (OpenClaw on the same machine).
    let me = std::env::var("HEARTHLOOM_PUBLIC_URL")
        .unwrap_or_else(|_| format!("http://127.0.0.1:{}", std::env::var("HEARTHLOOM_PORT").unwrap_or_else(|_| "43278".into())));
    let report_url = format!("{}/v1/agents/tasks/{}/report", me.trim_end_matches('/'), task.id);
    let response = reqwest::Client::new()
        .post(format!("{url}/hooks/agent"))
        .timeout(std::time::Duration::from_secs(30))
        .bearer_auth(hook_token)
        .json(&hook_body(agent, task, &report_url, token))
        .send()
        .await
        .map_err(|e| anyhow!("couldn't reach OpenClaw at {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(anyhow!("OpenClaw answered {}", response.status()));
    }
    Ok("sent to OpenClaw — waiting for its report".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_body_tells_openclaw_how_to_report() {
        let agent = AgentRow {
            id: "ag_o".into(), name: "Claw".into(), harness: "openclaw".into(),
            config: json!({"agent_id": "main"}), secret_name: None, key_id: None, enabled: true,
            created_at: String::new(), updated_at: String::new(),
        };
        let task = AgentTaskRow {
            id: "at_9".into(), ask_id: None, agent_id: Some("ag_o".into()), request: "r".into(),
            brief: "Text Ada the address".into(), classes: vec![], tainted: None, sensitive: false,
            status: "running".into(), result: None, error: None, node: None, actions: 0,
            created_at: String::new(), updated_at: String::new(),
        };
        let body = hook_body(&agent, &task, "http://h/v1/agents/tasks/at_9/report", "hlt_x");
        let message = body["message"].as_str().unwrap();
        assert!(message.contains("Text Ada the address") && message.contains("hlt_x"));
        assert!(message.contains("/v1/agents/tasks/at_9/report"));
        assert_eq!(body["agentId"], "main");
        assert_eq!(body["deliver"], false);
    }
}
