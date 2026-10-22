//! Optional local-model method routing. Scores are not business correctness probabilities.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Business-level preference; absent configuration always means the existing large-model path.
#[derive(Clone, Debug, Default, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ModelPreference {
    /// Keep the method/expression supplied by the main model.
    #[default]
    LargeModel,
    /// 允许主 Agent 按派发任务选择后端；服务地址仍由程序配置。
    AgentChoice {
        /// Laya 服务地址。
        laya_endpoint: String,
        /// Qwen 服务地址。
        qwen_endpoint: String,
        /// 快速模型请求超时。
        timeout_ms: u64,
    },
    /// Attempt a bounded local selection, falling back to the main model's existing expression.
    PreferFast {
        /// Local scoring protocol.
        backend: FastBackend,
        /// Service base URL, configured by the application, never tool input.
        endpoint: String,
        /// Whole HTTP request timeout.
        timeout_ms: u64,
    },
}

/// Supported local service protocols.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastBackend {
    /// Laya /decide choice distribution.
    Laya,
    /// Official Qwen native /rerank independent candidate scores.
    Qwen,
}

const IDS: [&str; 5] = ["add", "subtract", "multiply", "divide", "defer"];
const DESCRIPTIONS: [&str; 5] = [
    "两个数相加，求和，加法 +",
    "两个数相减，求差，减法 -",
    "两个数相乘，求积，乘法 *",
    "两个数相除，求商，除法 /",
    "没有合适的单一算子、请求有歧义或包含多个操作；交回主模型处理",
];

/// Pick a method for an already extracted expression, returning observable routing evidence.
/// This does not extract parameters or replace main-model planning.
pub async fn route_calculation(
    preference: &ModelPreference,
    objective: &str,
    expression: &str,
) -> Value {
    let ModelPreference::PreferFast {
        backend,
        endpoint,
        timeout_ms,
    } = preference
    else {
        return json!({"used":"large_model","reason":"default: main model supplied expression"});
    };
    let started = std::time::Instant::now();
    let expected = simple_operator(expression);
    // A deterministic cross-check prevents high-scoring but wrong arithmetic choices.
    // Unsupported syntax stays on the existing large-model/fixture validation path.
    let outcome = if let Some(expected) = expected {
        match request_choice(*backend, endpoint, *timeout_ms, objective, expression).await {
            Ok((selected, raw)) if selected == expected => json!({
                "used":"fast_model", "backend":backend, "selected":selected,
                "validation":"agrees_with_expression_operator", "raw_response":raw,
                "execution_expression":method_expression(&selected, expression)}),
            Ok((selected, raw)) => json!({"used":"large_model", "fallback":true,
                "reason":"fast choice deferred or disagreed with expression", "backend":backend,
                "selected":selected, "raw_response":raw}),
            Err(error) => {
                json!({"used":"large_model", "fallback":true,"backend":backend,"reason":error})
            }
        }
    } else {
        json!({"used":"large_model","fallback":true,"reason":"expression outside guarded single-operation subset"})
    };
    let mut outcome = outcome;
    outcome["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    outcome["fallback_semantics"] =
        json!("reuse main model supplied expression; no additional LLM call");
    outcome
}

// Construct the executor input from the accepted method and the original operands.
fn method_expression(selected: &str, expression: &str) -> String {
    let symbol = match selected {
        "add" => '+',
        "subtract" => '-',
        "multiply" => '*',
        "divide" => '/',
        _ => unreachable!("validated method"),
    };
    let compact: String = expression.chars().filter(|c| !c.is_whitespace()).collect();
    let index = compact
        .find(|c: char| "+-*/".contains(c))
        .expect("validated binary expression");
    format!("{}{}{}", &compact[..index], symbol, &compact[index + 1..])
}

fn simple_operator(expression: &str) -> Option<&'static str> {
    let expression: String = expression.chars().filter(|c| !c.is_whitespace()).collect();
    let matches: Vec<_> = expression
        .char_indices()
        .filter(|(_, c)| "+-*/".contains(*c))
        .collect();
    if matches.len() != 1 {
        return None;
    }
    let (index, op) = matches[0];
    let a: f64 = expression[..index].parse().ok()?;
    let b: f64 = expression[index + 1..].parse().ok()?;
    if !a.is_finite() || !b.is_finite() || (op == '/' && b == 0.0) {
        return None;
    }
    Some(match op {
        '+' => "add",
        '-' => "subtract",
        '*' => "multiply",
        '/' => "divide",
        _ => return None,
    })
}

async fn request_choice(
    backend: FastBackend,
    endpoint: &str,
    timeout_ms: u64,
    objective: &str,
    expression: &str,
) -> Result<(String, Value), String> {
    if timeout_ms == 0 {
        return Err("fast timeout must be positive".into());
    }
    let query = format!("{objective}\n待执行表达式： {expression}");
    let (path, body) = match backend {
        FastBackend::Laya => (
            "decide",
            json!({"goal":"为表达式选择一个算术操作；有歧义或包含多个操作时选择 defer。",
            "state":{"query":query}, "candidates":(0..4).map(|i| json!({"operation":IDS[i],"description":DESCRIPTIONS[i]})).collect::<Vec<_>>()}),
        ),
        FastBackend::Qwen => (
            "rerank",
            json!({"query":query,"instruction":"选择能完成该表达式的单个算术方法；没有合适的单个方法时选择 defer。",
            "documents":(0..5).map(|i| format!("{}: {}",IDS[i],DESCRIPTIONS[i])).collect::<Vec<_>>()}),
        ),
    };
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{}/{path}", endpoint.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("fast service request failed: {e}"))?
        .error_for_status()
        .map_err(|e| format!("fast service HTTP error: {e}"))?;
    let raw: Value = response
        .json()
        .await
        .map_err(|e| format!("invalid fast JSON: {e}"))?;
    let selected = decode(backend, &raw)?;
    Ok((selected, raw))
}

fn decode(backend: FastBackend, raw: &Value) -> Result<String, String> {
    match backend {
        FastBackend::Laya => {
            let probs = raw["probabilities"]
                .as_object()
                .ok_or("missing probabilities")?;
            if probs.len() != IDS.len() {
                return Err("wrong Laya candidate set".into());
            }
            let mut sum = 0.0;
            let mut best = -1.0_f64;
            for id in IDS {
                let p = probs
                    .get(id)
                    .and_then(Value::as_f64)
                    .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                    .ok_or("invalid Laya probability")?;
                sum += p;
                best = best.max(p);
            }
            let selected = raw["operation"].as_str().ok_or("missing operation")?;
            if !IDS.contains(&selected)
                || (sum - 1.0).abs() > 0.01
                || probs[selected].as_f64() != Some(best)
            {
                return Err("inconsistent Laya choice".into());
            }
            Ok(selected.into())
        }
        FastBackend::Qwen => {
            let rows = raw["results"].as_array().ok_or("missing rerank results")?;
            if rows.len() != IDS.len() {
                return Err("incomplete rerank results".into());
            }
            let mut seen = [false; 5];
            let mut best = (0, -1.0);
            for row in rows {
                let i = row["index"]
                    .as_u64()
                    .filter(|i| *i < 5)
                    .ok_or("invalid candidate index")? as usize;
                if seen[i] {
                    return Err("duplicate rerank candidate".into());
                }
                seen[i] = true;
                let score = row["score"]
                    .as_f64()
                    .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
                    .ok_or("invalid rerank score")?;
                if score > best.1 {
                    best = (i, score);
                }
            }
            Ok(IDS[best.0].into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_and_candidate_validation() {
        assert_eq!(ModelPreference::default(), ModelPreference::LargeModel);
        assert_eq!(simple_operator("10000/100"), Some("divide"));
        assert_eq!(simple_operator("1+2*3"), None);
        assert_eq!(simple_operator("1/0"), None);
        assert!(decode(
            FastBackend::Qwen,
            &json!({"results":[{"index":0,"score":0.99}]})
        )
        .is_err());
        assert!(decode(FastBackend::Laya,&json!({"operation":"multiply","probabilities":{"add":0.9,"subtract":0.0,"multiply":0.1,"divide":0.0,"defer":0.0}})).is_err());
    }
    #[tokio::test]
    async fn stalled_service_times_out_and_falls_back() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let preference = ModelPreference::PreferFast {
            backend: FastBackend::Laya,
            endpoint,
            timeout_ms: 50,
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            route_calculation(&preference, "加法", "1+2"),
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(result["fallback"], true);
        assert_eq!(result["used"], "large_model");
    }

    #[tokio::test]
    async fn http_accept_mismatch_and_unavailable_fallback() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (backend, body, expected_used) in [
            (
                FastBackend::Laya,
                json!({"operation":"divide","probabilities":{"add":0.0,"subtract":0.0,"multiply":0.0,"divide":1.0,"defer":0.0}}),
                "fast_model",
            ),
            (
                FastBackend::Laya,
                json!({"operation":"multiply","probabilities":{"add":0.0,"subtract":0.0,"multiply":1.0,"divide":0.0,"defer":0.0}}),
                "large_model",
            ),
            (
                FastBackend::Qwen,
                json!({"results":(0..5).map(|i|json!({"index":i,"score":if i==3 {0.8} else {0.1}})).collect::<Vec<_>>()}),
                "fast_model",
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0; 8192];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = body.to_string();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let preference = ModelPreference::PreferFast {
                backend,
                endpoint,
                timeout_ms: 1000,
            };
            assert_eq!(
                route_calculation(&preference, "一万除以一百", "10000/100").await["used"],
                expected_used
            );
            server.await.unwrap();
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let preference = ModelPreference::PreferFast {
            backend: FastBackend::Laya,
            endpoint,
            timeout_ms: 100,
        };
        assert_eq!(
            route_calculation(&preference, "加法", "1+2").await["fallback"],
            true
        );
        assert_eq!(
            route_calculation(&ModelPreference::LargeModel, "加法", "1+2").await["used"],
            "large_model"
        );
    }
}
