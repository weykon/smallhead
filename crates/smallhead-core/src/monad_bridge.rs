//! Model tool adapter; Monad lifecycle execution belongs to the shared runtime.
use crate::monad::{MonadRegistry, StaticMonadRegistry};
use crate::tool::{ToolError, ToolImpl};
use crate::{
    Budget, ContentBlock, MonadHandoff, MonadId, ResultSlot, TaskId, ToolContext, ToolResult,
    ToolSpec, TraceMeta, SCHEMA_VERSION_V1,
};
use async_trait::async_trait;
use serde_json::{json, Value};

/// A demo bridge bound by the runtime to one parent task, never by model input.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct MonadBridgeTool {
    /// Parent task to which all dispatched results belong.
    pub parent_task_id: TaskId,
    /// Per-business preference; missing entries use the main model.
    pub model_preferences: std::collections::BTreeMap<String, crate::fast_route::ModelPreference>,
}

#[async_trait]
impl ToolImpl for MonadBridgeTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "monad_bridge".into(), label: "Discover or dispatch business Monad".into(),
            description: "首先使用 action=discover 读取业务目录和输入格式。然后使用 action=dispatch，提供目录中的 monad_id、明确的 objective 和 inputs。每次派发执行一个子任务并返回实际结果。多个操作应分别派发，禁止编造结果。任务 ID 和结果回填槽位由程序分配。discover 的 routing_capabilities 区分支持的后端和当前允许的选择。仅在 agent_choice 模式下，dispatch 可填写 model_choice=large_model|laya|qwen；缺省沿用主模型。用户要求用快模型处理算术时，在 allowed_choices 内选择 laya 或 qwen 并明确填写 model_choice；bridge 内部先调用快速方法选择，再由本地 calculation 求值。快速模型不直接算数不代表不能用于这条链路，不必额外调用 fast_decision 做同一次方法选择。多个表达式分别派发，例如 {action:dispatch,monad_id:calculation,objective:计算100乘100,inputs:{expression:100*100},model_choice:laya}。手动偏好必须遵守。".into(),
            input_schema: json!({"type":"object","properties":{
                "action":{"type":"string","enum":["discover","dispatch"]},
                "monad_id":{"type":"string"},"objective":{"type":"string"},
                "inputs":{"type":"object"},"model_choice":{"type":"string","enum":["default","large_model","laya","qwen"],"description":"本次派发的模型选择，须属于目录 allowed_choices；不能设置服务地址。"}},"required":["action"],"additionalProperties":false}),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        if ctx.cancel {
            return Err(ToolError::Execution("cancelled".into()));
        }
        let registry = StaticMonadRegistry::with_fixtures();
        let output = match input["action"].as_str() {
            Some("discover") => {
                let mut entries = vec![];
                for (id, field) in [
                    ("calculation", "expression"),
                    ("echo", "echo_text"),
                    ("greeting", "name"),
                ] {
                    let instance = registry
                        .instantiate(&MonadId::new(id))
                        .map_err(|e| ToolError::Execution(e.to_string()))?;
                    let description = if id == "calculation" {
                        "只包含两个操作数和一个运算符的算术表达式，例如 100+100 或 100*100；不支持复合表达式。"
                    } else {
                        "此业务所需的字符串输入。"
                    };
                    let field_type = if id == "greeting" {
                        json!(["string", "number", "boolean", "null"])
                    } else {
                        json!("string")
                    };
                    let required = if id == "greeting" {
                        vec![]
                    } else {
                        vec![field]
                    };
                    let preference = self.model_preferences.get(id).cloned().unwrap_or_default();
                    let choices = allowed_model_choices(id, &preference);
                    entries.push(json!({"monad_id":id,
                        "routing_capabilities":{"supported_fast_backends":if id=="calculation" {vec!["laya","qwen"]} else {vec![]},
                            "allowed_choices":choices,"decision_scope":"method_selection","execution_backend":"local",
                            "usage_hint":if id=="calculation" { "dispatch 填写允许的 model_choice=laya|qwen 时，bridge 内部调用快速模型选择算术方法，本地代码求值；不需要先用 fast_decision 计算数值。" } else { "此业务由本地代码执行，没有快速方法适配。" }},"catalog":instance.catalog_entry(),
                        "model_preference":self.model_preferences.get(id).cloned().unwrap_or_default(),
                        "input_schema":{"type":"object","properties":{(field):{"type":field_type,"description":if id == "greeting" { "问候对象，接受字符串、数字、布尔值；缺省或 null 时使用 world。" } else { description }}},
                        "required":required,"additionalProperties":false}}));
                }
                json!({"catalog":entries})
            }
            Some("dispatch") => {
                let id = input["monad_id"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidInput("monad_id required".into()))?;
                let objective = input["objective"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| ToolError::InvalidInput("objective required".into()))?;
                let inputs = input["inputs"]
                    .as_object()
                    .ok_or_else(|| ToolError::InvalidInput("inputs must be an object".into()))?;
                let child = registry
                    .instantiate(&MonadId::new(id))
                    .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
                let field = match id {
                    "calculation" => "expression",
                    "echo" => "echo_text",
                    "greeting" => "name",
                    _ => return Err(ToolError::InvalidInput("unsupported monad".into())),
                };
                let valid = if id == "greeting" {
                    inputs.keys().all(|key| key == "name")
                        && inputs.get("name").is_none_or(|value| {
                            value.is_string()
                                || value.is_number()
                                || value.is_boolean()
                                || value.is_null()
                        })
                } else {
                    inputs.len() == 1 && inputs.get(field).and_then(Value::as_str).is_some()
                };
                if !valid {
                    return Err(ToolError::InvalidInput(format!(
                        "invalid inputs for {id}: expected {field}; calculation/echo require one string, greeting accepts an optional string/number/bool/null name and no extra fields"
                    )));
                }
                let configured = self.model_preferences.get(id).cloned().unwrap_or_default();
                let choice = match input.get("model_choice") {
                    None => "default",
                    Some(value) => value.as_str().ok_or_else(|| {
                        ToolError::InvalidInput("model_choice must be a string".into())
                    })?,
                };
                let preference = resolve_model_choice(id, &configured, choice)?;
                if let crate::fast_route::ModelPreference::PreferFast {
                    backend, endpoint, ..
                } = &preference
                {
                    let name = match backend {
                        crate::fast_route::FastBackend::Laya => "laya",
                        crate::fast_route::FastBackend::Qwen => "qwen",
                    };
                    crate::execution::ensure_fast_service(ctx, name, endpoint).await?;
                }
                let mut route = if id == "calculation" {
                    crate::fast_route::route_calculation(
                        &preference,
                        objective,
                        inputs[field].as_str().unwrap(),
                    )
                    .await
                } else {
                    json!({"used":"large_model","reason":"no fast method adapter for this business"})
                };
                route["requested_choice"] = json!(choice);
                route["selection_source"] = json!(if matches!(
                    configured,
                    crate::fast_route::ModelPreference::AgentChoice { .. }
                ) && choice != "default"
                {
                    "agent"
                } else {
                    "configured_default"
                });
                route["effective_preference"] = json!(&preference);
                crate::diagnostics::demo_event(
                    "route",
                    "model-route",
                    json!({"business":id,"route":route}),
                );
                let mut execution_inputs = inputs.clone();
                if let Some(expression) = route["execution_expression"].as_str() {
                    execution_inputs.insert("expression".into(), json!(expression));
                }
                let task_id = TaskId::new();
                let handoff = MonadHandoff {
                    schema_version: SCHEMA_VERSION_V1.into(),
                    task_id: task_id.clone(),
                    parent_task_id: Some(self.parent_task_id.clone()),
                    monad_id: MonadId::new(id),
                    attempt_id: 1,
                    objective: objective.into(),
                    inputs: execution_inputs,
                    relevant_context: vec![],
                    constraints: vec![],
                    completion_criteria: "返回业务执行结果".into(),
                    result_slot: ResultSlot(format!("children.{}", task_id.0)),
                    depends_on: vec![],
                    budget: Budget::default(),
                    issued_at_ms: now_ms(),
                    trace: TraceMeta {
                        frame_ref: "monad-demo-section-5".into(),
                        request_id: task_id.0.clone(),
                    },
                };
                crate::diagnostics::demo_event("monad", "dispatch", json!(&handoff));
                let runtime = ctx
                    .monad_runtime
                    .clone()
                    .unwrap_or_else(crate::monad::MonadRuntime::shared);
                let run = runtime
                    .execute(
                        child,
                        handoff.clone(),
                        ctx.cancellation.clone(),
                        std::sync::Arc::new(crate::monad::BindingValidator),
                    )
                    .await
                    .map_err(|e| ToolError::Execution(e.to_string()))?;
                let result = run.result;
                crate::diagnostics::demo_event("monad", "return", json!(&result));
                // Preserve handoff association even for WaitingContext, whose wire format only carries task_id.
                json!({"handoff":handoff,"result":result,"run_id":run.run_id,"model_route":route})
            }
            _ => {
                return Err(ToolError::InvalidInput(
                    "action must be discover or dispatch".into(),
                ))
            }
        };
        if input["action"] == "discover" {
            crate::diagnostics::demo_event("monad", "discover", output.clone());
        }
        Ok(ToolResult {
            tool_call_id: String::new(),
            is_error: output["result"]["status"] == "failed",
            content: vec![ContentBlock::Text {
                text: output.to_string(),
            }],
        })
    }
}

/// 可选值同时用于目录声明与派发校验，防止配置和执行分歧。
fn allowed_model_choices(
    id: &str,
    preference: &crate::fast_route::ModelPreference,
) -> Vec<&'static str> {
    use crate::fast_route::{FastBackend, ModelPreference};
    if id != "calculation" {
        return vec!["default", "large_model"];
    }
    match preference {
        ModelPreference::AgentChoice { .. } => vec!["default", "large_model", "laya", "qwen"],
        ModelPreference::PreferFast {
            backend: FastBackend::Laya,
            ..
        } => vec!["default", "laya"],
        ModelPreference::PreferFast {
            backend: FastBackend::Qwen,
            ..
        } => vec!["default", "qwen"],
        ModelPreference::LargeModel => vec!["default", "large_model"],
    }
}

fn resolve_model_choice(
    id: &str,
    preference: &crate::fast_route::ModelPreference,
    choice: &str,
) -> Result<crate::fast_route::ModelPreference, ToolError> {
    use crate::fast_route::{FastBackend, ModelPreference};
    if !allowed_model_choices(id, preference).contains(&choice) {
        return Err(ToolError::InvalidInput(format!(
            "model_choice {choice:?} is not allowed for {id}; allowed choices: {:?}",
            allowed_model_choices(id, preference)
        )));
    }
    if id != "calculation" {
        return Ok(ModelPreference::LargeModel);
    }
    match preference {
        ModelPreference::AgentChoice {
            laya_endpoint,
            qwen_endpoint,
            timeout_ms,
        } => match choice {
            "laya" => Ok(ModelPreference::PreferFast {
                backend: FastBackend::Laya,
                endpoint: laya_endpoint.clone(),
                timeout_ms: *timeout_ms,
            }),
            "qwen" => Ok(ModelPreference::PreferFast {
                backend: FastBackend::Qwen,
                endpoint: qwen_endpoint.clone(),
                timeout_ms: *timeout_ms,
            }),
            _ => Ok(ModelPreference::LargeModel),
        },
        _ => Ok(preference.clone()),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentLoop, AgentMessage, MockProvider, MockResponse, ToolKind};
    use std::sync::Arc;

    #[tokio::test]
    async fn model_calls_dispatch_children_and_feed_actual_results_back() {
        let parent = TaskId::new();
        let provider = Arc::new(MockProvider::new(vec![
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: json!({"action":"discover"}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: json!({"action":"dispatch","monad_id":"calculation","objective":"先算加法","inputs":{"expression":"100+100"}}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: json!({"action":"dispatch","monad_id":"calculation","objective":"再算乘法","inputs":{"expression":"100*100"}}),
            },
            MockResponse::AssistantText {
                text: "done".into(),
            },
        ]));
        let mut agent = AgentLoop::new(
            provider,
            vec![ToolKind::MonadBridge(MonadBridgeTool {
                parent_task_id: parent.clone(),
                model_preferences: Default::default(),
            })],
            "test",
        );
        let summary = agent
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "计算两项".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();
        assert_eq!(summary.tool_calls_executed, 3);
        let results: Vec<Value> = agent
            .state
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::ToolResult { content, .. } => match &content[0] {
                    ContentBlock::Text { text } => Some(serde_json::from_str(text).unwrap()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(results[0]["catalog"].as_array().unwrap().len(), 3);
        assert_eq!(results[1]["result"]["output"]["value"], 200.0);
        assert_eq!(results[2]["result"]["output"]["value"], 10000.0);
        assert_ne!(
            results[1]["result"]["task_id"],
            results[2]["result"]["task_id"]
        );
        for value in &results[1..] {
            assert_eq!(value["result"]["parent_task_id"], parent.0);
            assert_eq!(value["result"]["task_id"], value["handoff"]["task_id"]);
            assert_eq!(
                value["result"]["result_slot"],
                value["handoff"]["result_slot"]
            );
        }
    }

    #[tokio::test]
    async fn bridge_rejects_unknown_business_and_invalid_input() {
        let tool = MonadBridgeTool {
            parent_task_id: TaskId::new(),
            model_preferences: Default::default(),
        };
        for input in [
            json!({"action":"dispatch","monad_id":"unknown","objective":"test","inputs":{}}),
            json!({"action":"dispatch","monad_id":"calculation","objective":"test","inputs":{"expression":123}}),
        ] {
            assert!(tool.execute(input, &ToolContext::default()).await.is_err());
        }
        let result = tool.execute(json!({"action":"dispatch","monad_id":"calculation","objective":"test","inputs":{"expression":""}}), &ToolContext::default()).await.unwrap();
        let ContentBlock::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["result"]["status"], "waiting_context");
    }
    #[tokio::test]
    async fn greeting_discovery_matches_scalar_and_default_dispatch() {
        let tool = MonadBridgeTool {
            parent_task_id: TaskId::new(),
            model_preferences: Default::default(),
        };
        let result = tool
            .execute(json!({"action":"discover"}), &ToolContext::default())
            .await
            .unwrap();
        let ContentBlock::Text { text } = &result.content[0] else {
            panic!("expected text")
        };
        let catalog: Value = serde_json::from_str(text).unwrap();
        let schema = &catalog["catalog"][2]["input_schema"];
        assert_eq!(
            schema["properties"]["name"]["type"],
            json!(["string", "number", "boolean", "null"])
        );
        assert_eq!(schema["required"], json!([]));
        for (inputs, expected) in [
            (json!({"name":3000}), "3000"),
            (json!({"name":true}), "true"),
            (json!({}), "world"),
            (json!({"name":null}), "world"),
        ] {
            let result = tool.execute(json!({"action":"dispatch","monad_id":"greeting","objective":"打招呼","inputs":inputs}), &ToolContext::default()).await.unwrap();
            let ContentBlock::Text { text } = &result.content[0] else {
                panic!("expected text")
            };
            let value: Value = serde_json::from_str(text).unwrap();
            assert_eq!(value["result"]["output"]["name"], expected);
        }
    }

    #[tokio::test]
    async fn agent_recovers_from_bad_input_and_keeps_prior_results() {
        use crate::{AgentLoop, AgentMessage, MockProvider, MockResponse, ToolKind};
        let responses = vec![
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: json!({"action":"dispatch","monad_id":"calculation","objective":"计算","inputs":{"expression":"50*60"}}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: json!({"action":"dispatch","monad_id":"greeting","objective":"打招呼","inputs":{"name":{}}}),
            },
            MockResponse::ToolCall {
                name: "monad_bridge".into(),
                input: json!({"action":"dispatch","monad_id":"greeting","objective":"打招呼","inputs":{"name":3000}}),
            },
            MockResponse::AssistantText {
                text: "Hello, 3000!".into(),
            },
        ];
        struct RecordingProvider {
            inner: MockProvider,
            requests: std::sync::Mutex<Vec<crate::ProviderRequest>>,
        }
        #[async_trait]
        impl crate::Provider for RecordingProvider {
            fn id(&self) -> &str {
                "recording"
            }
            fn model(&self) -> &str {
                "mock"
            }
            fn context_window(&self) -> usize {
                4096
            }
            fn cache_key(&self, _: &crate::ProviderRequest) -> String {
                String::new()
            }
            fn abort_signal(&self) -> crate::CancellationToken {
                crate::CancellationToken::new()
            }
            async fn stream(
                &self,
                request: crate::ProviderRequest,
            ) -> Result<
                std::pin::Pin<Box<dyn futures::Stream<Item = crate::ProviderEvent> + Send>>,
                crate::ProviderError,
            > {
                self.requests.lock().unwrap().push(request.clone());
                crate::Provider::stream(&self.inner, request).await
            }
        }
        let provider = std::sync::Arc::new(RecordingProvider {
            inner: MockProvider::new(responses),
            requests: Default::default(),
        });
        let mut agent = AgentLoop::new(
            provider.clone(),
            vec![ToolKind::MonadBridge(MonadBridgeTool {
                parent_task_id: TaskId::new(),
                model_preferences: Default::default(),
            })],
            "test",
        );
        let summary = agent
            .run_one_turn(AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "计算后打招呼".into(),
                }],
                timestamp: 0,
            })
            .await
            .unwrap();
        assert_eq!(summary.tool_calls_executed, 3);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(matches!(
            requests[2].messages.last(),
            Some(crate::LlmMessage::ToolResult { is_error: true, .. })
        ));
        assert!(requests[2].messages.iter().any(|message| match message {
            crate::LlmMessage::ToolResult {
                content,
                is_error: false,
                ..
            } => content.iter().any(|block| match block {
                ContentBlock::Text { text } => text.contains("3000"),
                _ => false,
            }),
            _ => false,
        }));

        let results: Vec<_> = agent
            .state
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => Some((tool_call_id, content, *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(
            results.iter().map(|result| result.2).collect::<Vec<_>>(),
            vec![false, true, false]
        );
        let ContentBlock::Text { text } = &results[1].1[0] else {
            panic!("expected error text")
        };
        assert_eq!(
            serde_json::from_str::<Value>(text).unwrap()["error"]["kind"],
            "invalid_input"
        );
        // 每一个工具调用都必须回填一次，并保持对应 ID；失败后仍有最终回复。
        let calls: Vec<_> = agent
            .state
            .messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Assistant { tool_calls, .. } => {
                    Some(tool_calls.iter().map(|call| &call.id))
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            calls,
            results.iter().map(|result| result.0).collect::<Vec<_>>()
        );
        let request_messages: Vec<_> = agent
            .state
            .messages
            .iter()
            .filter_map(crate::ToLlmMessage::to_llm)
            .collect();
        assert!(request_messages.iter().any(|message| matches!(
            message,
            crate::LlmMessage::ToolResult { is_error: true, .. }
        )));
    }
    #[test]
    fn manual_preference_is_respected_and_auto_choice_is_bounded() {
        use crate::fast_route::{FastBackend, ModelPreference};
        let auto = ModelPreference::AgentChoice {
            laya_endpoint: "http://laya.local".into(),
            qwen_endpoint: "http://qwen.local".into(),
            timeout_ms: 100,
        };
        assert_eq!(
            resolve_model_choice("calculation", &auto, "default").unwrap(),
            ModelPreference::LargeModel
        );
        assert_eq!(
            resolve_model_choice("calculation", &auto, "large_model").unwrap(),
            ModelPreference::LargeModel
        );
        let manual = ModelPreference::PreferFast {
            backend: FastBackend::Laya,
            endpoint: "http://laya.local".into(),
            timeout_ms: 100,
        };
        assert_eq!(
            resolve_model_choice("calculation", &manual, "default").unwrap(),
            manual
        );
        assert!(resolve_model_choice("calculation", &manual, "qwen").is_err());
        assert!(resolve_model_choice("calculation", &ModelPreference::LargeModel, "laya").is_err());
        assert!(resolve_model_choice("echo", &auto, "laya").is_err());
        assert!(resolve_model_choice("calculation", &auto, "http://arbitrary.local").is_err());
    }

    #[tokio::test]
    async fn agent_selects_fast_backend_per_dispatch_and_can_keep_main_choice() {
        use crate::fast_route::ModelPreference;
        use crate::{AgentLoop, AgentMessage, MockProvider, MockResponse, ToolKind};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        for backend in ["laya", "qwen"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut first_line = String::new();
                socket.read_line(&mut first_line).await.unwrap();
                assert!(first_line.starts_with(if backend == "laya" {
                    "POST /decide "
                } else {
                    "POST /rerank "
                }));
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                if backend == "laya" {
                    assert!(body["state"]["query"].as_str().unwrap().contains("7+8"));
                } else {
                    assert!(body["query"].as_str().unwrap().contains("7+8"));
                }
                let body = if backend == "laya" {
                    json!({"operation":"add","probabilities":{"add":1.0,"subtract":0.0,"multiply":0.0,"divide":0.0,"defer":0.0}})
                } else {
                    json!({"results":(0..5).map(|i|json!({"index":i,"score":if i==0 {1.0} else {0.0}})).collect::<Vec<_>>()})
                };
                let body = body.to_string();
                socket.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            });
            let provider = std::sync::Arc::new(MockProvider::new(vec![
                MockResponse::ToolCall {
                    name: "monad_bridge".into(),
                    input: json!({"action":"discover"}),
                },
                MockResponse::ToolCall {
                    name: "monad_bridge".into(),
                    input: json!({"action":"dispatch","monad_id":"calculation","objective":"计算7加8","inputs":{"expression":"7+8"},"model_choice":backend}),
                },
                MockResponse::ToolCall {
                    name: "monad_bridge".into(),
                    input: json!({"action":"dispatch","monad_id":"calculation","objective":"计算1加1","inputs":{"expression":"1+1"},"model_choice":"large_model"}),
                },
                MockResponse::AssistantText {
                    text: "15 和 2".into(),
                },
            ]));
            let tool = MonadBridgeTool {
                parent_task_id: TaskId::new(),
                model_preferences: std::collections::BTreeMap::from([(
                    "calculation".into(),
                    ModelPreference::AgentChoice {
                        laya_endpoint: endpoint.clone(),
                        qwen_endpoint: endpoint,
                        timeout_ms: 500,
                    },
                )]),
            };
            let mut agent = AgentLoop::new(provider, vec![ToolKind::MonadBridge(tool)], "test");
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                agent.run_one_turn(AgentMessage::User {
                    content: vec![ContentBlock::Text {
                        text: "选择模型计算两项".into(),
                    }],
                    timestamp: 0,
                }),
            )
            .await
            .unwrap()
            .unwrap();
            server.await.unwrap();
            let results: Vec<Value> = agent
                .state
                .executed_tools
                .iter()
                .map(|result| {
                    assert!(!result.is_error);
                    let ContentBlock::Text { text } = &result.content[0] else {
                        panic!("expected text")
                    };
                    serde_json::from_str(text).unwrap()
                })
                .collect();
            assert_eq!(
                results[0]["catalog"][0]["routing_capabilities"]["allowed_choices"],
                json!(["default", "large_model", "laya", "qwen"])
            );
            assert_eq!(results[1]["model_route"]["backend"], backend);
            assert_eq!(results[1]["model_route"]["used"], "fast_model");
            assert_eq!(results[1]["model_route"]["selection_source"], "agent");
            assert_eq!(results[1]["result"]["output"]["value"], 15.0);
            assert_eq!(results[2]["model_route"]["used"], "large_model");
            assert_eq!(results[2]["result"]["output"]["value"], 2.0);
        }
    }
}
