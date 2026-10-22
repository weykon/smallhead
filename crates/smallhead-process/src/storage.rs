//! 有界日志存储与恢复。目录只由一个管理器持有；不根据旧 PID 接管进程。
use crate::{ExecutionId, ProcessError, ProcessLimits, ProcessRecord, ProcessState};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::Path,
};

const SERVICES_FILE: &str = "services.json";
const SERVICES_MAX_BYTES: u64 = 262_144;

/// 默认 schema_version；写入新文件时使用。
fn default_schema_version() -> u32 {
    crate::service::SERVICES_SCHEMA_VERSION
}

pub(crate) const AUDIT_RESERVE: u64 = 131072;

pub(crate) fn lock_root(root: &Path) -> Result<std::fs::File, ProcessError> {
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".manager.lock"))?;
    fs4::FileExt::try_lock_exclusive(&file).map_err(|e| {
        ProcessError::Internal(format!("日志目录已被其他管理器占用或无法锁定：{e}"))
    })?;
    Ok(file)
}

pub(crate) fn recover(
    root: &Path,
    limits: &ProcessLimits,
) -> Result<HashMap<ExecutionId, ProcessRecord>, ProcessError> {
    let mut records = HashMap::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Ok(id) = entry.file_name().to_string_lossy().parse::<ExecutionId>() else {
            continue;
        };
        if records.len() >= limits.max_records {
            return Err(ProcessError::Internal(
                "历史记录数量超过上限；提高 max_records 或归档旧日志后再启动".into(),
            ));
        }
        let path = entry.path().join("meta.json");
        if !path.exists() {
            continue;
        }
        if std::fs::metadata(&path)?.len() > 65536 {
            return Err(ProcessError::Internal("历史 meta.json 过大".into()));
        }
        let mut record: ProcessRecord = serde_json::from_slice(&std::fs::read(&path)?)
            .map_err(|e| ProcessError::Internal(format!("历史记录损坏 {}：{e}", path.display())))?;
        if record.id != id {
            return Err(ProcessError::Internal("历史记录 ID 与目录不一致".into()));
        }
        record.log_dir = Some(entry.path());
        if !record.is_terminal() {
            record.state = ProcessState::Lost(
                "上次管理器未正常关闭，无法确认进程身份；不会按旧 PID 发送信号".into(),
            );
            record.finished_at = Some(std::time::SystemTime::now());
            std::fs::write(
                &path,
                serde_json::to_vec_pretty(&record)
                    .map_err(|e| ProcessError::Internal(e.to_string()))?,
            )?;
        }
        records.insert(id, record);
    }
    Ok(records)
}

pub(crate) fn load_services(
    root: &Path,
) -> Result<BTreeMap<String, crate::service::ServiceRecord>, ProcessError> {
    let path = root.join(SERVICES_FILE);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    if std::fs::metadata(&path)?.len() > SERVICES_MAX_BYTES {
        return Err(ProcessError::Internal(
            "services.json 超过 256 KiB 上限".into(),
        ));
    }
    let bytes = std::fs::read(&path)?;
    if let Ok(envelope) = serde_json::from_slice::<ServicesEnvelopeOwned>(&bytes) {
        // 仅接受 <= 当前 schema 的版本，避免未知字段引入歧义。
        if envelope.schema_version > crate::service::SERVICES_SCHEMA_VERSION {
            return Err(ProcessError::Internal(format!(
                "services.json schema_version {} 高于管理器已知版本 {}",
                envelope.schema_version,
                crate::service::SERVICES_SCHEMA_VERSION
            )));
        }
        return Ok(envelope.services);
    }
    // v0：裸 BTreeMap，直接解析（`description` 与 `run_version` 已通过 `#[serde(default)]` 兼容）。
    serde_json::from_slice(&bytes)
        .map_err(|e| ProcessError::Internal(format!("服务登记损坏 {}：{e}", path.display())))
}

/// v1 envelope 的 owned 形态，供 `serde_json::from_slice` 反序列化使用。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct ServicesEnvelopeOwned {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    services: BTreeMap<String, crate::service::ServiceRecord>,
}

pub(crate) fn save_services(
    root: &Path,
    records: &BTreeMap<String, crate::service::ServiceRecord>,
) -> Result<(), ProcessError> {
    let envelope = ServicesEnvelope {
        schema_version: default_schema_version(),
        services: records,
    };
    let bytes = serde_json::to_vec_pretty(&envelope)
        .map_err(|e| ProcessError::Internal(format!("编码服务登记失败：{e}")))?;
    if bytes.len() as u64 > SERVICES_MAX_BYTES {
        return Err(ProcessError::Internal("服务登记超过 256 KiB 上限".into()));
    }
    let path = root.join(SERVICES_FILE);
    let temporary = root.join("services.json.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, &path)?;
    if let Ok(directory) = std::fs::File::open(root) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// v1 envelope。引入 `description` / `run_version` 与新事件类型。
///
/// 旧版（v0）services.json 是裸 `BTreeMap<String, ServiceRecord>`，由 [`load_services`] 兼容读取。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
struct ServicesEnvelope<'a> {
    schema_version: u32,
    services: &'a BTreeMap<String, crate::service::ServiceRecord>,
}

pub(crate) fn available(
    root: &Path,
    records: &HashMap<ExecutionId, ProcessRecord>,
    limits: &ProcessLimits,
) -> Result<(), ProcessError> {
    let reserve = limits.max_stream_bytes * 2 + AUDIT_RESERVE;
    let mut used = 0u64;
    // 只扫描本管理器的两层布局，不递归跟随链接。
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            used = used.saturating_add(entry.metadata()?.len());
            continue;
        }
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let active = entry
            .file_name()
            .to_string_lossy()
            .parse::<ExecutionId>()
            .ok()
            .and_then(|id| records.get(&id))
            .is_some_and(|r| !r.is_terminal());
        if active {
            used = used.saturating_add(reserve);
            continue;
        }
        for file in std::fs::read_dir(entry.path())? {
            let file = file?;
            if file.file_type()?.is_file() {
                used = used.saturating_add(file.metadata()?.len());
            }
        }
    }
    if used.saturating_add(reserve) > limits.max_disk_bytes {
        return Err(ProcessError::Internal(
            "日志总配额不足，未启动；请先清理已完成执行或提高配额".into(),
        ));
    }
    Ok(())
}
