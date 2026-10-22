//! Artifact storage and bounded projections for oversized tool results.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    message::{AgentMessage, ContentBlock},
    tool::{ToolResult, ToolResultProjection},
};

pub(crate) const TOOL_RESULT_PROJECTION_THRESHOLD_BYTES: usize = 16 * 1024;
pub(crate) const TOOL_RESULT_PROJECTION_VISIBLE_BYTES: usize = 12 * 1024;
pub(crate) const ARTIFACT_READ_DEFAULT_LIMIT: usize = 12 * 1024;
pub(crate) const ARTIFACT_READ_MAX_LIMIT: usize = 12 * 1024;

const TOOL_RESULT_URI_PREFIX: &str = "artifact://tool_result/sha256:";
const HISTORY_URI_PREFIX: &str = "artifact://history/sha256:";

#[derive(Debug, Clone)]
pub(crate) struct ProjectionOutcome {
    pub result: ToolResult,
    pub projection: Option<ToolResultProjection>,
}

#[derive(Debug, Clone)]
struct StoredArtifact {
    artifact_ref: String,
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Clone)]
pub(crate) struct HistoryCompactionArtifact {
    pub artifact_ref: String,
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: usize,
}

pub(crate) fn project_tool_result(
    tool_name: &str,
    result: ToolResult,
    artifact_root: Option<&Path>,
) -> ProjectionOutcome {
    let original_text = tool_result_text(&result);
    let original_bytes = original_text.len();
    if original_bytes <= TOOL_RESULT_PROJECTION_THRESHOLD_BYTES {
        return ProjectionOutcome {
            result,
            projection: None,
        };
    }

    let storage = store_tool_result_text(
        artifact_root.unwrap_or_else(|| Path::new("")),
        tool_name,
        &result.tool_call_id,
        result.is_error,
        &original_text,
        artifact_root.is_some(),
    );
    let (artifact_ref, artifact_path, sha256, storage_error) = match storage {
        Ok(stored) => (
            Some(stored.artifact_ref),
            Some(stored.path),
            Some(stored.sha256),
            None,
        ),
        Err(error) => (None, None, None, Some(error)),
    };

    let preview = head_tail_bytes(&original_text, TOOL_RESULT_PROJECTION_VISIBLE_BYTES);
    let visible_bytes = preview.len();
    let envelope = json!({
        "tool_result_projection": {
            "schema_version": 1,
            "tool": tool_name,
            "tool_call_id": &result.tool_call_id,
            "is_error": result.is_error,
            "truncated": true,
            "strategy": "head_tail",
            "original_bytes": original_bytes,
            "visible_bytes": visible_bytes,
            "artifact_ref": artifact_ref,
            "sha256": sha256,
            "storage_error": storage_error,
            "preview": preview,
            "read_more": artifact_ref.as_ref().map(|artifact_ref| json!({
                "tool": "artifact_read",
                "args": {
                    "ref": artifact_ref,
                    "offset": 0,
                    "limit": ARTIFACT_READ_DEFAULT_LIMIT
                },
                "note": "Preview is incomplete. Call artifact_read to inspect omitted output."
            }))
        }
    });
    let envelope_text = serde_json::to_string(&envelope).unwrap_or_else(|_| {
        "{\"tool_result_projection\":{\"truncated\":true,\"storage_error\":\"serialization failed\"}}"
            .to_string()
    });
    let projected = ToolResult {
        tool_call_id: result.tool_call_id.clone(),
        is_error: result.is_error,
        content: vec![ContentBlock::Text {
            text: envelope_text.clone(),
        }],
    };
    let projection = ToolResultProjection {
        projected: true,
        original_bytes,
        visible_bytes: envelope_text.len(),
        strategy: "head_tail".into(),
        artifact_ref,
        artifact_path,
        storage_error,
    };

    ProjectionOutcome {
        result: projected,
        projection: Some(projection),
    }
}

pub(crate) fn read_artifact_page(
    artifact_ref: &str,
    artifact_root: Option<&Path>,
    offset: usize,
    limit: usize,
    expected_sha256: Option<&str>,
) -> Result<serde_json::Value, String> {
    let parsed = parse_artifact_ref(artifact_ref)?;
    if let Some(expected) = expected_sha256 {
        if expected.to_ascii_lowercase() != parsed.sha256 {
            return Err(format!(
                "expected_sha256 mismatch: expected {expected}, artifact ref contains {}",
                parsed.sha256
            ));
        }
    }
    let root = artifact_root
        .map(PathBuf::from)
        .unwrap_or_else(default_artifact_root);
    let path = artifact_path_for_kind(&root, parsed.kind, &parsed.sha256);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read artifact failed for {artifact_ref}: {e}"))?;
    let actual_sha256 = sha256_hex(text.as_bytes());
    if actual_sha256 != parsed.sha256 {
        return Err(format!(
            "artifact hash mismatch for {artifact_ref}: expected {}, got {actual_sha256}",
            parsed.sha256
        ));
    }
    let total_bytes = text.len();
    let start = align_forward_char_boundary(&text, offset.min(total_bytes));
    let limit = limit.clamp(1, ARTIFACT_READ_MAX_LIMIT);
    let mut end = start.saturating_add(limit).min(total_bytes);
    end = align_backward_char_boundary(&text, end);
    if end < start {
        end = start;
    }
    let output = text[start..end].to_string();
    Ok(json!({
        "ref": artifact_ref,
        "kind": parsed.kind.label(),
        "sha256": parsed.sha256,
        "verified": true,
        "offset": start,
        "requested_offset": offset,
        "limit": limit,
        "output": output,
        "output_bytes": end.saturating_sub(start),
        "total_bytes": total_bytes,
        "next_offset": if end < total_bytes { Some(end) } else { None },
        "has_more": end < total_bytes,
        "offset_kind": "utf8_byte"
    }))
}

pub(crate) fn store_history_compaction(
    messages: &[AgentMessage],
    artifact_root: Option<&Path>,
) -> Result<HistoryCompactionArtifact, String> {
    let root = artifact_root
        .map(PathBuf::from)
        .unwrap_or_else(default_artifact_root);
    let text = serde_json::to_string_pretty(messages)
        .map_err(|e| format!("serialize compacted history failed: {e}"))?;
    let sha256 = sha256_hex(text.as_bytes());
    let path = artifact_path_for_kind(&root, ArtifactKind::History, &sha256);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create artifact dir failed: {e}"))?;
    }
    if !path.exists() {
        std::fs::write(&path, &text).map_err(|e| format!("write artifact failed: {e}"))?;
    }
    let metadata_path = history_metadata_path_for_hash(&root, &sha256);
    if !metadata_path.exists() {
        let metadata = json!({
            "schema_version": 1,
            "kind": ArtifactKind::History.label(),
            "sha256": sha256,
            "bytes": text.len(),
            "message_count": messages.len(),
            "created_at_ms": now_millis()
        });
        let metadata_text = serde_json::to_string_pretty(&metadata).map_err(|e| e.to_string())?;
        std::fs::write(&metadata_path, metadata_text)
            .map_err(|e| format!("write artifact metadata failed: {e}"))?;
    }
    Ok(HistoryCompactionArtifact {
        artifact_ref: format!("{HISTORY_URI_PREFIX}{sha256}"),
        path,
        sha256,
        bytes: text.len(),
    })
}

pub(crate) fn tool_result_text(result: &ToolResult) -> String {
    let mut text_parts = Vec::new();
    let mut all_text = true;
    for block in &result.content {
        match block {
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            _ => all_text = false,
        }
    }
    if all_text {
        return text_parts.join("\n");
    }
    serde_json::to_string(&result.content).unwrap_or_else(|_| text_parts.join("\n"))
}

pub(crate) fn default_artifact_root() -> PathBuf {
    if let Ok(path) = std::env::var("SMALLHEAD_ARTIFACT_ROOT") {
        return PathBuf::from(path);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".smallhead").join("artifacts");
    }
    PathBuf::from(".smallhead").join("artifacts")
}

fn store_tool_result_text(
    root: &Path,
    tool_name: &str,
    tool_call_id: &str,
    is_error: bool,
    text: &str,
    root_is_override: bool,
) -> Result<StoredArtifact, String> {
    let root = if root_is_override {
        root.to_path_buf()
    } else {
        default_artifact_root()
    };
    let sha256 = sha256_hex(text.as_bytes());
    let path = artifact_path_for_hash(&root, &sha256);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create artifact dir failed: {e}"))?;
    }
    if !path.exists() {
        std::fs::write(&path, text).map_err(|e| format!("write artifact failed: {e}"))?;
    }
    let metadata_path = path.with_extension("json");
    if !metadata_path.exists() {
        let metadata = json!({
            "schema_version": 1,
            "kind": "tool_result",
            "tool": tool_name,
            "tool_call_id": tool_call_id,
            "is_error": is_error,
            "sha256": sha256,
            "bytes": text.len(),
            "created_at_ms": now_millis()
        });
        let metadata_text = serde_json::to_string_pretty(&metadata).map_err(|e| e.to_string())?;
        std::fs::write(&metadata_path, metadata_text)
            .map_err(|e| format!("write artifact metadata failed: {e}"))?;
    }
    Ok(StoredArtifact {
        artifact_ref: format!("{TOOL_RESULT_URI_PREFIX}{sha256}"),
        path,
        sha256,
    })
}

fn artifact_path_for_hash(root: &Path, sha256: &str) -> PathBuf {
    artifact_path_for_kind(root, ArtifactKind::ToolResult, sha256)
}

#[derive(Debug, Clone, Copy)]
enum ArtifactKind {
    ToolResult,
    History,
}

impl ArtifactKind {
    fn label(self) -> &'static str {
        match self {
            Self::ToolResult => "tool_result",
            Self::History => "history",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::ToolResult => "txt",
            Self::History => "json",
        }
    }
}

#[derive(Debug, Clone)]
struct ParsedArtifactRef {
    kind: ArtifactKind,
    sha256: String,
}

fn artifact_path_for_kind(root: &Path, kind: ArtifactKind, sha256: &str) -> PathBuf {
    root.join(kind.label())
        .join("sha256")
        .join(format!("{}.{}", sha256, kind.extension()))
}

fn history_metadata_path_for_hash(root: &Path, sha256: &str) -> PathBuf {
    root.join(ArtifactKind::History.label())
        .join("sha256")
        .join(format!("{sha256}.meta.json"))
}

fn parse_artifact_ref(artifact_ref: &str) -> Result<ParsedArtifactRef, String> {
    let (kind, sha256) = if let Some(sha256) = artifact_ref.strip_prefix(TOOL_RESULT_URI_PREFIX) {
        (ArtifactKind::ToolResult, sha256)
    } else if let Some(sha256) = artifact_ref.strip_prefix(HISTORY_URI_PREFIX) {
        (ArtifactKind::History, sha256)
    } else {
        return Err(
            "artifact ref must look like artifact://tool_result/sha256:<hash> or artifact://history/sha256:<hash>"
                .into(),
        );
    };
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("artifact ref contains an invalid sha256 hash".into());
    }
    Ok(ParsedArtifactRef {
        kind,
        sha256: sha256.to_ascii_lowercase(),
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn head_tail_bytes(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let marker = format!(
        "\n... [omitted {} bytes from middle; use artifact_read for full output] ...\n",
        text.len().saturating_sub(budget)
    );
    if budget <= marker.len() + 2 {
        let end = align_backward_char_boundary(text, budget.min(text.len()));
        return text[..end].to_string();
    }
    let content_budget = budget - marker.len();
    let head_budget = content_budget * 2 / 3;
    let tail_budget = content_budget - head_budget;
    let head_end = align_backward_char_boundary(text, head_budget.min(text.len()));
    let tail_start = align_forward_char_boundary(text, text.len().saturating_sub(tail_budget));
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

fn align_backward_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn align_forward_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_tool_result_is_not_projected() {
        let result = ToolResult {
            tool_call_id: "call-1".into(),
            is_error: false,
            content: vec![ContentBlock::Text { text: "ok".into() }],
        };
        let outcome = project_tool_result("echo", result, None);
        assert!(outcome.projection.is_none());
        assert_eq!(tool_result_text(&outcome.result), "ok");
    }

    #[test]
    fn large_tool_result_is_projected_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let body = format!("head-{}-tail", "x".repeat(40_000));
        let result = ToolResult {
            tool_call_id: "call-large".into(),
            is_error: false,
            content: vec![ContentBlock::Text { text: body.clone() }],
        };

        let outcome = project_tool_result("echo", result, Some(dir.path()));
        let projection = outcome
            .projection
            .expect("large result should be projected");
        let artifact_ref = projection.artifact_ref.expect("artifact ref should exist");
        assert!(projection.original_bytes > TOOL_RESULT_PROJECTION_THRESHOLD_BYTES);
        assert!(tool_result_text(&outcome.result).len() < body.len());

        let mut restored = String::new();
        let mut offset = 0usize;
        loop {
            let page =
                read_artifact_page(&artifact_ref, Some(dir.path()), offset, 64_000, None).unwrap();
            restored.push_str(page["output"].as_str().unwrap());
            if !page["has_more"].as_bool().unwrap() {
                break;
            }
            offset = page["next_offset"].as_u64().unwrap() as usize;
        }
        assert_eq!(restored, body);
    }

    #[test]
    fn artifact_read_rejects_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let body = "x".repeat(40_000);
        let result = ToolResult {
            tool_call_id: "call-corrupt".into(),
            is_error: false,
            content: vec![ContentBlock::Text { text: body }],
        };

        let outcome = project_tool_result("echo", result, Some(dir.path()));
        let projection = outcome.projection.unwrap();
        let artifact_ref = projection.artifact_ref.unwrap();
        let artifact_path = projection.artifact_path.unwrap();
        std::fs::write(artifact_path, "corrupted").unwrap();

        let error = read_artifact_page(&artifact_ref, Some(dir.path()), 0, 12000, None)
            .expect_err("corrupted artifact must not be readable");
        assert!(error.contains("artifact hash mismatch"));
    }

    #[test]
    fn artifact_read_respects_utf8_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let body = "你好abc".repeat(10_000);
        let result = ToolResult {
            tool_call_id: "call-utf8".into(),
            is_error: false,
            content: vec![ContentBlock::Text { text: body }],
        };
        let outcome = project_tool_result("echo", result, Some(dir.path()));
        let artifact_ref = outcome.projection.unwrap().artifact_ref.unwrap();

        let page = read_artifact_page(&artifact_ref, Some(dir.path()), 1, 8, None).unwrap();
        assert!(page["output"].as_str().unwrap().is_char_boundary(0));
        assert!(page["offset"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn history_compaction_artifact_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        let messages = vec![AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: "older context".into(),
            }],
            timestamp: 0,
        }];

        let stored = store_history_compaction(&messages, Some(dir.path())).unwrap();
        assert!(stored
            .artifact_ref
            .starts_with("artifact://history/sha256:"));

        let page = read_artifact_page(&stored.artifact_ref, Some(dir.path()), 0, 12000, None)
            .expect("history artifact should be readable");
        assert_eq!(page["kind"], "history");
        assert!(page["output"].as_str().unwrap().contains("older context"));
    }
}
