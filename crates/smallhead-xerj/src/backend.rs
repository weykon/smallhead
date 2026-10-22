use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use smallhead_core::{execution::ExecutionRuntime, ToolContext};
use smallhead_process::{
    service::ServiceSpec, ExecutionMetadata, ExitReason, OutputStream, ProcessSpec, ProcessState,
};

/// Host-owned configuration. The model cannot choose a server address or binary.
#[derive(Clone, Debug)]
pub struct XerjConfig {
    /// Executable path; defaults to ~/.local/bin/xerj.
    pub binary: PathBuf,
    /// Dedicated data root outside indexed projects.
    pub data_dir: PathBuf,
    /// ES-compatible listener. Native and gRPC listeners use the next two ports.
    pub port: u16,
}

impl XerjConfig {
    /// Read SMALLHEAD_XERJ_BIN / DATA_DIR / PORT; no processes start here.
    pub fn from_env() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is required for XERJ defaults")?;
        let config = Self {
            binary: std::env::var_os("SMALLHEAD_XERJ_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/bin/xerj")),
            data_dir: std::env::var_os("SMALLHEAD_XERJ_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".smallhead/xerj")),
            port: std::env::var("SMALLHEAD_XERJ_PORT")
                .unwrap_or_else(|_| "19320".into())
                .parse()
                .map_err(|_| "invalid SMALLHEAD_XERJ_PORT")?,
        };
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !self.binary.is_absolute()
            || !self.data_dir.is_absolute()
            || !(1024..=65533).contains(&self.port)
        {
            return Err(
                "XERJ requires absolute binary/data paths and a port in 1024..65533".into(),
            );
        }
        Ok(())
    }

    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub(crate) fn service_spec(&self) -> ServiceSpec {
        let mut process = ProcessSpec::program(
            self.binary.to_string_lossy(),
            vec![
                "--insecure".into(),
                "--bind".into(),
                "127.0.0.1".into(),
                "--port".into(),
                self.port.to_string(),
                "--data-dir".into(),
                self.data_dir.join("node").to_string_lossy().into_owned(),
                "--embed-mode".into(),
                "lexical".into(),
            ],
        );
        process.cwd = Some(self.data_dir.clone());
        process.description =
            Some("XERJ local project retrieval; lexical embedding; no external reranking".into());
        ServiceSpec {
            name: "xerj".into(),
            process,
            health_url: format!("{}/", self.url()),
            identity: BTreeMap::from([("cluster_name".into(), json!("xerj"))]),
            startup_timeout_ms: 30_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "operation", rename_all = "snake_case")]
pub(crate) enum Request {
    Ensure,
    Index {
        #[serde(default)]
        project_path: Option<PathBuf>,
        #[serde(default)]
        approval: Option<Approval>,
    },
    Map {
        #[serde(default)]
        project_path: Option<PathBuf>,
    },
    Search {
        #[serde(default)]
        project_path: Option<PathBuf>,
        query: String,
    },
    Poll {
        execution_id: smallhead_process::ExecutionId,
        #[serde(default)]
        offset: u64,
        #[serde(default)]
        stderr: bool,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Approval {
    Proceed,
    Fast,
}

pub(crate) fn project_scope(path: Option<&Path>, cwd: &Path) -> Result<(PathBuf, String), String> {
    let path = path.unwrap_or(cwd);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let root = path
        .canonicalize()
        .map_err(|e| format!("project path unavailable: {e}"))?;
    if !root.is_dir() {
        return Err("project_path must be a directory".into());
    }
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    let key: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    Ok((root, format!("sh-{key}")))
}

pub(crate) fn runtime(ctx: &ToolContext) -> Result<Arc<ExecutionRuntime>, String> {
    ctx.execution
        .clone()
        .ok_or_else(|| "project_research requires the host execution manager".into())
}

async fn http(
    config: &XerjConfig,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.request(method, format!("{}{}", config.url(), path));
    if let Some(body) = body {
        request = request.json(&body);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| format!("XERJ request failed; use ensure to start the node: {e}"))?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err("XERJ response exceeds 2 MiB; narrow the corpus/query".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("XERJ returned non-JSON (HTTP {status})"))?;
    if !status.is_success() {
        return Err(format!("XERJ HTTP {status}: {value}"));
    }
    Ok(value)
}

fn search_body(mappings: &Value, query: &str) -> Result<Value, String> {
    let mut fields = std::collections::BTreeSet::new();
    for mapping in mappings
        .as_object()
        .ok_or("invalid XERJ mappings")?
        .values()
    {
        if let Some(properties) = mapping["mappings"]["properties"].as_object() {
            for (name, definition) in properties {
                if !name.starts_with("ax_")
                    && matches!(
                        definition["type"].as_str(),
                        Some("text" | "semantic_text" | "keyword")
                    )
                {
                    fields.insert(name.clone());
                }
            }
        }
    }
    if fields.is_empty() {
        return Err("No searchable project fields. Index the project, poll its result, and inspect map before searching.".into());
    }
    if fields.len() > 64 {
        return Err("Project has more than 64 text fields; narrow the indexed directory.".into());
    }
    let mut should = vec![json!({"multi_match":{"query":query,"fields":fields}})];
    if fields.contains("defs") {
        should.push(json!({"match_phrase":{"defs":{"query":query,"boost":4}}}));
    }
    let mut source = vec![
        "title".to_owned(),
        "ax_path".into(),
        "ax_paths".into(),
        "language".into(),
        "symbols".into(),
    ];
    source.extend(
        fields
            .iter()
            .filter(|field| !source_metadata(field))
            .cloned(),
    );
    Ok(
        json!({"size":5,"query":{"bool":{"should":should,"minimum_should_match":1}},
        "_source":source,"fields":["_passage"]}),
    )
}

fn source_metadata(field: &str) -> bool {
    matches!(
        field,
        "title" | "ax_path" | "ax_paths" | "language" | "symbols"
    )
}

// Lexical hits do not always carry XERJ's semantic _passage field. Bound raw
// source text before it enters the agent context and label our excerpts separately.
pub(crate) fn project_hits(response: &mut Value, query: &str) {
    let Some(hits) = response["hits"]["hits"].as_array_mut() else {
        return;
    };
    hits.truncate(5);
    for hit in hits.iter_mut().take(5) {
        let Some(source) = hit.get_mut("_source").and_then(Value::as_object_mut) else {
            continue;
        };
        let mut excerpts = Vec::new();
        for (field, value) in source
            .iter()
            .filter(|(field, _)| !source_metadata(field))
            .filter(|(_, value)| value.is_string())
            .take(3)
        {
            let text = value.as_str().unwrap_or_default();
            let match_byte = text
                .find(query)
                .or_else(|| query.split_whitespace().find_map(|word| text.find(word)))
                .unwrap_or(0);
            let start = text[..match_byte].chars().count().saturating_sub(200);
            let excerpt: String = text.chars().skip(start).take(1200).collect();
            let end = start + excerpt.chars().count();
            excerpts.push(json!({"field":field,"text":excerpt,"start_char":start,"end_char":end,"truncated":start > 0 || text.chars().count() > end}));
        }
        source.retain(|field, _| source_metadata(field));
        if !excerpts.is_empty() {
            hit["source_excerpts"] = json!(excerpts);
        }
    }
}

async fn verify_node(config: &XerjConfig) -> Result<(), String> {
    let identity = http(config, Method::GET, "/", None).await?;
    if identity["cluster_name"] != "xerj" {
        return Err(
            "Configured port does not identify as XERJ; refusing to query another service".into(),
        );
    }
    Ok(())
}

pub(crate) async fn run(
    config: &XerjConfig,
    ctx: &ToolContext,
    request: Request,
) -> Result<Value, String> {
    config.validate()?;
    let rt = runtime(ctx)?;
    match request {
        Request::Ensure => {
            std::fs::create_dir_all(&config.data_dir).map_err(|e| e.to_string())?;
            let (record, operation) = rt
                .services
                .ensure(config.service_spec())
                .await
                .map_err(|e| e.to_string())?;
            if record.description.is_none() {
                rt.services.update_description("xerj", "项目代码和文档检索后端。按项目隔离索引，默认词法模式；索引数据保存在本机。由项目研究工具按需启动，可在服务页查看状态和停止。".into()).map_err(|e| e.to_string())?;
            }
            Ok(
                json!({"service":record,"operation_id":operation,"embedding_mode":"lexical","node_url":config.url()}),
            )
        }
        Request::Index {
            project_path,
            approval,
        } => {
            let (root, prefix) = project_scope(project_path.as_deref(), &ctx.cwd)?;
            verify_node(config).await?;
            std::fs::create_dir_all(&config.data_dir).map_err(|e| e.to_string())?;
            let data = config.data_dir.canonicalize().map_err(|e| e.to_string())?;
            if data.starts_with(&root) || root.starts_with(&data) {
                return Err(
                    "XERJ data directory and indexed project must not contain each other".into(),
                );
            }
            // Serialize submission checks across simultaneous tool invocations.
            static SUBMISSION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
            let _guard = SUBMISSION.lock().await;
            let objective = format!("XERJ index {}", root.display());
            if let Some(record) = rt
                .processes
                .list()
                .into_iter()
                .filter_map(|id| rt.processes.inspect(id))
                .find(|r| {
                    !r.is_terminal()
                        && r.metadata.source.as_deref() == Some("xerj_index")
                        && r.metadata.objective.as_deref() == Some(&objective)
                })
            {
                return Ok(
                    json!({"phase":"running","already_running":true,"execution_id":record.id,"project_path":root,"index_pattern":format!("{prefix}-*")}),
                );
            }
            let mut args = vec![
                "autoindex".into(),
                root.to_string_lossy().into_owned(),
                "--url".into(),
                config.url(),
                "--prefix".into(),
                prefix.clone(),
                "--state-dir".into(),
                data.join("state")
                    .join(&prefix)
                    .to_string_lossy()
                    .into_owned(),
                "--workers".into(),
                "2".into(),
                "--no-graph".into(),
                "--max-minutes".into(),
                "10".into(),
                "--json".into(),
                "--progress".into(),
                "json".into(),
            ];
            if let Some(approval) = approval {
                args.extend([
                    "--approve".into(),
                    match approval {
                        Approval::Proceed => "proceed",
                        Approval::Fast => "fast",
                    }
                    .into(),
                ]);
            }
            let mut spec = ProcessSpec::program(config.binary.to_string_lossy(), args);
            spec.cwd = Some(root.clone());
            spec.timeout = Some(Duration::from_secs(600).min(rt.processes.limits().max_timeout));
            spec.description = Some(objective.clone());
            let id = rt
                .processes
                .start_with_metadata(
                    spec,
                    ctx.owner.clone(),
                    ExecutionMetadata {
                        source: Some("xerj_index".into()),
                        objective: Some(objective),
                        tool_call_id: ctx.tool_call_id.clone(),
                        completion_policy: Some(
                            json!({"allowed_exit_codes":[0,3],"decision_exit_code":4}),
                        ),
                    },
                )
                .await
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"phase":"running","execution_id":id,"project_path":root,"index_pattern":format!("{prefix}-*"),
                "next":"Poll this execution until terminal. Submission is not indexing success; inspect stdout and stderr. Exit 3 can include refused datasets; exit 4 needs a user decision."}),
            )
        }
        Request::Poll {
            execution_id,
            offset,
            stderr,
        } => {
            let record = rt
                .processes
                .inspect(execution_id)
                .ok_or("unknown execution_id")?;
            if record.metadata.source.as_deref() != Some("xerj_index") {
                return Err("execution_id is not an XERJ index job".into());
            }
            let phase = match &record.state {
                ProcessState::Exited(info) if info.reason == ExitReason::Natural => {
                    match info.exit_code {
                        Some(0) => "completed",
                        Some(3) => "completed_with_junk",
                        Some(4) => "needs_decision",
                        _ => "failed",
                    }
                }
                state if state.is_terminal() => "failed",
                _ => "running",
            };
            let output = rt
                .processes
                .read_output(
                    execution_id,
                    if stderr {
                        OutputStream::Stderr
                    } else {
                        OutputStream::Stdout
                    },
                    offset,
                    12_000,
                )
                .await
                .map_err(|e| e.to_string())?;
            let stderr_head = rt
                .processes
                .read_output(execution_id, OutputStream::Stderr, 0, 12_000)
                .await
                .map_err(|e| e.to_string())?;
            let stderr_tail = if stderr_head.stored_bytes > 12_000 {
                rt.processes
                    .read_output(
                        execution_id,
                        OutputStream::Stderr,
                        stderr_head.stored_bytes.saturating_sub(12_000),
                        12_000,
                    )
                    .await
                    .map_err(|e| e.to_string())?
            } else {
                stderr_head
            };
            let blocker = stderr_tail.text.lines().rev().find(|line| !record.is_terminal() && line.contains("read_only_allow_delete") && line.contains("flood-stage"))
                .map(|line| json!({"kind":"disk_flood_stage","message":line.chars().take(2048).collect::<String>(),"next":"If this job is still running, stop it with process_control and report the disk watermark. Free space or choose another data volume; do not silently change server protection settings."}));
            Ok(
                json!({"phase":phase,"execution_id":execution_id,"record":record,"output":output,"blocker":blocker,
                "note":"Output is paginated; use next_offset and also read stderr. completed_with_junk does not guarantee every dataset is searchable."}),
            )
        }
        Request::Map { project_path } => {
            let (root, prefix) = project_scope(project_path.as_deref(), &ctx.cwd)?;
            verify_node(config).await?;
            let mappings =
                http(config, Method::GET, &format!("/{prefix}-*/_mapping"), None).await?;
            Ok(
                json!({"project_path":root,"index_pattern":format!("{prefix}-*"),"mappings":mappings}),
            )
        }
        Request::Search {
            project_path,
            query,
        } => {
            if query.trim().is_empty() || query.len() > 4096 {
                return Err("query must contain 1..4096 bytes".into());
            }
            let (root, prefix) = project_scope(project_path.as_deref(), &ctx.cwd)?;
            verify_node(config).await?;
            let mappings =
                http(config, Method::GET, &format!("/{prefix}-*/_mapping"), None).await?;
            let mut response = http(
                config,
                Method::POST,
                &format!("/{prefix}-*/_search"),
                Some(search_body(&mappings, &query)?),
            )
            .await?;
            project_hits(&mut response, &query);
            Ok(
                json!({"project_path":root,"index_pattern":format!("{prefix}-*"),"search":response,
                "note":"Cite project_path + ax_path and actual symbol line or passage offsets. source_excerpts are host-bounded slices of the indexed field, with Unicode character offsets in that field, not file line numbers. Scores are relevance ranks, not confidence. Empty hits are not evidence of absence; refresh the index or use source tools."}),
            )
        }
    }
}
