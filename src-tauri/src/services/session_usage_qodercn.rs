//! QoderCN (国内通义灵码版) session usage importer.
//!
//! Scans `~/.qoder-cn/projects/*` and `transcript/*.jsonl`.
//! Parses user and assistant message turns, estimates token counts from text (~3.5 chars/token),
//! maps models (dmodel, dfmodel, qfmodel, gfmodel, qmodel_latest, etc.) against
//! `model_pricing` to calculate costs (unmatched models record 0 cost).
//! Deduplication keys `qodercn:<session_id>:<uuid>` are recorded in `session_usage_dedup`.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::{
    metadata_modified_nanos, update_sync_state_on_conn, SessionSyncResult,
};
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL;
use crate::services::usage_stats::find_model_pricing;
use rusqlite::OptionalExtension;
use rust_decimal::Decimal;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub(crate) const APP_TYPE: &str = "qodercn";
pub(crate) const DATA_SOURCE: &str = "qodercn_session";
pub(crate) const PROVIDER_PLACEHOLDER: &str = "_qodercn_session";
#[allow(dead_code)]
pub(crate) const UNKNOWN_MODEL: &str = "unknown";
pub(crate) const MAX_USAGE_LABEL_BYTES: usize = 512;
pub(crate) const MAX_SESSION_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MIN_SQLITE_UNIX_SECONDS: i64 = -62_167_219_200;
pub(crate) const MAX_SQLITE_UNIX_SECONDS: i64 = 253_402_300_799;
pub(crate) const REQUEST_ID_PREFIX: &str = "qodercn:";
pub(crate) const CHARS_PER_TOKEN: f64 = 3.5;
pub(crate) const QODERCN_REQUEST_DEDUP_SQL: &str = "SELECT EXISTS(
    SELECT 1 FROM session_usage_dedup
    WHERE data_source = ?1 AND request_id = ?2
)";

#[derive(Debug, Clone)]
pub(crate) struct QoderCnUsageRecord {
    pub request_id: String,
    pub provider_id: String,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_creation_tokens: u32,
    pub created_at: i64,
    pub session_id: String,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedQoderCnFile {
    pub records: Vec<QoderCnUsageRecord>,
    pub deferred: bool,
}

/// Map QoderCN IDE internal model keys (qmodel_38max, dmodel, dfmodel, gfmodel, etc.)
/// to canonical model names configured in `model_pricing`.
pub(crate) fn map_qodercn_model(model_key: &str) -> &'static str {
    match model_key {
        "qmodel_38max" => "qwen3.8-max",
        "qfmodel" => "qwen3.8-flash",
        "qmodel_latest" => "qwen3.7-max",
        "qmodel" => "qwen3.7-plus",
        "q37fmodel" => "qwen3.6-flash",
        "qmodel_preview" => "qwen3.6-max-preview",
        "dmodel" => "deepseek-v4-pro",
        "dfmodel" => "deepseek-v4-flash",
        "gmodel" => "glm-5.3",
        "gfmodel" => "glm-5.3-flash",
        "gm51model" => "glm-5.2",
        "kmodel_latest" => "kimi-k3",
        "kmodel" => "kimi-k2.7-code",
        "mmodel" => "minimax-m2.7",
        "auto" | "" | "custom_model" => "qwen3.8-max",
        other => {
            let lower = other.to_ascii_lowercase();
            if lower.contains("deepseek") && lower.contains("flash") {
                "deepseek-v4-flash"
            } else if lower.contains("deepseek") {
                "deepseek-v4-pro"
            } else if lower.contains("glm") && lower.contains("flash") {
                "glm-5.3-flash"
            } else if lower.contains("glm") {
                "glm-5.3"
            } else if lower.contains("qwen") && lower.contains("flash") {
                "qwen3.8-flash"
            } else if lower.contains("qwen") && lower.contains("plus") {
                "qwen3.7-plus"
            } else if lower.contains("kimi") {
                "kimi-k3"
            } else if lower.contains("minimax") {
                "minimax-m2.7"
            } else {
                "qwen3.8-max"
            }
        }
    }
}

/// Locate QoderCN IDE SQLite database (`local.db`).
/// Supports override via `QODERCN_DB_PATH`.
pub fn qodercn_ide_db_path() -> PathBuf {
    if let Some(custom) = std::env::var_os("QODERCN_DB_PATH") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .unwrap_or_else(crate::config::get_home_dir)
            .join("Library")
            .join("Application Support")
            .join("QoderCN")
            .join("SharedClientCache")
            .join("cache")
            .join("db")
            .join("local.db")
    }
    #[cfg(target_os = "linux")]
    {
        dirs::home_dir()
            .unwrap_or_else(crate::config::get_home_dir)
            .join(".config")
            .join("QoderCN")
            .join("SharedClientCache")
            .join("cache")
            .join("db")
            .join("local.db")
    }
    #[cfg(target_os = "windows")]
    {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\AppData\Roaming"))
            .join("QoderCN")
            .join("SharedClientCache")
            .join("cache")
            .join("db")
            .join("local.db")
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        dirs::home_dir()
            .unwrap_or_else(crate::config::get_home_dir)
            .join(".qoder-cn")
            .join("local.db")
    }
}

/// Clean up legacy records where model was 'unknown' or unpriced due to missing IDE database scanning.
fn cleanup_legacy_unknown_records(db: &Database) {
    if let Ok(conn) = db.conn.lock() {
        let _ = conn.execute(
            "DELETE FROM session_usage_dedup WHERE data_source = 'qodercn_session' AND request_id IN (SELECT request_id FROM proxy_request_logs WHERE app_type = 'qodercn' AND model = 'unknown')",
            [],
        );
        let _ = conn.execute(
            "DELETE FROM proxy_request_logs WHERE app_type = 'qodercn' AND model = 'unknown'",
            [],
        );
    }
}

/// Import usage from QoderCN IDE SQLite database (`local.db`), where real chat sessions,
/// exact prompt/completion/cached tokens, and model configurations are stored.
pub(crate) fn sync_qodercn_ide_db(
    db: &Database,
    db_path: &Path,
) -> Result<SessionSyncResult, AppError> {
    if !db_path.exists() {
        return Ok(SessionSyncResult::default());
    }

    let source_conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .or_else(|_| {
        let uri = format!("file:{}?mode=ro&immutable=1", db_path.display());
        rusqlite::Connection::open_with_flags(
            &uri,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
    });

    let source_conn = match source_conn {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "[QODERCN-SYNC] 无法以只读模式打开 QoderCN IDE 数据库 {}: {e}",
                db_path.display()
            );
            return Ok(SessionSyncResult::default());
        }
    };

    let mut stmt = match source_conn.prepare(
        "SELECT
            m.id,
            COALESCE(m.session_id, ''),
            COALESCE(m.model_info, ''),
            COALESCE(m.token_info, ''),
            COALESCE(m.gmt_create, 0),
            COALESCE(s.preferred_model_info, '')
        FROM chat_message m
        LEFT JOIN chat_session s ON m.session_id = s.session_id
        WHERE m.token_info IS NOT NULL AND m.token_info != ''",
    ) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("[QODERCN-SYNC] QoderCN IDE 数据库表不存在或未初始化: {e}");
            return Ok(SessionSyncResult::default());
        }
    };

    let mut rows = stmt
        .query([])
        .map_err(|e| AppError::Database(format!("执行 QoderCN IDE 查询失败: {e}")))?;

    let target_conn = lock_conn!(db.conn);
    let tx = target_conn
        .unchecked_transaction()
        .map_err(|e| AppError::Database(format!("启动 QoderCN IDE 事务失败: {e}")))?;

    let mut result = SessionSyncResult {
        files_scanned: 1,
        ..Default::default()
    };

    while let Some(row) = rows
        .next()
        .map_err(|e| AppError::Database(format!("读取 QoderCN 记录失败: {e}")))?
    {
        let msg_id: String = row.get(0)?;
        let session_id: String = row.get(1)?;
        let model_info_str: String = row.get(2)?;
        let token_info_str: String = row.get(3)?;
        let gmt_create: i64 = row.get(4)?;
        let preferred_model_info_str: String = row.get(5)?;

        let request_id = format!("{REQUEST_ID_PREFIX}ide:{msg_id}");

        let already_seen: bool = tx
            .query_row(
                QODERCN_REQUEST_DEDUP_SQL,
                rusqlite::params![DATA_SOURCE, request_id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if already_seen {
            result.skipped = result.skipped.saturating_add(1);
            continue;
        }

        let Ok(token_info) = serde_json::from_str::<serde_json::Value>(&token_info_str) else {
            result.skipped = result.skipped.saturating_add(1);
            continue;
        };

        let prompt_tokens = token_info
            .get("prompt_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;
        let completion_tokens = token_info
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;
        let cached_tokens = token_info
            .get("cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32;

        if prompt_tokens == 0 && completion_tokens == 0 {
            result.skipped = result.skipped.saturating_add(1);
            continue;
        }

        let mut model_key = serde_json::from_str::<serde_json::Value>(&model_info_str)
            .ok()
            .and_then(|v| {
                v.get("model_key")
                    .and_then(|k| k.as_str())
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default();

        if model_key.is_empty() || model_key == "auto" {
            if let Ok(pref) = serde_json::from_str::<serde_json::Value>(&preferred_model_info_str) {
                if let Some(p) = pref.get("preferred_model").and_then(|v| v.as_str()) {
                    model_key = p.trim().to_string();
                }
            }
        }

        let model = map_qodercn_model(&model_key);

        let created_at = if gmt_create > 1_000_000_000_000 {
            (gmt_create / 1000).clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS)
        } else if gmt_create > 0 {
            gmt_create.clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS)
        } else {
            0
        };

        tx.execute(
            "INSERT OR IGNORE INTO session_usage_dedup (data_source, request_id, semantic_id, has_entry_id) VALUES (?1, ?2, ?3, 1)",
            rusqlite::params![DATA_SOURCE, request_id, request_id],
        )?;

        let usage = TokenUsage {
            input_tokens: prompt_tokens,
            output_tokens: completion_tokens,
            cache_read_tokens: cached_tokens,
            cache_creation_tokens: 0,
            model: Some(model.to_string()),
            message_id: None,
        };

        let pricing_opt = find_model_pricing(&tx, model)
            .or_else(|| find_model_pricing(&tx, &model_key));
        let (input_cost, output_cost, cache_read_cost, cache_write_cost, total_cost) = match pricing_opt {
            Some(pricing) => {
                let calc = CostCalculator::calculate_for_app(APP_TYPE, &usage, &pricing, Decimal::ONE);
                (
                    calc.input_cost,
                    calc.output_cost,
                    calc.cache_read_cost,
                    calc.cache_creation_cost,
                    calc.total_cost,
                )
            }
            None => (
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
                Decimal::ZERO,
            ),
        };

        tx.execute(
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
                request_id,
                PROVIDER_PLACEHOLDER,
                APP_TYPE,
                model,
                model,
                model,
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                0u32,
                INPUT_TOKEN_SEMANTICS_TOTAL,
                input_cost.to_string(),
                output_cost.to_string(),
                cache_read_cost.to_string(),
                cache_write_cost.to_string(),
                total_cost.to_string(),
                0i64,
                Option::<i64>::None,
                200i64,
                Option::<String>::None,
                session_id,
                Some(DATA_SOURCE),
                0i64,
                "1.0",
                created_at,
                DATA_SOURCE,
            ],
        )?;

        result.imported = result.imported.saturating_add(1);
    }

    tx.commit()
        .map_err(|e| AppError::Database(format!("提交 QoderCN IDE 导入事务失败: {e}")))?;

    if result.imported > 0 {
        log::info!(
            "[QODERCN-SYNC] IDE 数据库同步完成: 导入 {} 条, 跳过 {} 条 ({})",
            result.imported,
            result.skipped,
            db_path.display()
        );
    }

    Ok(result)
}

/// Import usage from QoderCN IDE SQLite database (`local.db`) and projects directory.
pub fn sync_qodercn_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    cleanup_legacy_unknown_records(db);

    let mut result = SessionSyncResult::default();

    let ide_db = qodercn_ide_db_path();
    if ide_db.exists() {
        match sync_qodercn_ide_db(db, &ide_db) {
            Ok(ide_res) => result.merge(ide_res),
            Err(e) => {
                let msg = format!("QoderCN IDE 数据库解析失败: {e}");
                log::warn!("[QODERCN-SYNC] {msg}");
                result.errors.push(msg);
            }
        }
    }

    let root = qodercn_projects_root();
    if root.exists() {
        let files = qodercn_session_files(&root);
        result.merge(sync_qodercn_files(db, &files));
    }

    Ok(result)
}

fn qodercn_projects_root() -> PathBuf {
    if let Some(custom) = std::env::var_os("QODERCN_PROJECTS_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(crate::config::get_home_dir)
        .join(".qoder-cn")
        .join("projects")
}

fn qodercn_session_files(root: &Path) -> Vec<PathBuf> {
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

fn sync_qodercn_files(db: &Database, files: &[PathBuf]) -> SessionSyncResult {
    let mut result = SessionSyncResult {
        files_scanned: files.len().min(u32::MAX as usize) as u32,
        ..Default::default()
    };

    for file_path in files {
        match sync_single_qodercn_file(db, file_path) {
            Ok(file_result) => result.merge(file_result),
            Err(error) => {
                let message = format!("{}: {error}", file_path.display());
                log::warn!("[QODERCN-SYNC] 会话文件解析失败: {message}");
                result.errors.push(message);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[QODERCN-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    result
}

fn sync_single_qodercn_file(
    db: &Database,
    file_path: &Path,
) -> Result<SessionSyncResult, AppError> {
    let metadata = fs::symlink_metadata(file_path)
        .map_err(|error| AppError::Config(format!("无法读取 QoderCN 会话文件元数据: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(AppError::Config("QoderCN 会话路径不是普通文件".to_string()));
    }
    if metadata.len() > MAX_SESSION_BYTES {
        log::warn!(
            "[QODERCN-SYNC] 文件超过安全上限 ({} > {} bytes)，延迟处理: {}",
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
    if qodercn_file_unchanged(db, &file_path_string, modified)? {
        return Ok(SessionSyncResult::default());
    }

    let parsed = parse_qodercn_file(file_path, modified / 1_000_000_000)?;
    if parsed.deferred {
        return Ok(SessionSyncResult {
            deferred_files: 1,
            ..Default::default()
        });
    }

    let conn = lock_conn!(db.conn);
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| AppError::Database(format!("启动 QoderCN 用量导入事务失败: {error}")))?;
    let mut result = SessionSyncResult::default();
    for record in &parsed.records {
        if insert_qodercn_record(&tx, record)? {
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
        .map_err(|error| AppError::Database(format!("提交 QoderCN 用量导入事务失败: {error}")))?;

    Ok(result)
}

fn qodercn_file_unchanged(
    db: &Database,
    file_path: &str,
    modified: i64,
) -> Result<bool, AppError> {
    let conn = lock_conn!(db.conn);
    let last_modified: Option<i64> = conn
        .query_row(
            "SELECT last_modified FROM session_log_sync WHERE file_path = ?1",
            rusqlite::params![file_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| AppError::Database(format!("读取 QoderCN 会话同步状态失败: {error}")))?;
    Ok(last_modified.is_some_and(|last| modified <= last))
}

fn parse_qodercn_file(
    file_path: &Path,
    file_modified_seconds: i64,
) -> Result<ParsedQoderCnFile, AppError> {
    let file = File::open(file_path)
        .map_err(|error| AppError::Config(format!("无法打开 QoderCN 会话文件: {error}")))?;
    let reader = BufReader::new(file);
    let mut parsed = ParsedQoderCnFile::default();

    let mut current_runtime_model: Option<String> = None;
    let mut pending_user_chars: usize = 0;

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

        let msg_type = value.get("type").and_then(Value::as_str).unwrap_or("");

        if msg_type == "runtime-config" {
            if let Some(m) = value.get("model").and_then(Value::as_str) {
                let trimmed_m = m.trim();
                if !trimmed_m.is_empty() && trimmed_m != "auto" && trimmed_m != "<synthetic>" {
                    current_runtime_model = Some(trimmed_m.to_string());
                }
            }
            continue;
        }

        if msg_type == "user" {
            let user_content = value
                .get("message")
                .and_then(|m| m.get("content"))
                .or_else(|| value.get("content"));
            pending_user_chars = extract_content_chars(user_content);
            continue;
        }

        if msg_type == "assistant" {
            if let Some(record) = parse_assistant_turn(
                &value,
                pending_user_chars,
                current_runtime_model.as_deref(),
                file_modified_seconds,
            ) {
                parsed.records.push(record);
            }
            pending_user_chars = 0;
        }
    }

    Ok(parsed)
}

fn parse_assistant_turn(
    value: &Value,
    user_chars: usize,
    runtime_model: Option<&str>,
    file_modified_seconds: i64,
) -> Option<QoderCnUsageRecord> {
    let message = value.get("message");
    let usage = message
        .and_then(|m| m.get("usage"))
        .or_else(|| value.get("usage"));

    let extracted_model = message
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .or_else(|| value.get("model").and_then(Value::as_str))
        .map(str::trim)
        .filter(|m| !m.is_empty() && *m != "auto" && *m != "<synthetic>");

    let raw_model = extracted_model.or(runtime_model).unwrap_or("auto");
    let model = map_qodercn_model(raw_model).to_string();

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
        .map(str::trim)
        .filter(|u| !u.is_empty());

    let final_uuid = match uuid {
        Some(u) => u.to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    };

    let request_id = format!("{REQUEST_ID_PREFIX}{session_id}:{final_uuid}");

    let assistant_chars = extract_content_chars(
        message
            .and_then(|m| m.get("content"))
            .or_else(|| value.get("content")),
    );

    let estimated_input_tokens = if user_chars > 0 {
        ((user_chars as f64) / CHARS_PER_TOKEN).round() as u32
    } else {
        0
    };
    let estimated_input_tokens = if user_chars > 0 && estimated_input_tokens == 0 {
        1
    } else {
        estimated_input_tokens
    };

    let estimated_output_tokens = if assistant_chars > 0 {
        ((assistant_chars as f64) / CHARS_PER_TOKEN).round() as u32
    } else {
        0
    };
    let estimated_output_tokens = if assistant_chars > 0 && estimated_output_tokens == 0 {
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

    let input_tokens = if reported_input_tokens > 0 {
        reported_input_tokens
    } else {
        estimated_input_tokens
    };

    let output_tokens = if reported_output_tokens > 0 {
        reported_output_tokens
    } else {
        estimated_output_tokens
    };

    let created_at = parse_timestamp(value.get("timestamp"))
        .unwrap_or(file_modified_seconds)
        .clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS);

    Some(QoderCnUsageRecord {
        request_id,
        provider_id: PROVIDER_PLACEHOLDER.to_string(),
        model,
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
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

fn insert_qodercn_record(
    conn: &rusqlite::Connection,
    record: &QoderCnUsageRecord,
) -> Result<bool, AppError> {
    let already_seen: bool = conn
        .query_row(
            QODERCN_REQUEST_DEDUP_SQL,
            rusqlite::params![DATA_SOURCE, record.request_id],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Database(format!("查询 QoderCN 用量去重账本失败: {error}")))?;
    if already_seen {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup
         (data_source, request_id, semantic_id, has_entry_id)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![DATA_SOURCE, record.request_id, record.request_id, 1i64],
    )
    .map_err(|error| AppError::Database(format!("写入 QoderCN 用量去重账本失败: {error}")))?;

    let usage = TokenUsage {
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        cache_read_tokens: record.cache_read_tokens,
        cache_creation_tokens: record.cache_creation_tokens,
        model: Some(record.model.clone()),
        message_id: None,
    };

    let costs = find_model_pricing(conn, &record.model).map(|pricing| {
        let calculated =
            CostCalculator::calculate_for_app(APP_TYPE, &usage, &pricing, Decimal::ONE);
        (
            calculated.input_cost,
            calculated.output_cost,
            calculated.cache_read_cost,
            calculated.cache_creation_cost,
            calculated.total_cost,
        )
    });

    let (input_cost, output_cost, cache_read_cost, cache_write_cost, total_cost) =
        costs.unwrap_or((
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
        ));

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
            INPUT_TOKEN_SEMANTICS_TOTAL,
            input_cost.to_string(),
            output_cost.to_string(),
            cache_read_cost.to_string(),
            cache_write_cost.to_string(),
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
    .map_err(|error| AppError::Database(format!("插入 QoderCN 会话用量失败: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct EnvGuard {
        _mutex: std::sync::MutexGuard<'static, ()>,
        orig_projects: Option<std::ffi::OsString>,
        orig_db: Option<std::ffi::OsString>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.orig_projects {
                Some(v) => std::env::set_var("QODERCN_PROJECTS_DIR", v),
                None => std::env::remove_var("QODERCN_PROJECTS_DIR"),
            }
            match &self.orig_db {
                Some(v) => std::env::set_var("QODERCN_DB_PATH", v),
                None => std::env::remove_var("QODERCN_DB_PATH"),
            }
        }
    }

    fn lock_qodercn_env(temp: &Path) -> EnvGuard {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let mutex = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let orig_projects = std::env::var_os("QODERCN_PROJECTS_DIR");
        let orig_db = std::env::var_os("QODERCN_DB_PATH");
        std::env::set_var("QODERCN_PROJECTS_DIR", temp);
        std::env::set_var("QODERCN_DB_PATH", temp.join("nonexistent_local.db"));
        EnvGuard {
            _mutex: mutex,
            orig_projects,
            orig_db,
        }
    }

    // ── Test 1: Session parsing, token estimation & model pricing matching ──
    #[test]
    fn test_qodercn_turn_parsing_tokens_and_pricing_lookup() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-1");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("sess-cn-1.jsonl");

        let user_line = serde_json::json!({
            "type": "user",
            "uuid": "u-user-1",
            "sessionId": "sess-cn-01",
            "message": { "role": "user", "content": "12345678901234567890123456789012345" }
        });
        let assistant_line = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-asst-1",
            "sessionId": "sess-cn-01",
            "timestamp": "2026-06-30T12:00:00Z",
            "message": {
                "role": "assistant",
                "model": "dmodel",
                "content": "1234567890123456789012345678901234567890123456789012345678901234567890"
            }
        });
        fs::write(
            &jsonl_file,
            format!("{}\n{}\n", user_line, assistant_line),
        )
        .unwrap();

        let db = Database::memory()?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT OR REPLACE INTO model_pricing (
                    model_id, display_name, input_cost_per_million, output_cost_per_million,
                    cache_read_cost_per_million, cache_creation_cost_per_million
                ) VALUES ('deepseek-v4-pro', 'DeepSeek V4 Pro', 2.0, 8.0, 0.5, 1.0)",
                [],
            )?;
        }

        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.imported, 1);
        assert_eq!(res.skipped, 0);

        let conn = lock_conn!(db.conn);
        let row = conn.query_row(
            "SELECT request_id, provider_id, app_type, model, input_tokens, output_tokens,
                    input_cost_usd, output_cost_usd, total_cost_usd, session_id, data_source
             FROM proxy_request_logs WHERE request_id = 'qodercn:sess-cn-01:uuid-asst-1'",
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
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                ))
            },
        )?;

        assert_eq!(row.0, "qodercn:sess-cn-01:uuid-asst-1");
        assert_eq!(row.1, PROVIDER_PLACEHOLDER);
        assert_eq!(row.2, "qodercn");
        assert_eq!(row.3, "deepseek-v4-pro");
        assert_eq!(row.4, 10);
        assert_eq!(row.5, 20);
        assert_eq!(row.6, "0.00002");
        assert_eq!(row.7, "0.00016");
        assert_eq!(row.8, "0.00018");
        assert_eq!(row.9, "sess-cn-01");
        assert_eq!(row.10, "qodercn_session");
        Ok(())
    }

    // ── Test 2: Unpriced model defaults to 0 cost ──
    #[test]
    fn test_qodercn_unpriced_model_defaults_to_zero_cost() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-unpriced");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("unpriced.jsonl");

        let asst_line = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-unpriced-01",
            "sessionId": "sess-unpriced",
            "message": {
                "model": "qmodel_latest",
                "content": "some text response"
            }
        });
        fs::write(&jsonl_file, format!("{}\n", asst_line)).unwrap();

        let db = Database::memory()?;
        // Clear all model pricing to test zero cost fallback
        {
            let conn = lock_conn!(db.conn);
            conn.execute("DELETE FROM model_pricing", [])?;
        }

        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.imported, 1);
        let total_cost: String = lock_conn!(db.conn).query_row(
            "SELECT total_cost_usd FROM proxy_request_logs WHERE request_id = 'qodercn:sess-unpriced:uuid-unpriced-01'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(total_cost, "0");
        Ok(())
    }

    // ── Test 3: Dedup idempotency: second pass 0 imported ──
    #[test]
    fn test_qodercn_dedup_idempotency_second_sync() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-idemp");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("idemp.jsonl");

        let asst_line = serde_json::json!({
            "type": "assistant",
            "uuid": "u-idemp-01",
            "sessionId": "s-idemp-01",
            "message": { "model": "dfmodel", "content": "dfmodel response" }
        });
        fs::write(&jsonl_file, format!("{}\n", asst_line)).unwrap();

        let db = Database::memory()?;
        let r1 = sync_qodercn_usage(&db)?;
        assert_eq!(r1.imported, 1);

        // Unchanged file -> 0 imported, 0 skipped
        let r2 = sync_qodercn_usage(&db)?;
        assert_eq!(r2.imported, 0);
        assert_eq!(r2.skipped, 0);

        // Touch file -> re-reads, dedup skips
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut f = fs::OpenOptions::new().append(true).open(&jsonl_file).unwrap();
        writeln!(f).unwrap();
        drop(f);

        let r3 = sync_qodercn_usage(&db)?;
        assert_eq!(r3.imported, 0);
        assert_eq!(r3.skipped, 1);
        Ok(())
    }

    // ── Test 4: Transcript subdirectories recursively collected ──
    #[test]
    fn test_qodercn_transcript_subdirectories_scanned() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let transcript_dir = temp.path().join("proj-a/transcript");
        fs::create_dir_all(&transcript_dir).unwrap();
        let transcript_file = transcript_dir.join("t-01.jsonl");

        let asst_line = serde_json::json!({
            "type": "assistant",
            "uuid": "u-transcript-01",
            "sessionId": "s-transcript-01",
            "message": { "model": "qfmodel", "content": "transcript response" }
        });
        fs::write(&transcript_file, format!("{}\n", asst_line)).unwrap();

        let db = Database::memory()?;
        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.files_scanned, 1);
        assert_eq!(res.imported, 1);
        Ok(())
    }

    // ── Test 5: Missing directory handled gracefully ──
    #[test]
    fn test_qodercn_missing_directory_returns_gracefully() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let non_existent = temp.path().join("nonexistent_qodercn_path");
        let _guard = lock_qodercn_env(&non_existent);

        let db = Database::memory()?;
        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.files_scanned, 0);
        assert_eq!(res.imported, 0);
        assert!(res.errors.is_empty());
        Ok(())
    }

    // ── Test 6: Skips corrupted lines, session_meta, progress ──
    #[test]
    fn test_qodercn_skips_corrupted_lines_and_meta() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-meta");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("meta.jsonl");

        let content = "\
{\"type\":\"session_meta\",\"sessionId\":\"s-meta\"}\n\
{\"type\":\"progress\",\"sessionId\":\"s-meta\"}\n\
{ not valid json }\n\
{\"type\":\"assistant\",\"uuid\":\"u-meta-01\",\"sessionId\":\"s-meta\",\"message\":{\"model\":\"gfmodel\",\"content\":\"meta test\"}}\n";
        fs::write(&jsonl_file, content).unwrap();

        let db = Database::memory()?;
        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.imported, 1);
        assert!(res.errors.is_empty());
        Ok(())
    }

    // ── Test 7: Large file > 32MB is deferred ──
    #[test]
    fn test_qodercn_defers_large_file_over_32mb() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-large");
        fs::create_dir_all(&project_dir).unwrap();
        let large_file = project_dir.join("large.jsonl");

        let file = File::create(&large_file).unwrap();
        file.set_len(MAX_SESSION_BYTES + 1024).unwrap();
        drop(file);

        let db = Database::memory()?;
        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.deferred_files, 1);
        assert_eq!(res.imported, 0);
        Ok(())
    }

    // ── Test 8: Multiple turns in single session file ──
    #[test]
    fn test_qodercn_multiple_turns_in_single_file() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-turns");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("turns.jsonl");

        let u1 = serde_json::json!({
            "type": "user",
            "message": { "content": "12345678901234567890123456789012345" } // 35 chars -> 10 tokens
        });
        let a1 = serde_json::json!({
            "type": "assistant",
            "uuid": "asst-turn-1",
            "sessionId": "sess-turns",
            "message": { "model": "dmodel", "content": "1234567" } // 7 chars -> 2 tokens
        });
        let u2 = serde_json::json!({
            "type": "user",
            "message": { "content": "12345678901234" } // 14 chars -> 4 tokens
        });
        let a2 = serde_json::json!({
            "type": "assistant",
            "uuid": "asst-turn-2",
            "sessionId": "sess-turns",
            "message": { "model": "dfmodel", "content": "123456789012345678901" } // 21 chars -> 6 tokens
        });

        fs::write(
            &jsonl_file,
            format!("{}\n{}\n{}\n{}\n", u1, a1, u2, a2),
        )
        .unwrap();

        let db = Database::memory()?;
        let res = sync_qodercn_usage(&db)?;
        assert_eq!(res.imported, 2);

        let conn = lock_conn!(db.conn);
        let turn1: (u32, u32, String) = conn.query_row(
            "SELECT input_tokens, output_tokens, model FROM proxy_request_logs WHERE request_id = 'qodercn:sess-turns:asst-turn-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(turn1, (10, 2, "deepseek-v4-pro".to_string()));

        let turn2: (u32, u32, String) = conn.query_row(
            "SELECT input_tokens, output_tokens, model FROM proxy_request_logs WHERE request_id = 'qodercn:sess-turns:asst-turn-2'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(turn2, (4, 6, "deepseek-v4-flash".to_string()));
        Ok(())
    }

    // ── Test 9: Placeholder provider resolves to QoderCN (Session) ──
    #[test]
    fn placeholder_provider_resolves_to_qodercn_display_name() -> Result<(), AppError> {
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
                    "display-qodercn-test",
                    PROVIDER_PLACEHOLDER,
                    APP_TYPE,
                    "deepseek-v4-pro",
                    "deepseek-v4-pro",
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
                p.provider_id == PROVIDER_PLACEHOLDER && p.provider_name == "QoderCN (Session)"
            }),
            "provider_placeholder 必须解析为 'QoderCN (Session)': {providers:?}"
        );
        Ok(())
    }

    // ── Test 10: QoderCN step is registered in sync_all_unlocked (Safeguard test) ──
    #[test]
    fn qodercn_step_is_registered_in_sync_all_unlocked() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let project_dir = temp.path().join("proj-cn-registered");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl_file = project_dir.join("sess.jsonl");

        let asst_line = serde_json::json!({
            "type": "assistant",
            "uuid": "uuid-cn-reg-01",
            "sessionId": "sess-cn-reg-01",
            "message": { "model": "dmodel", "content": "registered step test" }
        });
        fs::write(&jsonl_file, format!("{}\n", asst_line)).unwrap();

        let db = Database::memory()?;
        let result = crate::services::session_usage::sync_all_unlocked(&db);

        let count: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'qodercn_session'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 1, "sync_all_unlocked 必须执行 QoderCN 导入步骤");
        assert!(
            result.errors.iter().all(|e| !e.contains("QoderCN")),
            "QoderCN 步骤不应产生错误: {:?}",
            result.errors
        );
        Ok(())
    }

    // ── Test 11: Model mapping table coverage ──
    #[test]
    fn test_qodercn_model_mapping_coverage() {
        assert_eq!(map_qodercn_model("qmodel_38max"), "qwen3.8-max");
        assert_eq!(map_qodercn_model("qfmodel"), "qwen3.8-flash");
        assert_eq!(map_qodercn_model("qmodel_latest"), "qwen3.7-max");
        assert_eq!(map_qodercn_model("qmodel"), "qwen3.7-plus");
        assert_eq!(map_qodercn_model("q37fmodel"), "qwen3.6-flash");
        assert_eq!(map_qodercn_model("dmodel"), "deepseek-v4-pro");
        assert_eq!(map_qodercn_model("dfmodel"), "deepseek-v4-flash");
        assert_eq!(map_qodercn_model("gmodel"), "glm-5.3");
        assert_eq!(map_qodercn_model("gfmodel"), "glm-5.3-flash");
        assert_eq!(map_qodercn_model("gm51model"), "glm-5.2");
        assert_eq!(map_qodercn_model("kmodel_latest"), "kimi-k3");
        assert_eq!(map_qodercn_model("kmodel"), "kimi-k2.7-code");
        assert_eq!(map_qodercn_model("mmodel"), "minimax-m2.7");
        assert_eq!(map_qodercn_model("auto"), "qwen3.8-max");
        assert_eq!(map_qodercn_model(""), "qwen3.8-max");
        assert_eq!(map_qodercn_model("custom_model"), "qwen3.8-max");
    }

    // ── Test 12: IDE SQLite database sync ──
    #[test]
    fn test_sync_qodercn_ide_db() -> Result<(), AppError> {
        let temp = tempfile::tempdir().unwrap();
        let _guard = lock_qodercn_env(temp.path());
        let db_file = temp.path().join("local.db");

        // Create mock IDE sqlite database
        {
            let ide_conn = rusqlite::Connection::open(&db_file).unwrap();
            ide_conn
                .execute_batch(
                    "CREATE TABLE chat_session (
                        session_id VARCHAR(64) PRIMARY KEY,
                        preferred_model_info TEXT
                    );
                    CREATE TABLE chat_message (
                        id VARCHAR(64) PRIMARY KEY,
                        session_id VARCHAR(64),
                        model_info TEXT,
                        token_info TEXT,
                        gmt_create INTEGER
                    );
                    INSERT INTO chat_session (session_id, preferred_model_info)
                    VALUES ('s-01', '{\"preferred_model\":\"dmodel\"}');
                    INSERT INTO chat_message (id, session_id, model_info, token_info, gmt_create)
                    VALUES ('msg-01', 's-01', '{\"model_key\":\"qmodel_38max\"}', '{\"prompt_tokens\":1000,\"completion_tokens\":50,\"cached_tokens\":800}', 1781075253596);
                    INSERT INTO chat_message (id, session_id, model_info, token_info, gmt_create)
                    VALUES ('msg-02', 's-01', '{\"model_key\":\"auto\"}', '{\"prompt_tokens\":500,\"completion_tokens\":20,\"cached_tokens\":0}', 1781075254000);",
                )
                .unwrap();
        }

        let db = Database::memory()?;
        {
            let conn = lock_conn!(db.conn);
            conn.execute(
                "INSERT OR REPLACE INTO model_pricing (
                    model_id, display_name, input_cost_per_million, output_cost_per_million,
                    cache_read_cost_per_million, cache_creation_cost_per_million
                ) VALUES ('qwen3.8-max', 'Qwen3.8 Max', 2.0, 6.0, 0.25, 2.50)",
                [],
            )?;
            conn.execute(
                "INSERT OR REPLACE INTO model_pricing (
                    model_id, display_name, input_cost_per_million, output_cost_per_million,
                    cache_read_cost_per_million, cache_creation_cost_per_million
                ) VALUES ('deepseek-v4-pro', 'DeepSeek V4 Pro', 2.0, 8.0, 0.5, 1.0)",
                [],
            )?;
        }

        let res = sync_qodercn_ide_db(&db, &db_file)?;
        assert_eq!(res.imported, 2);
        assert_eq!(res.skipped, 0);

        // Verify row 1: qwen3.8-max & row 2
        {
            let conn = lock_conn!(db.conn);
            let row1: (String, u32, u32, u32, i64) = conn.query_row(
                "SELECT model, input_tokens, output_tokens, cache_read_tokens, input_token_semantics
                 FROM proxy_request_logs WHERE request_id = 'qodercn:ide:msg-01'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            assert_eq!(row1.0, "qwen3.8-max");
            assert_eq!(row1.1, 1000);
            assert_eq!(row1.2, 50);
            assert_eq!(row1.3, 800);
            assert_eq!(row1.4, INPUT_TOKEN_SEMANTICS_TOTAL);

            // Verify row 2: auto fallback to preferred_model "dmodel" -> "deepseek-v4-pro"
            let row2: (String, u32, u32) = conn.query_row(
                "SELECT model, input_tokens, output_tokens
                 FROM proxy_request_logs WHERE request_id = 'qodercn:ide:msg-02'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            assert_eq!(row2.0, "deepseek-v4-pro");
            assert_eq!(row2.1, 500);
            assert_eq!(row2.2, 20);
        }

        // Idempotency: second sync imports 0, skips 2
        let res2 = sync_qodercn_ide_db(&db, &db_file)?;
        assert_eq!(res2.imported, 0);
        assert_eq!(res2.skipped, 2);

        Ok(())
    }
}
