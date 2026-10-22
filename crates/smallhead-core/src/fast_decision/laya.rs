use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};

pub(super) async fn evaluate(
    client: &DecisionClient,
    request: &LayaRequest,
) -> Result<Value, ToolError> {
    validate(request)?;
    let spec_id = format!("decision_{}", uuid::Uuid::now_v7().simple());
    let spec_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&request.questions).map_err(|e| invalid(e.to_string()))?)
    );
    let raw = client.post(&client.laya_endpoint, "evaluate", json!({"spec_version":2,"spec_id":spec_id,"spec_hash":spec_hash,"state":request.state,"questions":request.questions})).await?;
    if raw["protocol_version"] != 2 || raw["spec_id"] != spec_id || raw["spec_hash"] != spec_hash {
        return Err(service_error("Laya 协议版本或问题身份不匹配"));
    }
    let answers = decode(request, &raw)?;
    Ok(
        json!({"backend":"laya","status":"advisory","answers":answers,"raw_response":raw,"executed":false}),
    )
}
fn validate(request: &LayaRequest) -> Result<(), ToolError> {
    if !(1..=16).contains(&request.questions.len())
        || serde_json::to_vec(&request.state)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > 8192
    {
        return Err(invalid("Laya 需要 1..16 个问题，state 不超过 8192 字节"));
    }
    for (id, q) in &request.questions {
        if !identifier(id, 80) {
            return Err(invalid("问题 ID 必须是短 ASCII 标识符"));
        }
        let (instructions, valid) = match q {
            LayaQuestion::Choice {
                instructions,
                criteria,
            } => (
                instructions,
                (2..=32).contains(&criteria.len())
                    && criteria
                        .iter()
                        .all(|(id, s)| identifier(id, 80) && s.chars().count() <= 256),
            ),
            LayaQuestion::Score {
                instructions,
                criteria,
            } => (
                instructions,
                (2..=8).contains(&criteria.len()) && criteria.iter().all(|s| text(s, 256)),
            ),
            LayaQuestion::Noul {
                instructions,
                criteria,
            } => (
                instructions,
                criteria.as_ref().is_none_or(|c| {
                    c.len() == 2
                        && c.contains_key("false")
                        && c.contains_key("true")
                        && c.values().all(|s| text(s, 256))
                }),
            ),
        };
        if !text(instructions, 512) || !valid {
            return Err(invalid(format!("问题 {id} 的指令或 criteria 不合法")));
        }
    }
    Ok(())
}
pub(super) fn decode(request: &LayaRequest, raw: &Value) -> Result<Value, ToolError> {
    let answers = raw["answers"]
        .as_object()
        .ok_or_else(|| service_error("缺少 answers"))?;
    if answers.len() != request.questions.len() {
        return Err(service_error("答案集合不匹配"));
    }
    let mut normalized = serde_json::Map::new();
    for (id, q) in &request.questions {
        let answer = answers
            .get(id)
            .ok_or_else(|| service_error("缺少问题答案"))?;
        let confidence = probability(&answer["confidence"])?;
        let kind = match q {
            LayaQuestion::Choice { .. } => "choice",
            LayaQuestion::Score { .. } => "score",
            LayaQuestion::Noul { .. } => "noul",
        };
        if answer.get("type").is_some_and(|t| t != kind) {
            return Err(service_error("答案类型不匹配"));
        }
        let mut output = json!({"type":kind,"confidence":confidence,"confidence_calibrated":false});
        if let LayaQuestion::Noul { .. } = q {
            output["value"] = json!(probability(&answer["noul"])?);
        } else {
            let labels: Vec<String> = match q {
                LayaQuestion::Choice { criteria, .. } => criteria.keys().cloned().collect(),
                LayaQuestion::Score { criteria, .. } => {
                    (0..criteria.len()).map(|i| i.to_string()).collect()
                }
                _ => unreachable!(),
            };
            let probs = answer["probabilities"]
                .as_object()
                .ok_or_else(|| service_error("缺少 probabilities"))?;
            if probs.len() != labels.len() {
                return Err(service_error("概率候选集合不匹配"));
            }
            let scores: Vec<f64> = labels
                .iter()
                .map(|id| probability(&probs[id]))
                .collect::<Result<_, _>>()?;
            if (scores.iter().sum::<f64>() - 1.0).abs() > 0.01 {
                return Err(service_error("概率总和不为 1"));
            }
            output["probabilities"] = json!(probs);
            match q {
                LayaQuestion::Choice { .. } => {
                    let choice = answer["choice"]
                        .as_str()
                        .filter(|s| labels.iter().any(|id| id == s))
                        .ok_or_else(|| service_error("未知 choice"))?;
                    let best = scores.iter().copied().fold(0.0, f64::max);
                    if probability(&probs[choice])? != best {
                        return Err(service_error("choice 与最大概率不一致"));
                    }
                    output["selected"] = json!(choice);
                }
                LayaQuestion::Score { criteria, .. } => {
                    let score = answer["score"]
                        .as_f64()
                        .filter(|s| s.is_finite() && *s >= 0.0 && *s <= (criteria.len() - 1) as f64)
                        .ok_or_else(|| service_error("score 超出等级范围"))?;
                    output["score"] = json!(score);
                    output["levels"] = json!(criteria);
                }
                _ => unreachable!(),
            }
        }
        normalized.insert(id.clone(), output);
    }
    Ok(json!(normalized))
}
