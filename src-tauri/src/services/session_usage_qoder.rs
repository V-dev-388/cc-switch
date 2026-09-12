//! Qoder (海外版) session usage importer.
//!
//! Qoder persists project sessions under `~/.qoder/projects/*/*.jsonl` (including
//! subagents). Assistant messages record precise `credits` but zero tokens;
//! costs are converted from credits (1 credit = $0.01) into `total_cost_usd`, and
//! tokens are estimated from text content (~3.5 characters per token).
//! Deduplication keys `qoder:<request_id>` are recorded in `session_usage_dedup`
//! to guarantee idempotent sync passes.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::services::session_usage::{
    metadata_modified_nanos, update_sync_state_on_conn, SessionSyncResult,
};
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_FRESH;
use rusqlite::OptionalExtension;
use rust_decimal::Decimal;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub(crate) const APP_TYPE: &str = "qoder";
pub(crate) const DATA_SOURCE: &str = "qoder_session";
pub(crate) const PROVIDER_PLACEHOLDER: &str = "_qoder_session";
pub(crate) const UNKNOWN_MODEL: &str = "unknown";
pub(crate) const MAX_USAGE_LABEL_BYTES: usize = 512;
pub(crate) const MAX_SESSION_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MIN_SQLITE_UNIX_SECONDS: i64 = -62_167_219_200;
pub(crate) const MAX_SQLITE_UNIX_SECONDS: i64 = 253_402_300_799;
pub(crate) const REQUEST_ID_PREFIX: &str = "qoder:";
pub(crate) const CHARS_PER_TOKEN: f64 = 3.5;
pub(crate) const QODER_REQUEST_DEDUP_SQL: &str = "SELECT EXISTS(
    SELECT 1 FROM session_usage_dedup
    WHERE data_source = ?1 AND request_id = ?2
)";

#[derive(Debug, Clone)]
pub(crate) struct QoderUsageRecord {
    pub request_id: String,
    pub provider_id: String,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub credits: f64,
    pub created_at: i64,
    pub session_id: String,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedQoderFile {
    pub records: Vec<QoderUsageRecord>,
    pub deferred: bool,
}

/// Import usage from every Qoder session file discovered under the projects root.
pub fn sync_qoder_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let root = qoder_projects_root();
    if !root.exists() {
        return Ok(SessionSyncResult {
            files_scanned: 0,
            ..Default::default()
        });
    }
    let files = qoder_session_files(&root);
    Ok(sync_qoder_files(db, &files))
}

fn qoder_projects_root() -> PathBuf {
    if let Some(custom) = std::env::var_os("QODER_PROJECTS_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(crate::config::get_home_dir)
        .join(".qoder")
        .join("projects")
}

fn qoder_session_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_files(root, &mut files);
    files.sort();
    files
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files);
        } else if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn sync_qoder_files(db: &Database, files: &[PathBuf]) -> SessionSyncResult {
    let mut result = SessionSyncResult {
        files_scanned: files.len().min(u32::MAX as usize) as u32,
        ..Default::default()
    };

    for file_path in files {
        match sync_single_qoder_file(db, file_path) {
            Ok(file_result) => result.merge(file_result),
            Err(error) => {
                let message = format!("{}: {error}", file_path.display());
                log::warn!("[QODER-SYNC] 会话文件解析失败: {message}");
                result.errors.push(message);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[QODER-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    result
}

fn sync_single_qoder_file(db: &Database, file_path: &Path) -> Result<SessionSyncResult, AppError> {
    let metadata = fs::symlink_metadata(file_path)
        .map_err(|error| AppError::Config(format!("无法读取 Qoder 会话文件元数据: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(AppError::Config("Qoder 会话路径不是普通文件".to_string()));
    }
    if metadata.len() > MAX_SESSION_BYTES {
        log::warn!(
            "[QODER-SYNC] 文件超过安全上限 ({} > {} bytes)，延迟处理: {}",
            metadata.len(),
            MAX_SESSION_BYTES,
            file_path.display()
        );
        return Ok(SessionSyncResult {
            deferred_files: 1,
            ..Default::default()
        });
    }

    let modified = metadata_modified_nanos(&metadata);
    let file_path_string = file_path.to_string_lossy().to_string();
    if qoder_file_unchanged(db, &file_path_string, modified)? {
        return Ok(SessionSyncResult::default());
    }

    let parsed = parse_qoder_file(file_path, modified / 1_000_000_000)?;
    if parsed.deferred {
        return Ok(SessionSyncResult {
            deferred_files: 1,
            ..Default::default()
        });
    }

    let conn = lock_conn!(db.conn);
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| AppError::Database(format!("启动 Qoder 用量导入事务失败: {error}")))?;
    let mut result = SessionSyncResult::default();
    for record in &parsed.records {
        if insert_qoder_record(&tx, record)? {
            result.imported = result.imported.saturating_add(1);
        } else {
            result.skipped = result.skipped.saturating_add(1);
        }
    }

    update_sync_state_on_conn(
        &tx,
        &file_path_string,
        modified,
        parsed.records.len().min(i64::MAX as usize) as i64,
    )?;
    tx.commit()
        .map_err(|error| AppError::Database(format!("提交 Qoder 用量导入事务失败: {error}")))?;

    Ok(result)
}

fn qoder_file_unchanged(db: &Database, file_path: &str, modified: i64) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    let last_modified: Option<i64> = conn
        .query_row(
            "SELECT last_modified FROM session_log_sync WHERE file_path = ?1",
            rusqlite::params![file_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| AppError::Database(format!("读取 Qoder 会话同步状态失败: {error}")))?;
    Ok(last_modified.is_some_and(|last| modified <= last))
}

fn parse_qoder_file(
    file_path: &Path,
    file_modified_seconds: i64,
) -> Result<ParsedQoderFile, AppError> {
    let file = File::open(file_path)
        .map_err(|error| AppError::Config(format!("无法打开 Qoder 会话文件: {error}")))?;
    let reader = BufReader::new(file);
    let mut parsed = ParsedQoderFile::default();

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<Value>(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if value.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }

        if let Some(record) = parse_assistant_record(&value, file_modified_seconds) {
            parsed.records.push(record);
        }
    }

    Ok(parsed)
}

fn parse_assistant_record(value: &Value, file_modified_seconds: i64) -> Option<QoderUsageRecord> {
    let message = value.get("message");
    let usage = message
        .and_then(|m| m.get("usage"))
        .or_else(|| value.get("usage"));

    let model = message
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .or_else(|| value.get("model").and_then(Value::as_str))
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(truncate_usage_label)
        .unwrap_or(UNKNOWN_MODEL)
        .to_string();

    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .or_else(|| {
            message
                .and_then(|m| m.get("sessionId"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(truncate_usage_label)
        .unwrap_or("")
        .to_string();

    let uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .or_else(|| message.and_then(|m| m.get("id")).and_then(Value::as_str))
        .unwrap_or("");

    let raw_request_id = usage
        .and_then(|u| u.get("request_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(uuid);

    let request_id = if raw_request_id.is_empty() {
        format!("{REQUEST_ID_PREFIX}{}", uuid::Uuid::new_v4())
    } else {
        format!("{REQUEST_ID_PREFIX}{raw_request_id}")
    };

    let credits = usage
        .and_then(|u| u.get("credits"))
        .and_then(Value::as_f64)
        .or_else(|| value.get("credits").and_then(Value::as_f64))
        .unwrap_or(0.0)
        .max(0.0);

    let text_chars = extract_content_chars(
        message
            .and_then(|m| m.get("content"))
            .or_else(|| value.get("content")),
    );
    let estimated_output_tokens = if text_chars > 0 {
        ((text_chars as f64) / CHARS_PER_TOKEN).round() as u32
    } else {
        0
    };
    let estimated_output_tokens = if text_chars > 0 && estimated_output_tokens == 0 {
        1
    } else {
        estimated_output_tokens
    };

    let reported_input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;

    let reported_output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;

    let output_tokens = if reported_output_tokens > 0 {
        reported_output_tokens
    } else {
        estimated_output_tokens
    };

    let created_at = parse_timestamp(value.get("timestamp"))
        .unwrap_or(file_modified_seconds)
        .clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS);

    Some(QoderUsageRecord {
        request_id,
        provider_id: PROVIDER_PLACEHOLDER.to_string(),
        model,
        input_tokens: reported_input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        credits,
        created_at,
        session_id,
    })
}

fn extract_content_chars(content: Option<&Value>) -> usize {
    let Some(content) = content else {
        return 0;
    };
    match content {
        Value::String(s) => s.chars().count(),
        Value::Array(blocks) => {
            let mut total = 0;
            for block in blocks {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    total += text.chars().count();
                } else if let Some(thinking) = block.get("thinking").and_then(Value::as_str) {
                    total += thinking.chars().count();
                } else if let Some(input) = block.get("input") {
                    if let Some(s) = input.as_str() {
                        total += s.chars().count();
                    } else {
                        total += input.to_string().chars().count();
                    }
                }
            }
            total
        }
        _ => 0,
    }
}

fn parse_timestamp(timestamp_val: Option<&Value>) -> Option<i64> {
    let val = timestamp_val?;
    if let Some(num) = val.as_i64() {
        return if num > 1_000_000_000_000 {
            Some(num / 1000)
        } else {
            Some(num)
        };
    }
    if let Some(s) = val.as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
            return Some(dt.timestamp());
        }
    }
    None
}

fn truncate_usage_label(value: &str) -> &str {
    if value.len() <= MAX_USAGE_LABEL_BYTES {
        return value;
    }
    let mut end = MAX_USAGE_LABEL_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn insert_qoder_record(
    conn: &rusqlite::Connection,
    record: &QoderUsageRecord,
) -> Result<bool, AppError> {
    let already_seen: bool = conn
        .query_row(
            QODER_REQUEST_DEDUP_SQL,
            rusqlite::params![DATA_SOURCE, record.request_id],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Database(format!("查询 Qoder 用量去重账本失败: {error}")))?;
    if already_seen {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup
         (data_source, request_id, semantic_id, has_entry_id)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![DATA_SOURCE, record.request_id, record.request_id, 1i64],
    )
    .map_err(|error| AppError::Database(format!("写入 Qoder 用量去重账本失败: {error}")))?;

    let total_cost = (Decimal::from_f64_retain(record.credits)
        .unwrap_or(Decimal::ZERO)
        .max(Decimal::ZERO)
        * Decimal::new(1, 2))
    .round_dp(6)
    .normalize();
    let output_cost = total_cost;
    let input_cost = Decimal::ZERO;

    conn.execute(
        "INSERT OR IGNORE INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd,
            cache_creation_cost_usd, total_cost_usd,
            latency_ms, first_token_ms, status_code, error_message, session_id,
            provider_type, is_streaming, cost_multiplier, created_at, data_source
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
            ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26
        )",
        rusqlite::params![
            record.request_id,
            record.provider_id,
            APP_TYPE,
            record.model,
            record.model,
            record.model,
            record.input_tokens,
            record.output_tokens,
            record.cache_read_tokens,
            record.cache_creation_tokens,
            INPUT_TOKEN_SEMANTICS_FRESH,
            input_cost.to_string(),
            output_cost.to_string(),
            "0",
            "0",
            total_cost.to_string(),
            0i64,
            Option::<i64>::None,
            200i64,
            Option::<String>::None,
            record.session_id,
            Some(DATA_SOURCE),
            0i64,
            "1.0",
            record.created_at,
            DATA_SOURCE,
        ],
    )
    .map(|changed| changed > 0)
    .map_err(|error| AppError::Database(format!("插入 Qoder 会话用量失败: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn lock_qoder_env() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // ── Test 1: Column assertion, credits conversion & token estimation ──
    #[test]
    fn test_qoder_single_record_columns_and_credits_to_cost() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-1");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("sess-1.jsonl");

        // 35 characters text -> 35 / 3.5 = 10 output tokens
        // credits = 5.176 -> cost = $0.05176
        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-qoder-test-01",
            "sessionId": "sess-alpha-001",
            "timestamp": "2026-08-27T13:32:57.000Z",
            "message": {
                "id": "chatcmpl-01",
                "model": "qmodel_38max",
                "usage": {
                    "credits": 5.176,
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "request_id": "req-qoder-01"
                },
                "content": [
                    {
                        "type": "text",
                        "text": "12345678901234567890123456789012345"
                    }
                ]
            }
        });
        fs::write(&jsonl_file, format!("{}\n", line)).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let result = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.files_scanned, 1);

        let conn = lock_conn!(db.conn);
        let row = conn.query_row(
            "SELECT request_id, provider_id, app_type, model, input_tokens, output_tokens,
                    total_cost_usd, session_id, data_source
             FROM proxy_request_logs WHERE request_id = 'qoder:req-qoder-01'",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, u32>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            },
        )?;

        assert_eq!(row.0, "qoder:req-qoder-01");
        assert_eq!(row.1, PROVIDER_PLACEHOLDER);
        assert_eq!(row.2, "qoder");
        assert_eq!(row.3, "qmodel_38max");
        assert_eq!(row.4, 0);
        assert_eq!(row.5, 10); // 35 / 3.5 = 10
        assert_eq!(row.6, "0.05176"); // 5.176 * 0.01
        assert_eq!(row.7, "sess-alpha-001");
        assert_eq!(row.8, "qoder_session");
        Ok(())
    }

    // ── Test 2: Idempotent second sync: 0 imported ──
    #[test]
    fn test_qoder_idempotent_second_sync_zero_imported() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-idemp");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("sess.jsonl");

        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "u-1",
            "sessionId": "s-1",
            "message": {
                "model": "qmodel_38max",
                "usage": { "credits": 2.0, "request_id": "r-idemp-1" },
                "content": "hello world"
            }
        });
        fs::write(&jsonl_file, format!("{}\n", line)).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let r1 = sync_qoder_usage(&db)?;
        assert_eq!(r1.imported, 1);

        // Second pass on unchanged file: 0 imported, 0 skipped
        let r2 = sync_qoder_usage(&db)?;
        assert_eq!(r2.imported, 0);
        assert_eq!(r2.skipped, 0);

        // Touch file mtime and sync again: re-reads file, deduplication skips row
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut file = fs::OpenOptions::new().append(true).open(&jsonl_file).unwrap();
        writeln!(file).unwrap();
        drop(file);

        let r3 = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(r3.imported, 0);
        assert_eq!(r3.skipped, 1);

        let count: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'qoder'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1, "重扫绝不重复新增");
        Ok(())
    }

    // ── Test 3: Missing directory gracefully returns 0 scanned ──
    #[test]
    fn test_qoder_missing_directory_returns_gracefully() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", "/path/to/nonexistent/qoder/projects");
        let db = Database::memory()?;
        let res = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(res.files_scanned, 0);
        assert_eq!(res.imported, 0);
        assert_eq!(res.skipped, 0);
        assert!(res.errors.is_empty());
        Ok(())
    }

    // ── Test 4: Corrupted lines and non-assistant lines are skipped ──
    #[test]
    fn test_qoder_skips_corrupted_lines_and_other_types() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-corrupt");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("corrupt.jsonl");

        let content = "\
{ not valid json }\n\
{\"type\":\"workspace-directories\",\"sessionId\":\"s-2\"}\n\
{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n\
\n\
{\"type\":\"assistant\",\"uuid\":\"uuid-valid-1\",\"sessionId\":\"s-2\",\"message\":{\"model\":\"qmodel_test\",\"usage\":{\"credits\":1.5,\"request_id\":\"req-valid-1\"},\"content\":\"test answer\"}}\n\
{ broken tail json\n";
        fs::write(&jsonl_file, content).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let res = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(res.imported, 1);
        assert_eq!(res.errors.len(), 0);
        Ok(())
    }

    // ── Test 5: Large file > 32MB is deferred ──
    #[test]
    fn test_qoder_defers_large_file_over_32mb() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-large");
        fs::create_dir_all(&project_dir).unwrap();
        let large_file = project_dir.join("large.jsonl");

        let file = File::create(&large_file).unwrap();
        file.set_len(MAX_SESSION_BYTES + 1024).unwrap();
        drop(file);

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let res = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(res.deferred_files, 1);
        assert_eq!(res.imported, 0);
        Ok(())
    }

    // ── Test 6: Fallback to uuid when usage.request_id is absent ──
    #[test]
    fn test_qoder_request_id_fallback_to_uuid() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-fallback");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("fallback.jsonl");

        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "unique-msg-uuid-999",
            "sessionId": "sess-fallback",
            "message": {
                "model": "qmodel_38max",
                "usage": { "credits": 0.5 },
                "content": "ok"
            }
        });
        fs::write(&jsonl_file, format!("{}\n", line)).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let res = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(res.imported, 1);
        let req_id: String = lock_conn!(db.conn).query_row(
            "SELECT request_id FROM proxy_request_logs WHERE session_id = 'sess-fallback'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(req_id, "qoder:unique-msg-uuid-999");
        Ok(())
    }

    // ── Test 7: Subagents directories recursively scanned ──
    #[test]
    fn test_qoder_subagents_directory_recursively_scanned() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let subagent_dir = temp.path().join("proj-main/sess-root/subagents");
        fs::create_dir_all(&subagent_dir).unwrap();
        let subagent_file = subagent_dir.join("sub-01.jsonl");

        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "subagent-uuid-01",
            "sessionId": "sub-sess-01",
            "message": {
                "model": "qmodel_38max",
                "usage": { "credits": 3.0, "request_id": "req-sub-01" },
                "content": "subagent completed"
            }
        });
        fs::write(&subagent_file, format!("{}\n", line)).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let res = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(res.files_scanned, 1);
        assert_eq!(res.imported, 1);
        Ok(())
    }

    // ── Test 8: Thinking, text and tool_use blocks counted in token estimation ──
    #[test]
    fn test_qoder_complex_blocks_token_estimation() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-blocks");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("blocks.jsonl");

        // thinking: 35 chars, text: 35 chars -> 70 chars / 3.5 = 20 tokens
        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-blocks",
            "sessionId": "sess-blocks",
            "message": {
                "model": "qmodel_38max",
                "usage": { "credits": 4.2, "request_id": "req-blocks" },
                "content": [
                    { "type": "thinking", "thinking": "12345678901234567890123456789012345" },
                    { "type": "text", "text": "abcdefghijklmnopqrstuvwxyzABCDEFGHI" }
                ]
            }
        });
        fs::write(&jsonl_file, format!("{}\n", line)).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let res = sync_qoder_usage(&db)?;
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        assert_eq!(res.imported, 1);
        let tokens: u32 = lock_conn!(db.conn).query_row(
            "SELECT output_tokens FROM proxy_request_logs WHERE request_id = 'qoder:req-blocks'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(tokens, 20);
        Ok(())
    }

    // ── Test 9: Placeholder provider resolves to Qoder (Session) ──
    #[test]
    fn placeholder_provider_resolves_to_qoder_display_name() -> Result<(), AppError> {
        let db = Database::memory()?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT INTO proxy_request_logs (
                    request_id, provider_id, app_type, model, request_model,
                    input_tokens, output_tokens, latency_ms, status_code,
                    created_at, data_source
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    "display-qoder-test",
                    PROVIDER_PLACEHOLDER,
                    APP_TYPE,
                    "qmodel_38max",
                    "qmodel_38max",
                    10,
                    20,
                    0,
                    200,
                    1_787_362_690,
                    DATA_SOURCE,
                ],
            )?;
        }
        let providers = db.get_provider_stats(None, None, Some(APP_TYPE), None, None)?;
        assert!(
            providers.iter().any(|p| {
                p.provider_id == PROVIDER_PLACEHOLDER && p.provider_name == "Qoder (Session)"
            }),
            "provider_placeholder 必须解析为 'Qoder (Session)': {providers:?}"
        );
        Ok(())
    }

    // ── Test 10: Qoder step is registered in sync_all_unlocked (Safeguard test) ──
    #[test]
    fn qoder_step_is_registered_in_sync_all_unlocked() -> Result<(), AppError> {
        let _guard = lock_qoder_env();
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("proj-registered");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("sess.jsonl");

        let line = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-reg-01",
            "sessionId": "sess-reg-01",
            "message": {
                "model": "qmodel_38max",
                "usage": { "credits": 1.0, "request_id": "req-reg-01" },
                "content": "registered step test"
            }
        });
        fs::write(&jsonl_file, format!("{}\n", line)).unwrap();

        let original = std::env::var_os("QODER_PROJECTS_DIR");
        std::env::set_var("QODER_PROJECTS_DIR", temp.path());
        let db = Database::memory()?;
        let result = crate::services::session_usage::sync_all_unlocked(&db);
        match original {
            Some(v) => std::env::set_var("QODER_PROJECTS_DIR", v),
            None => std::env::remove_var("QODER_PROJECTS_DIR"),
        }

        let count: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'qoder_session'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1, "sync_all_unlocked 必须执行 Qoder 导入步骤");
        assert!(
            result.errors.iter().all(|e| !e.contains("Qoder")),
            "Qoder 步骤不应产生错误: {:?}",
            result.errors
        );
        Ok(())
    }
}
