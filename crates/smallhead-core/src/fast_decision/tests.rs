use super::*;
use crate::{
    fast_route::FastBackend,
    tool::{ToolContext, ToolImpl},
};
use serde_json::json;

fn laya_request() -> LayaRequest {
    serde_json::from_value(json!({"state":{"query":"中文回复"},"questions":{
        "language":{"type":"choice","instructions":"选择语言","criteria":{"zh":"中文","en":"英文"}},
        "priority":{"type":"score","instructions":"紧急程度","criteria":["低","中","高"]},
        "history":{"type":"noul","instructions":"是否需要历史记录"}
    }}))
    .unwrap()
}
fn qwen_request(policy: RankingPolicy) -> QwenRequest {
    QwenRequest {
        query: "语言偏好".into(),
        instruction: "排序相关记忆".into(),
        candidates: vec![
            Candidate {
                id: "first".into(),
                text: "用户偏好中文".into(),
            },
            Candidate {
                id: "second".into(),
                text: "使用 Rust".into(),
            },
            Candidate {
                id: "third".into(),
                text: "需要简短".into(),
            },
        ],
        policy,
    }
}
#[test]
fn laya_keeps_all_typed_answers_and_rejects_bad_output() {
    let request = laya_request();
    let raw = json!({"answers":{
        "language":{"choice":"zh","confidence":0.8,"probabilities":{"zh":0.8,"en":0.2}},
        "priority":{"score":1.2,"confidence":0.7,"probabilities":{"0":0.1,"1":0.6,"2":0.3}},
        "history":{"noul":0.2,"confidence":0.6}
    }});
    let output = laya::decode(&request, &raw).unwrap();
    assert_eq!(output["language"]["selected"], "zh");
    assert_eq!(output["priority"]["score"], 1.2);
    assert_eq!(output["history"]["value"], 0.2);
    for path in [
        "missing",
        "bad_choice",
        "bad_probability",
        "bad_score",
        "bad_noul",
        "wrong_type",
    ] {
        let mut malformed = raw.clone();
        match path {
            "missing" => {
                malformed["answers"]
                    .as_object_mut()
                    .unwrap()
                    .remove("history");
            }
            "bad_choice" => malformed["answers"]["language"]["choice"] = json!("en"),
            "bad_probability" => malformed["answers"]["language"]["probabilities"]["zh"] = json!(2),
            "bad_score" => malformed["answers"]["priority"]["score"] = json!(3),
            "bad_noul" => malformed["answers"]["history"]["noul"] = json!(false),
            _ => malformed["answers"]["history"]["type"] = json!("choice"),
        }
        assert!(laya::decode(&request, &malformed).is_err(), "{path}");
    }
}
#[test]
fn ranking_policies_preserve_ids_and_defer_on_ambiguity() {
    let raw = json!({"results":[{"index":2,"score":0.4},{"index":0,"score":0.9},{"index":1,"score":0.8}]});
    let cases = [
        (RankingPolicy::RankAll, 3),
        (
            RankingPolicy::TopK {
                k: 2,
                min_score: None,
            },
            2,
        ),
        (RankingPolicy::Filter { min_score: 0.85 }, 1),
        (
            RankingPolicy::SelectOne {
                min_score: 0.85,
                min_margin: 0.05,
            },
            1,
        ),
        (
            RankingPolicy::SelectOne {
                min_score: 0.95,
                min_margin: 0.05,
            },
            0,
        ),
        (
            RankingPolicy::SelectOne {
                min_score: 0.7,
                min_margin: 0.2,
            },
            0,
        ),
        (RankingPolicy::Filter { min_score: 0.95 }, 0),
    ];
    for (policy, count) in cases {
        let output = qwen::decode(&qwen_request(policy), raw.clone()).unwrap();
        assert_eq!(output["ranking"][0]["id"], "first");
        assert_eq!(output["selected"].as_array().unwrap().len(), count);
        assert_eq!(
            output["status"],
            if count == 0 { "defer" } else { "advisory" }
        );
    }
    let tie = json!({"results":[{"index":2,"score":0.1},{"index":1,"score":0.9},{"index":0,"score":0.9}]});
    let output = qwen::decode(
        &qwen_request(RankingPolicy::SelectOne {
            min_score: 0.0,
            min_margin: 0.0,
        }),
        tie,
    )
    .unwrap();
    assert_eq!(output["status"], "defer");
    assert_eq!(output["ranking"][0]["id"], "first");
    for bad in [
        json!({"results":[]}),
        json!({"results":[{"index":0,"score":0.1},{"index":0,"score":0.2},{"index":2,"score":0.3}]}),
        json!({"results":[{"index":0,"score":0.1},{"index":1,"score":2},{"index":2,"score":0.3}]}),
    ] {
        assert!(qwen::decode(&qwen_request(RankingPolicy::RankAll), bad).is_err());
    }
    let mut duplicate = qwen_request(RankingPolicy::RankAll);
    duplicate.candidates[1].id = "first".into();
    assert!(qwen::validate(&duplicate).is_err());
    assert!(qwen::validate(&qwen_request(RankingPolicy::TopK {
        k: 0,
        min_score: None
    }))
    .is_err());
}

async fn server(path: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(socket);
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        assert!(line.starts_with(&format!("POST /{path} ")));
        let mut length = 0;
        loop {
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((key, v)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse::<usize>().unwrap();
                }
            }
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        let response=if path=="evaluate" {
            assert_eq!(request["spec_hash"].as_str().unwrap().len(),64);
            assert_eq!(request["questions"].as_object().unwrap().len(),3);
            json!({"protocol_version":2,"spec_id":request["spec_id"],"spec_hash":request["spec_hash"],"answers":{
                "language":{"choice":"zh","confidence":0.8,"probabilities":{"zh":0.8,"en":0.2}},
                "priority":{"score":1.2,"confidence":0.7,"probabilities":{"0":0.1,"1":0.6,"2":0.3}},
                "history":{"noul":0.2,"confidence":0.6}
            }})
        } else {
            assert_eq!(request["documents"].as_array().unwrap().len(),3);
            json!({"results":[{"index":1,"score":0.8},{"index":0,"score":0.9},{"index":2,"score":0.1}]})
        }.to_string();
        stream.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",response.len(),response).as_bytes()).await.unwrap();
    });
    (endpoint, task)
}
#[tokio::test]
async fn agent_can_discover_and_use_both_backends_with_real_http() {
    use crate::{AgentLoop, AgentMessage, ContentBlock, MockProvider, MockResponse, ToolKind};
    let (laya, ltask) = server("evaluate").await;
    let (qwen, qtask) = server("rerank").await;
    let tool = FastDecisionTool {
        client: DecisionClient {
            laya_endpoint: laya,
            qwen_endpoint: qwen,
            timeout_ms: 500,
        },
        allowed_backends: vec![FastBackend::Laya, FastBackend::Qwen],
    };
    let provider = std::sync::Arc::new(MockProvider::new(vec![
        MockResponse::ToolCall {
            name: "fast_decision".into(),
            input: json!({"action":"discover"}),
        },
        MockResponse::ToolCall {
            name: "fast_decision".into(),
            input: json!({"action":"laya","request":laya_request()}),
        },
        MockResponse::ToolCall {
            name: "fast_decision".into(),
            input: json!({"action":"qwen","request":qwen_request(RankingPolicy::TopK {k:2,min_score:None})}),
        },
        MockResponse::AssistantText {
            text: "决策完成，尚未执行业务".into(),
        },
    ]));
    let mut agent = AgentLoop::new(provider, vec![ToolKind::FastDecision(tool)], "test");
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        agent.run_one_turn(AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "决策".into(),
            }],
            timestamp: 0,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    ltask.await.unwrap();
    qtask.await.unwrap();
    assert_eq!(agent.state.executed_tools.len(), 3);
    for result in &agent.state.executed_tools {
        assert!(!result.is_error);
    }
    let ContentBlock::Text { text } = &agent.state.executed_tools[2].content[0] else {
        panic!()
    };
    let output: Value = serde_json::from_str(text).unwrap();
    assert_eq!(output["selected"][0]["id"], "first");
    assert_eq!(output["selected"].as_array().unwrap().len(), 2);
}
#[tokio::test]
async fn permissions_unknown_fields_and_service_failure_are_observable() {
    let mut tool = FastDecisionTool {
        client: DecisionClient::default(),
        allowed_backends: vec![],
    };
    let ctx = ToolContext::default();
    assert!(tool
        .execute(json!({"action":"discover"}), &ctx)
        .await
        .is_ok());
    assert!(tool
        .execute(json!({"action":"laya","request":laya_request()}), &ctx)
        .await
        .is_err());
    assert!(tool
        .execute(
            json!({"action":"discover","endpoint":"http://example.com"}),
            &ctx
        )
        .await
        .is_err());
    tool.allowed_backends.push(FastBackend::Laya);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    tool.client.laya_endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(matches!(
        tool.execute(json!({"action":"laya","request":laya_request()}), &ctx)
            .await,
        Err(ToolError::Execution(_))
    ));
    let mut invalid_request = laya_request();
    if let LayaQuestion::Choice { criteria, .. } =
        invalid_request.questions.get_mut("language").unwrap()
    {
        criteria.clear();
    }
    assert!(matches!(
        tool.client.evaluate(&invalid_request).await,
        Err(ToolError::InvalidInput(_))
    ));
}
