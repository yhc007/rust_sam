//! Flow execution engine — runs multi-step flow pipelines.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use sam_core::{CronStore, FlowDef, FlowStep, SamConfig};
use sam_memory_adapter::MemoryAdapter;

use crate::llm_client::LlmClient;
use crate::tools::{execute_builtin_no_flow, ToolContext};
use crate::types::ChatMessage;

/// Result of running a single flow.
#[derive(Debug)]
pub struct FlowResult {
    pub flow_name: String,
    pub outputs: HashMap<String, String>,
    pub success: bool,
    pub error: Option<String>,
}

/// Execute a flow definition step by step.
///
/// Variable interpolation: `{{step_name.output}}` is replaced with the
/// output of a previously executed step.
pub async fn run_flow(
    flow: &FlowDef,
    client: &dyn LlmClient,
    memory: Option<&mut MemoryAdapter>,
    config: &SamConfig,
    cron_store: Option<Arc<Mutex<CronStore>>>,
    sender_handle: &str,
) -> FlowResult {
    let mut outputs: HashMap<String, String> = HashMap::new();
    let mut mem_opt = memory;

    info!(flow = %flow.name, steps = flow.steps.len(), "starting flow execution");

    for (i, step) in flow.steps.iter().enumerate() {
        let step_name = step.step_name().to_string();
        info!(flow = %flow.name, step = %step_name, index = i, "executing step");

        let result = match step {
            FlowStep::Llm { prompt, max_tokens, .. } => {
                let expanded = interpolate(prompt, &outputs);
                exec_llm_step(client, &expanded, *max_tokens).await
            }
            FlowStep::Tool { tool, input, .. } => {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                let expanded = interpolate(&input_str, &outputs);
                let input_val: serde_json::Value =
                    serde_json::from_str(&expanded).unwrap_or(serde_json::Value::Object(Default::default()));
                let mut ctx = ToolContext {
                    memory: mem_opt.as_deref_mut(),
                    config,
                    cron_store: cron_store.clone(),
                    sender_handle: sender_handle.to_string(),
                    flow_store: None,
                    llm_client: None,
                    mcp_clients: None,
                    skill_store: None,
                };
                execute_builtin_no_flow(tool, &input_val, &mut ctx).await
            }
            FlowStep::Send { body, .. } => {
                // Send steps produce the interpolated body as output.
                // Actual sending is handled by the caller who reads
                // FlowResult.outputs for "send" steps.
                let expanded = interpolate(body, &outputs);
                Ok(expanded)
            }
        };

        match result {
            Ok(output) => {
                outputs.insert(step_name, output);
            }
            Err(e) => {
                warn!(flow = %flow.name, step = step.step_name(), error = %e, "step failed");
                return FlowResult {
                    flow_name: flow.name.clone(),
                    outputs,
                    success: false,
                    error: Some(format!("Step '{}' failed: {}", step.step_name(), e)),
                };
            }
        }
    }

    info!(flow = %flow.name, "flow completed successfully");
    FlowResult {
        flow_name: flow.name.clone(),
        outputs,
        success: true,
        error: None,
    }
}

/// Execute an LLM step — single-shot prompt, no tool use.
async fn exec_llm_step(
    client: &dyn LlmClient,
    prompt: &str,
    _max_tokens: u32,
) -> Result<String, String> {
    let messages = vec![ChatMessage::text("user", prompt)];
    let resp = client
        .chat(
            "You are a helpful assistant executing a workflow step. Be concise.",
            &messages,
            None,
        )
        .await
        .map_err(|e| format!("LLM call failed: {e}"))?;
    Ok(resp.text)
}

/// Replace `{{step_name.output}}` placeholders with actual outputs.
fn interpolate(template: &str, outputs: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (name, value) in outputs {
        let placeholder = format!("{{{{{name}.output}}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_works() {
        let mut outputs = HashMap::new();
        outputs.insert("step1".to_string(), "hello world".to_string());
        outputs.insert("search".to_string(), "result data".to_string());

        let template = "Previous: {{step1.output}} and {{search.output}}";
        let result = interpolate(template, &outputs);
        assert_eq!(result, "Previous: hello world and result data");
    }

    #[test]
    fn interpolation_leaves_unknown_placeholders() {
        let outputs = HashMap::new();
        let template = "{{unknown.output}} stays";
        let result = interpolate(template, &outputs);
        assert_eq!(result, "{{unknown.output}} stays");
    }
}
