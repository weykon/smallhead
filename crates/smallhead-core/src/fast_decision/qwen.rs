use super::*;
use serde_json::json;

pub(super) async fn rerank(
    client: &DecisionClient,
    request: &QwenRequest,
) -> Result<Value, ToolError> {
    validate(request)?;
    let raw = client.post(&client.qwen_endpoint,"rerank",json!({"query":request.query,"instruction":request.instruction,"documents":request.candidates.iter().map(|c|&c.text).collect::<Vec<_>>()})).await?;
    decode(request, raw)
}
fn bounded(n: f64) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
}
pub(super) fn validate(request: &QwenRequest) -> Result<(), ToolError> {
    let mut ids = std::collections::HashSet::new();
    if !text(&request.query, 32768)
        || !text(&request.instruction, 2048)
        || !(1..=32).contains(&request.candidates.len())
        || !request
            .candidates
            .iter()
            .all(|c| text(&c.id, 128) && text(&c.text, 32768) && ids.insert(&c.id))
    {
        return Err(invalid(
            "Qwen 需要非空查询、指令和 1..32 个具有唯一 ID 的候选",
        ));
    }
    let valid = match request.policy {
        RankingPolicy::RankAll => true,
        RankingPolicy::TopK { k, min_score } => {
            k > 0 && k <= request.candidates.len() && min_score.is_none_or(bounded)
        }
        RankingPolicy::Filter { min_score } => bounded(min_score),
        RankingPolicy::SelectOne {
            min_score,
            min_margin,
        } => bounded(min_score) && bounded(min_margin),
    };
    if !valid {
        return Err(invalid("排序策略的数量或阈值不合法"));
    }
    Ok(())
}
pub(super) fn decode(request: &QwenRequest, raw: Value) -> Result<Value, ToolError> {
    let rows = raw["results"]
        .as_array()
        .ok_or_else(|| service_error("缺少 Qwen results"))?;
    if rows.len() != request.candidates.len() {
        return Err(service_error("Qwen 候选结果不完整"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut ranking = Vec::new();
    for row in rows {
        let i = row["index"]
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < request.candidates.len())
            .ok_or_else(|| service_error("候选索引非法"))?;
        if !seen.insert(i) {
            return Err(service_error("候选索引重复"));
        }
        ranking.push((i, probability(&row["score"])?));
    }
    ranking.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let ranked: Vec<Value> = ranking
        .iter()
        .map(|(i, s)| json!({"id":request.candidates[*i].id,"score":s,"index":i}))
        .collect();
    let mut reason = "policy_applied";
    let selected: Vec<Value> = match request.policy {
        RankingPolicy::RankAll => ranked.clone(),
        RankingPolicy::TopK { k, min_score } => ranked
            .iter()
            .filter(|r| min_score.is_none_or(|min| r["score"].as_f64().unwrap() >= min))
            .take(k)
            .cloned()
            .collect(),
        RankingPolicy::Filter { min_score } => ranked
            .iter()
            .filter(|r| r["score"].as_f64().unwrap() >= min_score)
            .cloned()
            .collect(),
        RankingPolicy::SelectOne {
            min_score,
            min_margin,
        } => {
            if ranking[0].1 < min_score {
                reason = "below_min_score";
                vec![]
            } else if ranking.len() > 1
                && (ranking[0].1 == ranking[1].1 || ranking[0].1 - ranking[1].1 < min_margin)
            {
                reason = "ambiguous_margin";
                vec![]
            } else {
                vec![ranked[0].clone()]
            }
        }
    };
    Ok(
        json!({"backend":"qwen","status":if selected.is_empty(){"defer"}else{"advisory"},"ranking":ranked,"selected":selected,"policy":request.policy,"reason":reason,
        "score_type":"independent_yes_no_relevance","scores_calibrated":false,"executed":false,"raw_response":raw}),
    )
}
