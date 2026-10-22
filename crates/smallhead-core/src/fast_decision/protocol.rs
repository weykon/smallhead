use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Laya 的原生问题类型，不把不同语义统一压成单选。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LayaQuestion {
    /// 从合法标签中选择一项。
    Choice {
        /// 问题描述。
        instructions: String,
        /// 稳定标签与含义，2..32 项。
        criteria: BTreeMap<String, String>,
    },
    /// 根据有序等级给出评分，输出可以是小数。
    Score {
        /// 问题描述。
        instructions: String,
        /// 从低到高排列的等级，2..8 项。
        criteria: Vec<String>,
    },
    /// 判断一个语义命题，返回原生 0..1 值；不是系统事实证明。
    Noul {
        /// 要判断的命题。
        instructions: String,
        /// 可选的 false/true 含义说明。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<BTreeMap<String, String>>,
    },
}
/// 一次请求包含多个独立问题，共享局部观察。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayaRequest {
    /// 精简后的观察对象，最多 8192 字节。
    pub state: serde_json::Map<String, Value>,
    /// 问题稳定 ID 到问题定义，1..16 项。
    pub questions: BTreeMap<String, LayaQuestion>,
}
/// Qwen 待排序的工具、记忆或数据候选。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// 调用方分配的稳定 ID，结果仍保留此 ID。
    pub id: String,
    /// 供模型评分的内容。
    pub text: String,
}
/// 应用策略；阈值必须针对业务校准，不能视为正确率。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RankingPolicy {
    /// 返回完整排序，不自动执行。
    #[default]
    RankAll,
    /// 返回前 K 项，可附加最低分要求。
    TopK {
        /// 保留数量。
        k: usize,
        /// 可选的最低相关性分数。
        #[serde(default)]
        min_score: Option<f64>,
    },
    /// 保留所有满足显式阈值的候选，可以没有任何候选。
    Filter {
        /// 最低相关性分数。
        min_score: f64,
    },
    /// 最多接受一项；低分或第一二名接近则 defer。
    SelectOne {
        /// 最低相关性分数。
        min_score: f64,
        /// 第一名相对第二名的最低差值。
        min_margin: f64,
    },
}
/// Qwen 相关性评分请求，不是聊天生成请求。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QwenRequest {
    /// 查询或任务局部问题。
    pub query: String,
    /// 本业务的评分指令。
    pub instruction: String,
    /// 候选，1..32 项。
    pub candidates: Vec<Candidate>,
    /// 缺省返回完整排序。
    #[serde(default)]
    pub policy: RankingPolicy,
}
