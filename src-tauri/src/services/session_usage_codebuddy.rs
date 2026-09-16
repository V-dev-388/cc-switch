//! CodeBuddy session usage importer — traces JSON reader.
//!
//! CodeBuddy stores session traces in `~/.codebuddy/traces/*/trace_*.json`.
//! Each trace file has the shape `{ trace: { traceId, sessionId, modelInfo, … }, spans: […] }`
//! where every span with `name == "generation"` carries a `toolOutput` JSON string
//! whose nested `usage` object holds `prompt_tokens`, `completion_tokens`,
//! `total_tokens`, and `prompt_tokens_details.cached_tokens`.
//!
//! The importer globs every trace file, parses the JSON, walks the spans, and
//! inserts only generation spans whose `toolOutput` yields a usage object.
//! Spans that are still running or whose `toolOutput` has no parseable usage
//! are counted as skipped. Idempotence is guaranteed by the shared
//! `session_usage_dedup` ledger keyed by `codebuddy:<traceId>:<spanId>`.
//!
//! Precise model extraction rules:
//! 1. Priority 1: `The exact model ID is ([A-Za-z0-9.-]+)` from `toolInput` (e.g. within `<codebuddy_background_info>`).
//! 2. Priority 2: `powered by ([A-Za-z0-9.-]+)` from `toolInput` (filtering out "the").
//! 3. Priority 3: `trace.modelInfo.models[0]`.
//! 4. Fallback: `"unknown"`.
//!
//! Trailing full stops (periods) are trimmed to prevent sentence punctuation from polluting the model name.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::SessionSyncResult;
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL;
use crate::services::usage_stats::find_model_pricing;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

const APP_TYPE: &str = "codebuddy";
const DATA_SOURCE: &str = "codebuddy_session";
const PROVIDER_PLACEHOLDER: &str = "_codebuddy_session";
const REQUEST_ID_PREFIX: &str = "codebuddy:";
/// Trace files larger than 32 MB are almost certainly corrupt or truncated;
/// skip them and let the next sync pass try again.
const MAX_TRACE_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MIN_SQLITE_UNIX_SECONDS: i64 = -62_167_219_200;
const MAX_SQLITE_UNIX_SECONDS: i64 = 253_402_300_799;
const CODEBUDDY_REQUEST_DEDUP_SQL: &str = "SELECT EXISTS(
         SELECT 1 FROM session_usage_dedup
         WHERE data_source = ?1 AND request_id = ?2
     )";

/// One generation span mapped to the dashboard schema.
struct CodeBuddyUsageRecord {
    request_id: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_creation_tokens: i64,
    session_id: String,
    created_at: i64,
}

/// Parsed usage buckets from a `toolOutput` JSON string.
struct ExtractedUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    cached_tokens: i64,
    cache_write_tokens: i64,
}

/// Import usage from CodeBuddy trace files and CodeBuddy CN IDE logs.
/// Missing directories simply mean there is nothing to import yet.
pub fn sync_codebuddy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let traces_dir = codebuddy_traces_dir();
    let mut result = sync_codebuddy_traces_dir(db, &traces_dir);

    let logs_dir = codebuddy_cn_logs_dir();
    let cn_result = sync_codebuddy_cn_logs_dir(db, &logs_dir);
    result.merge(cn_result);

    Ok(result)
}

/// Resolve the CodeBuddy traces root. `CODEBUDDY_DATA_DIR` overrides the
/// `.codebuddy` base (tests use tempdirs instead); when unset the importer
/// reads `dirs::home_dir()/.codebuddy/traces`.
fn codebuddy_traces_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("CODEBUDDY_DATA_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("traces");
        }
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".codebuddy")
        .join("traces")
}

/// Path-injectable core: `traces_dir` is the directory whose `*/trace_*.json`
/// children should be imported. A missing or unreadable directory yields an
/// empty result without errors.
pub(crate) fn sync_codebuddy_traces_dir(db: &Database, traces_dir: &Path) -> SessionSyncResult {
    let mut result = SessionSyncResult::default();

    let trace_files = match collect_trace_files(traces_dir) {
        Ok(files) => files,
        Err(_) => {
            // The traces directory does not exist (or cannot be read). This is
            // the normal case when CodeBuddy has never been run on this machine.
            return result;
        }
    };

    result.files_scanned = trace_files.len().min(u32::MAX as usize) as u32;

    for file_path in &trace_files {
        match sync_single_trace_file(db, file_path) {
            Ok(per_file) => result.merge(per_file),
            Err(error) => {
                let message = format!("{}: {error}", file_path.display());
                log::warn!("[CODEBUDDY-SYNC] {message}");
                result.errors.push(message);
                result.deferred_files = result.deferred_files.saturating_add(1);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[CODEBUDDY-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    result
}

/// Glob `traces_dir/*/trace_*.json` and return a sorted list. Returns an
/// error only when `traces_dir` itself cannot be read (missing directory is
/// surfaced as an error so the caller can treat it as "nothing to import").
fn collect_trace_files(traces_dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(traces_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let sub_dir = entry.path();
        for sub_entry in std::fs::read_dir(&sub_dir)? {
            let sub_entry = sub_entry?;
            let path = sub_entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("trace_") && name.ends_with(".json") {
                    files.push(path);
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Import one trace file. Read/parse failures defer to the next sync pass
/// rather than failing the whole run.
fn sync_single_trace_file(db: &Database, file_path: &Path) -> Result<SessionSyncResult, AppError> {
    let metadata = std::fs::metadata(file_path)
        .map_err(|e| AppError::Database(format!("读取 trace 文件元数据失败: {e}")))?;
    if metadata.len() > MAX_TRACE_FILE_BYTES {
        return Err(AppError::Database(format!(
            "trace 文件超过 32MB ({} bytes)，跳过",
            metadata.len()
        )));
    }

    let content = std::fs::read_to_string(file_path)
        .map_err(|e| AppError::Database(format!("读取 trace 文件失败: {e}")))?;
    let root: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| AppError::Database(format!("解析 trace JSON 失败: {e}")))?;

    let trace_obj = root.get("trace").and_then(|v| v.as_object());
    let trace_id = trace_obj
        .and_then(|t| t.get("traceId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_id = trace_obj
        .and_then(|t| t.get("sessionId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let model_info_models: Vec<String> = trace_obj
        .and_then(|t| t.get("modelInfo"))
        .and_then(|mi| mi.get("models"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut result = SessionSyncResult::default();

    let Some(spans) = root.get("spans").and_then(|v| v.as_array()) else {
        return Ok(result);
    };

    {
        let conn = lock_conn!(db.conn);
        let tx = conn.unchecked_transaction().map_err(|error| {
            AppError::Database(format!("启动 CodeBuddy 用量导入事务失败: {error}"))
        })?;

        for span in spans {
            let name = span.get("name").and_then(|v| v.as_str());
            if name != Some("generation") {
                continue;
            }

            let tool_output_str = span
                .get("toolOutput")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Some(usage) = extract_usage_from_tool_output(tool_output_str) else {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            };

            let tool_input_str = span.get("toolInput").and_then(|v| v.as_str()).unwrap_or("");
            let model = extract_model(tool_input_str, &model_info_models);

            let started_at = span.get("startedAt").and_then(|v| v.as_str()).unwrap_or("");
            let created_at = parse_started_at_utc(started_at).unwrap_or(0);

            let span_id = span.get("spanId").and_then(|v| v.as_str()).unwrap_or("");
            let request_id = format!("{REQUEST_ID_PREFIX}{trace_id}:{span_id}");

            let record = CodeBuddyUsageRecord {
                request_id,
                model,
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_tokens: usage.cached_tokens,
                cache_creation_tokens: usage.cache_write_tokens,
                session_id: session_id.to_string(),
                created_at: created_at.clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS),
            };

            let inserted = insert_codebuddy_record(&tx, &record)?;
            if inserted {
                result.imported = result.imported.saturating_add(1);
            } else {
                result.skipped = result.skipped.saturating_add(1);
            }
        }

        tx.commit().map_err(|error| {
            AppError::Database(format!("提交 CodeBuddy 用量导入事务失败: {error}"))
        })?;
    }

    if result.imported > 0 {
        log::info!(
            "[CODEBUDDY-SYNC] {}: 导入 {} 条, 跳过 {} 条",
            file_path.display(),
            result.imported,
            result.skipped
        );
    }
    Ok(result)
}

/// Resolve the CodeBuddy CN IDE logs root. `CODEBUDDY_LOGS_DIR` overrides the
/// base directory (tests use tempdirs instead); when unset the importer reads
/// `dirs::data_dir()/CodeBuddyExtension/Logs/CodeBuddyIDE`.
pub(crate) fn codebuddy_cn_logs_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("CODEBUDDY_LOGS_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::data_dir()
        .unwrap_or_default()
        .join("CodeBuddyExtension")
        .join("Logs")
        .join("CodeBuddyIDE")
}

/// Resolve the CodeBuddyExtension Data directory where history and messages are stored.
/// `CODEBUDDY_EXTENSION_DATA_DIR` overrides the base directory (for unit tests).
pub(crate) fn codebuddy_extension_data_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("CODEBUDDY_EXTENSION_DATA_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    dirs::data_dir()
        .unwrap_or_default()
        .join("CodeBuddyExtension")
        .join("Data")
}

/// Recursively index all message files `<msg_id>.json` under any `messages` folder in `data_dir`.
pub(crate) fn build_codebuddy_messages_index(data_dir: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    if data_dir.is_dir() {
        let _ = collect_messages_recursive(data_dir, 0, &mut map);
    }
    map
}

fn collect_messages_recursive(
    dir: &Path,
    depth: usize,
    map: &mut HashMap<String, PathBuf>,
) -> Result<(), std::io::Error> {
    if depth > 9 {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_messages_recursive(&path, depth + 1, map)?;
        } else if file_type.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("json") {
                    if let Some(parent) = path.parent() {
                        if parent.file_name().map_or(false, |n| n == "messages") {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                map.insert(stem.to_string(), path);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Extract assistant output text from a CodeBuddy `messages/<msg_id>.json` file
/// and estimate output tokens (~1.8 chars per token for Chinese + reasoning text).
pub(crate) fn extract_output_tokens_from_message_file(path: &Path) -> Option<i64> {
    let content = std::fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    let role = root.get("role").and_then(|r| r.as_str()).unwrap_or("");
    if role != "assistant" {
        return None;
    }
    let msg_val = root.get("message")?;
    let parsed_msg: serde_json::Value = if let Some(s) = msg_val.as_str() {
        serde_json::from_str(s).ok()?
    } else if msg_val.is_object() {
        msg_val.clone()
    } else {
        return None;
    };

    let char_count = count_content_chars(parsed_msg.get("content")?);
    if char_count == 0 {
        return None;
    }
    let tokens = ((char_count as f64) / 1.8).round() as i64;
    Some(tokens.max(1))
}

fn count_content_chars(val: &serde_json::Value) -> usize {
    if let Some(s) = val.as_str() {
        return s.chars().count();
    }
    if let Some(arr) = val.as_array() {
        let mut total = 0;
        for item in arr {
            if let Some(s) = item.as_str() {
                total += s.chars().count();
            } else if let Some(obj) = item.as_object() {
                for key in ["text", "reasoning", "thinking", "content"] {
                    if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                        total += s.chars().count();
                    }
                }
            }
        }
        return total;
    }
    0
}

/// Path-injectable core for CodeBuddy CN IDE logs.
/// Reads all `*.log` files recursively within `logs_dir`.
pub(crate) fn sync_codebuddy_cn_logs_dir(db: &Database, logs_dir: &Path) -> SessionSyncResult {
    let mut result = SessionSyncResult::default();

    let log_files = match collect_codebuddy_cn_log_files(logs_dir) {
        Ok(files) => files,
        Err(_) => {
            // Missing or unreadable logs directory means CodeBuddy CN has not been run.
            return result;
        }
    };

    result.files_scanned = log_files.len().min(u32::MAX as usize) as u32;

    let data_dir = codebuddy_extension_data_dir();
    let messages_index = build_codebuddy_messages_index(&data_dir);

    for file_path in &log_files {
        match sync_single_codebuddy_cn_log_file(db, file_path, &messages_index) {
            Ok(per_file) => result.merge(per_file),
            Err(error) => {
                let message = format!("{}: {error}", file_path.display());
                log::warn!("[CODEBUDDY-CN-SYNC] {message}");
                result.errors.push(message);
                result.deferred_files = result.deferred_files.saturating_add(1);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[CODEBUDDY-CN-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    result
}

/// Recursively collect `*.log` files within `logs_dir` up to 4 levels deep.
pub(crate) fn collect_codebuddy_cn_log_files(
    logs_dir: &Path,
) -> Result<Vec<PathBuf>, std::io::Error> {
    if !logs_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "CodeBuddy CN logs directory does not exist",
        ));
    }
    let mut files = Vec::new();
    collect_logs_recursive(logs_dir, 0, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_logs_recursive(
    dir: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
) -> Result<(), std::io::Error> {
    if depth > 4 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_logs_recursive(&path, depth + 1, files)?;
        } else if file_type.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext.eq_ignore_ascii_case("log") {
                    files.push(path);
                }
            }
        }
    }
    Ok(())
}

/// Import one CodeBuddy CN IDE log file.
fn sync_single_codebuddy_cn_log_file(
    db: &Database,
    file_path: &Path,
    messages_index: &HashMap<String, PathBuf>,
) -> Result<SessionSyncResult, AppError> {
    let metadata = std::fs::metadata(file_path)
        .map_err(|e| AppError::Database(format!("读取 CodeBuddy IDE 日志元数据失败: {e}")))?;
    if metadata.len() > MAX_TRACE_FILE_BYTES {
        return Err(AppError::Database(format!(
            "CodeBuddy IDE 日志超过 32MB ({} bytes)，跳过",
            metadata.len()
        )));
    }

    let file = std::fs::File::open(file_path)
        .map_err(|e| AppError::Database(format!("打开 CodeBuddy IDE 日志失败: {e}")))?;
    let reader = std::io::BufReader::new(file);

    let mut current_model: Option<String> = None;
    let mut current_trace_id: Option<String> = None;
    let mut current_conv_id: Option<String> = None;
    let mut req_to_trace: HashMap<String, String> = HashMap::new();
    let mut req_to_conv: HashMap<String, String> = HashMap::new();
    let mut req_to_model: HashMap<String, String> = HashMap::new();

    static RE_PREP_MODEL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_prep_model = RE_PREP_MODEL.get_or_init(|| {
        regex::Regex::new(r"(?:Preparing model|Model prepared):\s*([A-Za-z0-9.:_-]+)")
            .expect("valid regex")
    });

    static RE_MODEL_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_model_id = RE_MODEL_ID.get_or_init(|| {
        regex::Regex::new(r"modelId:\s*([A-Za-z0-9.:_-]+)").expect("valid regex")
    });

    static RE_AGENT_REP: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_agent_rep = RE_AGENT_REP.get_or_init(|| {
        regex::Regex::new(
            r"conversationId:\s*([a-zA-Z0-9_-]+),\s*requestId:\s*([a-zA-Z0-9_-]+),\s*traceId:\s*([a-zA-Z0-9_-]+)",
        )
        .expect("valid regex")
    });

    static RE_CHUNK_PERF: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_chunk_perf = RE_CHUNK_PERF.get_or_init(|| {
        regex::Regex::new(r"requestId=([a-zA-Z0-9_-]+),\s*conversationId=([a-zA-Z0-9_-]+)")
            .expect("valid regex")
    });

    static RE_BRACKET_TRACE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_bracket_trace = RE_BRACKET_TRACE.get_or_init(|| {
        regex::Regex::new(r"\[(?:CraftInvokableAgent|AgentReporter)\]\s*\[([a-zA-Z0-9_-]+)\]")
            .expect("valid regex")
    });

    static RE_STEP_NUM: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_step_num = RE_STEP_NUM
        .get_or_init(|| regex::Regex::new(r"step:\s*(\d+)").expect("valid regex"));

    static RE_STEP_REQ_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_step_req_id = RE_STEP_REQ_ID
        .get_or_init(|| regex::Regex::new(r"requestId:\s*([a-zA-Z0-9_-]+)").expect("valid regex"));

    static RE_STEP_MSG_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_step_msg_id = RE_STEP_MSG_ID
        .get_or_init(|| regex::Regex::new(r"messageId:\s*([a-zA-Z0-9_-]+)").expect("valid regex"));

    static RE_STEP_BRACKET_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_step_bracket_id = RE_STEP_BRACKET_ID.get_or_init(|| {
        regex::Regex::new(r"\[BaseAgent:[^\]]+\]\s*\[([a-zA-Z0-9_-]+)\]").expect("valid regex")
    });

    let mut result = SessionSyncResult::default();

    let conn = lock_conn!(db.conn);
    let tx = conn.unchecked_transaction().map_err(|error| {
        AppError::Database(format!("启动 CodeBuddy IDE 日志导入事务失败: {error}"))
    })?;

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };

        if line.is_empty() {
            continue;
        }

        // 1. Model detection
        if let Some(caps) = re_prep_model.captures(&line) {
            if let Some(m) = caps.get(1) {
                if let Some(cleaned) = clean_model_name(m.as_str()) {
                    current_model = Some(cleaned);
                }
            }
        }
        if let Some(caps) = re_model_id.captures(&line) {
            if let Some(m) = caps.get(1) {
                if let Some(cleaned) = clean_model_name(m.as_str()) {
                    current_model = Some(cleaned);
                }
            }
        }

        // 2. Trace and conversation mapping
        if let Some(caps) = re_agent_rep.captures(&line) {
            let conv_id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let req_id = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let trace_id = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            if !req_id.is_empty() {
                if !trace_id.is_empty() {
                    req_to_trace.insert(req_id.to_string(), trace_id.to_string());
                    current_trace_id = Some(trace_id.to_string());
                }
                if !conv_id.is_empty() {
                    req_to_conv.insert(req_id.to_string(), conv_id.to_string());
                    current_conv_id = Some(conv_id.to_string());
                }
                if let Some(ref mod_name) = current_model {
                    req_to_model.insert(req_id.to_string(), mod_name.clone());
                }
            }
        } else if let Some(caps) = re_chunk_perf.captures(&line) {
            let req_id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let conv_id = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            if !req_id.is_empty() && !conv_id.is_empty() {
                req_to_conv.insert(req_id.to_string(), conv_id.to_string());
                current_conv_id = Some(conv_id.to_string());
                if let Some(ref mod_name) = current_model {
                    req_to_model.insert(req_id.to_string(), mod_name.clone());
                }
            }
        }

        if let Some(caps) = re_bracket_trace.captures(&line) {
            if let Some(m) = caps.get(1) {
                current_trace_id = Some(m.as_str().to_string());
            }
        }

        // 3. Step usage import
        if line.contains("notifyStepEnd") {
            let Some(usage_start) = line.find("usage:") else {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            };
            let json_slice = &line[usage_start + 6..];
            let Some(open_brace) = json_slice.find('{') else {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            };
            let Some(close_brace) = json_slice[open_brace..].find('}') else {
                result.skipped = result.skipped.saturating_add(1);
                continue;
            };
            let usage_json_str = &json_slice[open_brace..=open_brace + close_brace];
            let usage_val: serde_json::Value = match serde_json::from_str(usage_json_str) {
                Ok(v) => v,
                Err(_) => {
                    result.skipped = result.skipped.saturating_add(1);
                    continue;
                }
            };

            let input_tokens = usage_val
                .get("inputTokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let mut output_tokens = usage_val
                .get("outputTokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_read_tokens = usage_val
                .get("cacheTokens")
                .or_else(|| usage_val.get("cachedTokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_creation_tokens = usage_val
                .get("cachedWriteTokens")
                .or_else(|| usage_val.get("cacheWriteTokens"))
                .or_else(|| usage_val.get("cacheCreationTokens"))
                .or_else(|| usage_val.get("cachedCreationTokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let step_num = re_step_num
                .captures(&line)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse::<u32>().ok())
                .unwrap_or(1);

            let req_id = re_step_req_id
                .captures(&line)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .unwrap_or("");

            let msg_id = re_step_msg_id
                .captures(&line)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .unwrap_or("");

            // If CodeBuddy's upstream stream omitted usage, CodeBuddy fell back to input-only
            // estimation and set outputTokens: 0. Recover the actual output tokens by reading
            // the assistant message content from the messages index.
            if output_tokens == 0 && !msg_id.is_empty() {
                if let Some(msg_path) = messages_index.get(msg_id) {
                    if let Some(est) = extract_output_tokens_from_message_file(msg_path) {
                        output_tokens = est;
                    }
                }
            }

            if input_tokens == 0 && output_tokens == 0 && cache_read_tokens == 0 && cache_creation_tokens == 0 {
                let total_tokens = usage_val
                    .get("totalTokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if total_tokens == 0 {
                    result.skipped = result.skipped.saturating_add(1);
                    continue;
                }
            }

            let bracket_id = re_step_bracket_id
                .captures(&line)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str())
                .unwrap_or("");

            let trace_id = if let Some(t) = req_to_trace.get(req_id) {
                t.as_str()
            } else if let Some(ref t) = current_trace_id {
                t.as_str()
            } else if !bracket_id.is_empty() {
                bracket_id
            } else {
                req_id
            };

            let session_id = if let Some(c) = req_to_conv.get(req_id) {
                c.clone()
            } else if let Some(ref c) = current_conv_id {
                c.clone()
            } else {
                trace_id.to_string()
            };

            let model = req_to_model
                .get(req_id)
                .cloned()
                .or_else(|| current_model.clone())
                .and_then(|m| clean_model_name(&m))
                .unwrap_or_else(|| "unknown".to_string());

            let span_id = if !msg_id.is_empty() {
                msg_id.to_string()
            } else {
                format!("step_{step_num}")
            };

            let request_id = format!("{REQUEST_ID_PREFIX}{trace_id}:{span_id}");
            let created_at = parse_log_timestamp(&line).unwrap_or(0);

            let record = CodeBuddyUsageRecord {
                request_id,
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                session_id,
                created_at: created_at.clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS),
            };

            let inserted = insert_codebuddy_record(&tx, &record)?;
            if inserted {
                result.imported = result.imported.saturating_add(1);
            } else {
                result.skipped = result.skipped.saturating_add(1);
            }
        }
    }

    tx.commit().map_err(|error| {
        AppError::Database(format!("提交 CodeBuddy IDE 日志导入事务失败: {error}"))
    })?;

    if result.imported > 0 {
        log::info!(
            "[CODEBUDDY-CN-SYNC] {}: 导入 {} 条, 跳过 {} 条",
            file_path.display(),
            result.imported,
            result.skipped
        );
    }

    Ok(result)
}

/// Parse `toolOutput` (a JSON string) and recursively search for the first
/// object that contains a `prompt_tokens` field. Returns `None` when the
/// string is empty, not valid JSON, or no usage object is found.
fn extract_usage_from_tool_output(tool_output_str: &str) -> Option<ExtractedUsage> {
    if tool_output_str.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(tool_output_str).ok()?;
    find_usage_object(&parsed)
}

/// Recursively walk a `serde_json::Value` tree looking for an object that has
/// a `prompt_tokens` key. The first match wins; nested arrays and objects are
/// both traversed depth-first.
fn find_usage_object(value: &serde_json::Value) -> Option<ExtractedUsage> {
    if let Some(obj) = value.as_object() {
        if let Some(prompt_tokens) = obj.get("prompt_tokens").and_then(|v| v.as_i64()) {
            let completion_tokens = obj
                .get("completion_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cached_tokens = obj
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_write_tokens = obj
                .get("prompt_tokens_details")
                .and_then(|d| {
                    d.get("cache_write_tokens")
                        .or_else(|| d.get("cache_creation_tokens"))
                        .or_else(|| d.get("cachedWriteTokens"))
                        .or_else(|| d.get("cacheCreationTokens"))
                })
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            return Some(ExtractedUsage {
                prompt_tokens,
                completion_tokens,
                cached_tokens,
                cache_write_tokens,
            });
        }
        for (_, v) in obj {
            if let Some(found) = find_usage_object(v) {
                return Some(found);
            }
        }
    }
    if let Some(arr) = value.as_array() {
        for v in arr {
            if let Some(found) = find_usage_object(v) {
                return Some(found);
            }
        }
    }
    None
}

/// Clean model name candidate: strips quotes, parentheses, trailing periods,
/// and `custom-local:` prefix. Discards "the", "unknown", or empty names.
pub(crate) fn clean_model_name(raw: &str) -> Option<String> {
    let mut model = raw.trim();
    model = model.trim_matches(|c| c == '\'' || c == '"' || c == '(' || c == ')' || c == ',');
    model = model.trim_end_matches('.');
    if let Some(stripped) = model.strip_prefix("custom-local:") {
        model = stripped;
    }
    let model = model.trim();
    if model.is_empty()
        || model.eq_ignore_ascii_case("the")
        || model.eq_ignore_ascii_case("unknown")
    {
        None
    } else {
        Some(model.to_string())
    }
}

/// Extract model with exact priority order:
/// 1. `The exact model ID is ([A-Za-z0-9.-]+)` from `toolInput`
/// 2. `powered by ([A-Za-z0-9.-]+)` from `toolInput` (discarding "the")
/// 3. `model_info_models[0]`
/// 4. `"unknown"`
///
/// Trailing full stops are trimmed to prevent sentence punctuation from sticking to model ID.
pub(crate) fn extract_model(tool_input_str: &str, model_info_models: &[String]) -> String {
    static RE_EXACT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_exact = RE_EXACT.get_or_init(|| {
        regex::Regex::new(r"The exact model ID is ([A-Za-z0-9.-]+)").expect("valid regex")
    });
    if let Some(caps) = re_exact.captures(tool_input_str) {
        if let Some(m) = caps.get(1) {
            if let Some(cleaned) = clean_model_name(m.as_str()) {
                return cleaned;
            }
        }
    }

    static RE_POWERED: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re_powered = RE_POWERED
        .get_or_init(|| regex::Regex::new(r"powered by ([A-Za-z0-9.-]+)").expect("valid regex"));
    if let Some(caps) = re_powered.captures(tool_input_str) {
        if let Some(m) = caps.get(1) {
            if let Some(cleaned) = clean_model_name(m.as_str()) {
                return cleaned;
            }
        }
    }

    if let Some(first) = model_info_models.first() {
        if let Some(cleaned) = clean_model_name(first) {
            return cleaned;
        }
    }

    "unknown".to_string()
}

/// Parse `[YYYY/M/D HH:MM:SS.mmm]` or `[YYYY/MM/DD HH:MM:SS.mmm]` as local time unix seconds.
pub(crate) fn parse_log_timestamp(line: &str) -> Option<i64> {
    if !line.starts_with('[') {
        return None;
    }
    let end_bracket = line.find(']')?;
    let ts_str = &line[1..end_bracket];
    let mut parts = ts_str.split_whitespace();
    let date_part = parts.next()?;
    let time_part = parts.next()?;

    let mut date_nums = date_part.split('/');
    let year: i32 = date_nums.next()?.parse().ok()?;
    let month: u32 = date_nums.next()?.parse().ok()?;
    let day: u32 = date_nums.next()?.parse().ok()?;

    let mut time_nums = time_part.split(':');
    let hour: u32 = time_nums.next()?.parse().ok()?;
    let minute: u32 = time_nums.next()?.parse().ok()?;
    let sec_part = time_nums.next()?;

    let (second, milli) = if let Some(dot) = sec_part.find('.') {
        let sec: u32 = sec_part[..dot].parse().ok()?;
        let milli_str = &sec_part[dot + 1..];
        let milli: u32 = milli_str.parse().ok()?;
        (sec, milli)
    } else {
        let sec: u32 = sec_part.parse().ok()?;
        (sec, 0)
    };

    let naive_date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let naive_dt = naive_date.and_hms_milli_opt(hour, minute, second, milli)?;
    use chrono::TimeZone;
    let ts = chrono::Local
        .from_local_datetime(&naive_dt)
        .single()
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| naive_dt.and_utc().timestamp());
    Some(ts)
}

/// Parse `startedAt` (e.g. `"2026-08-07T09:32:41.930Z"`) as UTC and return
/// unix seconds.
fn parse_started_at_utc(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.timestamp());
    }
    // Fallback for fractional-second variants that RFC 3339 rejects.
    chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.fZ")
        .ok()
        .map(|naive| naive.and_utc().timestamp())
}

/// Insert one CodeBuddy record into the dashboard. Returns `true` when a new
/// row was written, `false` when the ledger already had it.
fn insert_codebuddy_record(
    conn: &rusqlite::Connection,
    record: &CodeBuddyUsageRecord,
) -> Result<bool, AppError> {
    let already_seen: bool = conn
        .query_row(
            CODEBUDDY_REQUEST_DEDUP_SQL,
            rusqlite::params![DATA_SOURCE, record.request_id],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Database(format!("查询 CodeBuddy 用量去重账本失败: {error}")))?;
    if already_seen {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup
         (data_source, request_id, semantic_id, has_entry_id)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![DATA_SOURCE, record.request_id, record.request_id, 1i64],
    )
    .map_err(|error| AppError::Database(format!("写入 CodeBuddy 用量去重账本失败: {error}")))?;

    let usage = TokenUsage {
        input_tokens: record.input_tokens.max(0).min(u32::MAX as i64) as u32,
        output_tokens: record.output_tokens.max(0).min(u32::MAX as i64) as u32,
        cache_read_tokens: record.cache_read_tokens.max(0).min(u32::MAX as i64) as u32,
        cache_creation_tokens: record.cache_creation_tokens.max(0).min(u32::MAX as i64) as u32,
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
            PROVIDER_PLACEHOLDER,
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
    .map_err(|error| AppError::Database(format!("插入 CodeBuddy 会话用量失败: {error}")))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
mod tests {
    use super::*;
    use serde_json::json;

    static CODEBUDDY_ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    fn codebuddy_env_lock() -> &'static std::sync::Mutex<()> {
        CODEBUDDY_ENV_LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    fn make_trace(
        trace_id: &str,
        session_id: &str,
        model_info_models: &[&str],
        spans: &[(&str, &str, &str, &str)],
    ) -> serde_json::Value {
        let span_values: Vec<serde_json::Value> = spans
            .iter()
            .enumerate()
            .map(|(idx, (name, status, tool_input, tool_output))| {
                json!({
                    "traceId": trace_id,
                    "spanId": format!("span_{idx:04}"),
                    "name": name,
                    "type": "tool",
                    "startedAt": "2026-08-07T09:32:41.930Z",
                    "endedAt": "2026-08-07T09:32:43.100Z",
                    "duration": 1170,
                    "status": status,
                    "error": null,
                    "toolInput": tool_input,
                    "toolOutput": tool_output,
                })
            })
            .collect();

        json!({
            "trace": {
                "traceId": trace_id,
                "name": "Agent workflow",
                "workerPid": 93747,
                "startedAt": "2026-08-07T09:32:41.000Z",
                "endedAt": "2026-08-07T09:32:44.000Z",
                "duration": 3000,
                "status": "ok",
                "spanCount": spans.len(),
                "sessionId": session_id,
                "modelInfo": {
                    "models": model_info_models,
                }
            },
            "spans": span_values,
        })
    }

    const TOOL_INPUT_EXACT: &str =
        "{\"prompt\":\"hello <codebuddy_background_info>\\nThe exact model ID is hy3.\\n</codebuddy_background_info>\"}";
    const TOOL_INPUT_POWERED_THE: &str =
        "{\"prompt\":\"You are an interactive CLI tool powered by the assistant\"}";
    const TOOL_INPUT_POWERED_VALID: &str =
        "{\"prompt\":\"You are an assistant powered by deepseek-v4-flash\"}";

    const TOOL_OUTPUT_WITH_USAGE: &str = r#"[
        {
            "id": "resp_001",
            "model": "hy3",
            "object": "chat.completion",
            "choices": [],
            "usage": {
                "prompt_tokens": 15874,
                "completion_tokens": 101,
                "total_tokens": 15975,
                "prompt_tokens_details": {
                    "cached_tokens": 10368,
                    "cache_write_tokens": 512,
                    "reasoning_tokens": 0
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 21
                }
            }
        }
    ]"#;

    fn write_trace_file(base: &Path, sub: &str, name: &str, trace: &serde_json::Value) -> PathBuf {
        let dir = base.join("traces").join(sub);
        std::fs::create_dir_all(&dir).expect("mkdir trace dir");
        let path = dir.join(format!("trace_{name}.json"));
        std::fs::write(&path, trace.to_string()).expect("write trace file");
        path
    }

    // ── Test 1: span with usage is imported with per-column assertions ──

    #[test]
    fn imports_generation_span_with_usage_per_column_assertions() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let trace = make_trace(
            "trace_cb001",
            "sess-cb-100",
            &["hy3"],
            &[("generation", "ok", TOOL_INPUT_EXACT, TOOL_OUTPUT_WITH_USAGE)],
        );
        write_trace_file(temp.path(), "sess-dir-1", "cb001", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_codebuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.imported, 1);
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.skipped, 0);
        assert!(result.errors.is_empty());

        let conn = lock_conn!(db.conn);
        let row: (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            String,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT request_id, provider_id, app_type, model, request_model, pricing_model,
                        input_tokens, output_tokens, cache_read_tokens,
                        cache_creation_tokens, input_token_semantics, status_code,
                        session_id, data_source, created_at
                 FROM proxy_request_logs
                 WHERE request_id = 'codebuddy:trace_cb001:span_0000'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                    ))
                },
            )
            .expect("row must exist in proxy_request_logs");

        assert_eq!(
            row.0, "codebuddy:trace_cb001:span_0000",
            "request_id prefix and formatting"
        );
        assert_eq!(row.1, PROVIDER_PLACEHOLDER, "provider_id placeholder");
        assert_eq!(row.2, "codebuddy", "app_type");
        assert_eq!(
            row.3, "hy3",
            "model extracted from exact pattern without trailing period"
        );
        assert_eq!(row.4, "hy3", "request_model");
        assert_eq!(row.5, "hy3", "pricing_model");
        assert_eq!(row.6, 15874, "input_tokens prompt_tokens");
        assert_eq!(row.7, 101, "output_tokens completion_tokens");
        assert_eq!(row.8, 10368, "cache_read_tokens");
        assert_eq!(row.9, 512, "cache_creation_tokens must be 512");
        assert_eq!(row.10, 1, "input_token_semantics must be 1 (TOTAL)");
        assert_eq!(row.11, 200, "status_code must be 200");
        assert_eq!(row.12, "sess-cb-100", "session_id");
        assert_eq!(row.13, "codebuddy_session", "data_source");
        assert_eq!(
            row.14, 1786095161,
            "created_at UTC timestamp (2026-08-07T09:32:41.930Z)"
        );

        let dedup_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_usage_dedup WHERE data_source = 'codebuddy_session' AND request_id = 'codebuddy:trace_cb001:span_0000')",
            [],
            |r| r.get(0),
        ).expect("dedup query");
        assert!(
            dedup_exists,
            "session_usage_dedup ledger must record import"
        );

        Ok(())
    }

    // ── Test 2: replay is idempotent with zero new imports ──

    #[test]
    fn replay_is_idempotent_zero_new_imports() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let trace = make_trace(
            "trace_idempotent",
            "sess-idem",
            &["hy3"],
            &[("generation", "ok", TOOL_INPUT_EXACT, TOOL_OUTPUT_WITH_USAGE)],
        );
        write_trace_file(temp.path(), "sess-idem", "idem", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");

        let first = sync_codebuddy_traces_dir(&db, &traces_dir);
        assert_eq!(first.imported, 1, "first run imports exactly 1 row");
        assert_eq!(first.skipped, 0);

        let second = sync_codebuddy_traces_dir(&db, &traces_dir);
        assert_eq!(
            second.imported, 0,
            "second run must import 0 rows (idempotency)"
        );
        assert_eq!(
            second.skipped, 1,
            "second run marks existing row as skipped"
        );
        assert_eq!(second.files_scanned, 1);

        let total_rows: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'codebuddy'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            total_rows, 1,
            "database contains exactly 1 row after replay"
        );

        Ok(())
    }

    // ── Test 3: non-generation spans and spans without usage are skipped ──

    #[test]
    fn spans_without_usage_or_running_are_skipped_not_imported() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool_output_running = r#"[{"id":"resp_running","choices":[]}]"#;
        let trace = make_trace(
            "trace_skip_test",
            "sess-skip",
            &["hy3"],
            &[
                ("execution", "ok", "{}", "{}"),
                (
                    "generation",
                    "running",
                    TOOL_INPUT_EXACT,
                    tool_output_running,
                ),
                ("generation", "ok", TOOL_INPUT_EXACT, "not json at all"),
                ("generation", "ok", TOOL_INPUT_EXACT, TOOL_OUTPUT_WITH_USAGE),
            ],
        );
        write_trace_file(temp.path(), "sess-skip", "skip", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_codebuddy_traces_dir(&db, &traces_dir);

        assert_eq!(
            result.imported, 1,
            "only 1 valid generation span with usage is imported"
        );
        assert_eq!(
            result.skipped, 2,
            "2 generation spans without valid usage are skipped"
        );

        let count: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'codebuddy'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1);

        Ok(())
    }

    // ── Test 4: model extraction anti-the and exact recognition ──

    #[test]
    fn model_extraction_prioritizes_exact_model_and_rejects_the() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");

        // Case A: Exact model with period: "The exact model ID is deepseek-v4-pro."
        let input_a = "{\"prompt\":\"<codebuddy_background_info>\\nThe exact model ID is deepseek-v4-pro.\\n</codebuddy_background_info>\"}";
        let trace_a = make_trace(
            "trace_exact_dot",
            "sess-a",
            &["fallback-model"],
            &[("generation", "ok", input_a, TOOL_OUTPUT_WITH_USAGE)],
        );
        write_trace_file(temp.path(), "sub-a", "a", &trace_a);

        // Case B: Powered by "the" -> must reject "the" and fall back to modelInfo.models[0]
        let trace_b = make_trace(
            "trace_powered_the",
            "sess-b",
            &["glm-5.1"],
            &[(
                "generation",
                "ok",
                TOOL_INPUT_POWERED_THE,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "sub-b", "b", &trace_b);

        // Case C: Powered by valid model name
        let trace_c = make_trace(
            "trace_powered_valid",
            "sess-c",
            &["fallback-model"],
            &[(
                "generation",
                "ok",
                TOOL_INPUT_POWERED_VALID,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "sub-c", "c", &trace_c);

        // Case D: Powered by "The" (case-insensitive) and empty modelInfo -> falls back to "unknown"
        let trace_d = make_trace(
            "trace_powered_the_unknown",
            "sess-d",
            &[],
            &[(
                "generation",
                "ok",
                TOOL_INPUT_POWERED_THE,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "sub-d", "d", &trace_d);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_codebuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.imported, 4);

        let conn = lock_conn!(db.conn);

        // Verify strictly 0 rows have model = 'the'
        let the_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'codebuddy' AND (model = 'the' OR model = 'The')",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(the_count, 0, "model = 'the' count must strictly be 0");

        let model_a: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:trace_exact_dot:span_0000'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            model_a, "deepseek-v4-pro",
            "trailing dot trimmed from exact model"
        );

        let model_b: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:trace_powered_the:span_0000'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            model_b, "glm-5.1",
            "powered by the rejected, fallback to modelInfo"
        );

        let model_c: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:trace_powered_valid:span_0000'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            model_c, "deepseek-v4-flash",
            "powered by valid model name extracted"
        );

        let model_d: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:trace_powered_the_unknown:span_0000'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(
            model_d, "unknown",
            "powered by the with empty modelInfo falls back to unknown"
        );

        Ok(())
    }

    // ── Test 5: oversized trace file (>32MB) is deferred without crashing ──

    #[test]
    fn oversized_trace_file_is_deferred_without_crashing() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let sub = temp.path().join("traces").join("oversized");
        std::fs::create_dir_all(&sub).expect("create dir");
        let path = sub.join("trace_oversized.json");
        let file = std::fs::File::create(&path).expect("create file");
        file.set_len(33 * 1024 * 1024).expect("set oversized len");

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_codebuddy_traces_dir(&db, &traces_dir);

        assert_eq!(result.imported, 0);
        assert_eq!(result.files_scanned, 1);
        assert_eq!(
            result.deferred_files, 1,
            "oversized file must be counted as deferred"
        );
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("超过 32MB"));

        Ok(())
    }

    // ── Test 6: missing and empty traces directory handled gracefully ──

    #[test]
    fn missing_traces_dir_reports_zero_files_without_error() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let nonexistent = temp.path().join("traces_not_exist");
        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_traces_dir(&db, &nonexistent);

        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 0);
        assert!(
            result.errors.is_empty(),
            "missing dir should not produce error"
        );

        Ok(())
    }

    #[test]
    fn empty_traces_dir_yields_zero_files_without_error() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let empty_dir = temp.path().join("traces");
        std::fs::create_dir_all(&empty_dir).expect("create empty dir");
        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_traces_dir(&db, &empty_dir);

        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 0);
        assert!(result.errors.is_empty());

        Ok(())
    }

    // ── Test 7: CodeBuddy step is registered in sync_all_unlocked (Guard) ──

    #[test]
    #[allow(deprecated)]
    fn codebuddy_step_is_registered_in_sync_all_unlocked() -> Result<(), AppError> {
        let _guard = codebuddy_env_lock().lock().expect("env lock");
        let temp_cb = tempfile::tempdir().expect("tempdir cb");
        let temp_wb = tempfile::tempdir().expect("tempdir wb");
        let temp_cblogs = tempfile::tempdir().expect("tempdir cblogs");
        let trace = make_trace(
            "trace_registered",
            "sess-registered",
            &["hy3"],
            &[("generation", "ok", TOOL_INPUT_EXACT, TOOL_OUTPUT_WITH_USAGE)],
        );
        write_trace_file(temp_cb.path(), "registered-dir", "registered", &trace);

        let orig_cb = std::env::var_os("CODEBUDDY_DATA_DIR");
        let orig_wb = std::env::var_os("WORKBUDDY_DATA_DIR");
        let orig_cblogs = std::env::var_os("CODEBUDDY_LOGS_DIR");
        std::env::set_var("CODEBUDDY_DATA_DIR", temp_cb.path());
        std::env::set_var("WORKBUDDY_DATA_DIR", temp_wb.path());
        std::env::set_var("CODEBUDDY_LOGS_DIR", temp_cblogs.path());

        let db = Database::memory().expect("memory db");
        let result = crate::services::session_usage::sync_all_unlocked(&db);

        match orig_cb {
            Some(value) => std::env::set_var("CODEBUDDY_DATA_DIR", value),
            None => std::env::remove_var("CODEBUDDY_DATA_DIR"),
        }
        match orig_wb {
            Some(value) => std::env::set_var("WORKBUDDY_DATA_DIR", value),
            None => std::env::remove_var("WORKBUDDY_DATA_DIR"),
        }
        match orig_cblogs {
            Some(value) => std::env::set_var("CODEBUDDY_LOGS_DIR", value),
            None => std::env::remove_var("CODEBUDDY_LOGS_DIR"),
        }

        let count: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'codebuddy_session'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1, "sync_all_unlocked 必须执行 CodeBuddy 导入步骤");
        assert!(
            result
                .errors
                .iter()
                .all(|error| !error.contains("CodeBuddy")),
            "CodeBuddy 步骤不应产生错误: {:?}",
            result.errors
        );
        Ok(())
    }

    // ── Test 8: placeholder provider resolves to CodeBuddy display name ──

    #[test]
    fn placeholder_provider_resolves_to_codebuddy_display_name() -> Result<(), AppError> {
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
                    "display-name-test-codebuddy",
                    PROVIDER_PLACEHOLDER,
                    APP_TYPE,
                    "hy3",
                    "hy3",
                    10,
                    5,
                    0,
                    200,
                    1_787_362_690,
                    DATA_SOURCE,
                ],
            )?;
        }
        let providers = db.get_provider_stats(None, None, Some(APP_TYPE), None, None)?;
        assert!(
            providers.iter().any(|provider| {
                provider.provider_id == PROVIDER_PLACEHOLDER
                    && provider.provider_name == "CodeBuddy (Session)"
            }),
            "provider placeholder must resolve to CodeBuddy (Session): {providers:?}"
        );
        Ok(())
    }

    // ── Test 9: CodeBuddy CN log step is imported with per-column assertions ──

    #[test]
    fn codebuddy_cn_logs_imported_with_per_column_assertions() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let date_dir = temp.path().join("2026-09-14");
        std::fs::create_dir_all(&date_dir).expect("mkdir date dir");
        let log_path = date_dir.join("workspace__test01.log");

        let log_content = "\
[2026/9/14 14:16:25.216] [Info] [CraftInvokableAgent] [a98608e347b92d426390511bd3644fee]  Preparing model: glm-5.3 (custom-local:glm-5.3)
[2026/9/14 14:16:25.236] [Info] [AgentReporter] conversationId: conv_cb_cn_1, requestId: req_cb_cn_1, traceId: a98608e347b92d426390511bd3644fee
[2026/9/14 14:16:48.699] [Info] [BaseAgent:craft] [req_cb_cn_1]  notifyStepEnd, step: 1, requestId: req_cb_cn_1, messageId: msg_cb_cn_1, usage: {\"inputTokens\":9741,\"outputTokens\":120,\"totalTokens\":9861,\"cacheTokens\":256,\"cachedWriteTokens\":500,\"cachedMissTokens\":0,\"lastTokens\":9741,\"credit\":0,\"thinkingTokens\":0}, isMaxTokenLimit: false, isMaxStepLimit: false
";
        std::fs::write(&log_path, log_content).expect("write log file");

        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_cn_logs_dir(&db, temp.path());
        assert_eq!(result.imported, 1);
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.skipped, 0);
        assert!(result.errors.is_empty());

        let conn = lock_conn!(db.conn);
        let row: (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            String,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT request_id, provider_id, app_type, model, request_model, pricing_model,
                        input_tokens, output_tokens, cache_read_tokens,
                        cache_creation_tokens, input_token_semantics, status_code,
                        session_id, data_source, created_at
                 FROM proxy_request_logs
                 WHERE request_id = 'codebuddy:a98608e347b92d426390511bd3644fee:msg_cb_cn_1'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                    ))
                },
            )
            .expect("row must exist in proxy_request_logs");

        assert_eq!(
            row.0, "codebuddy:a98608e347b92d426390511bd3644fee:msg_cb_cn_1",
            "request_id prefix and formatting"
        );
        assert_eq!(row.1, PROVIDER_PLACEHOLDER, "provider_id placeholder");
        assert_eq!(row.2, "codebuddy", "app_type");
        assert_eq!(row.3, "glm-5.3", "model stripped of custom-local prefix");
        assert_eq!(row.4, "glm-5.3", "request_model");
        assert_eq!(row.5, "glm-5.3", "pricing_model");
        assert_eq!(row.6, 9741, "input_tokens");
        assert_eq!(row.7, 120, "output_tokens");
        assert_eq!(row.8, 256, "cache_read_tokens");
        assert_eq!(row.9, 500, "cache_creation_tokens must be 500");
        assert_eq!(row.10, 1, "input_token_semantics must be 1 (TOTAL)");
        assert_eq!(row.11, 200, "status_code must be 200");
        assert_eq!(row.12, "conv_cb_cn_1", "session_id");
        assert_eq!(row.13, "codebuddy_session", "data_source");
        assert!(row.14 > 0, "created_at timestamp must be valid");

        let dedup_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_usage_dedup WHERE data_source = 'codebuddy_session' AND request_id = 'codebuddy:a98608e347b92d426390511bd3644fee:msg_cb_cn_1')",
            [],
            |r| r.get(0),
        ).expect("dedup query");
        assert!(dedup_exists, "session_usage_dedup ledger must record CodeBuddy CN import");

        Ok(())
    }

    // ── Test 10: CodeBuddy CN model transition tracking and anti-the check ──

    #[test]
    fn codebuddy_cn_model_transition_and_anti_the() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let date_dir = temp.path().join("2026-09-14");
        std::fs::create_dir_all(&date_dir).expect("mkdir date dir");
        let log_path = date_dir.join("transition.log");

        let log_content = "\
[2026/9/14 14:16:25.216] [Info] [CraftInvokableAgent] [tr_1]  Preparing model: glm-5.3 (custom-local:glm-5.3)
[2026/9/14 14:16:25.236] [Info] [AgentReporter] conversationId: conv_1, requestId: req_1, traceId: tr_1
[2026/9/14 14:16:48.699] [Info] [BaseAgent:craft] [req_1]  notifyStepEnd, step: 1, requestId: req_1, messageId: msg_1, usage: {\"inputTokens\":1000,\"outputTokens\":100,\"totalTokens\":1100,\"cacheTokens\":0}
[2026/9/14 14:20:00.000] [Info] [CraftInvokableAgent] [tr_2]  Preparing model: the
[2026/9/14 14:20:05.000] [Info] [AgentReporter] conversationId: conv_2, requestId: req_2, traceId: tr_2
[2026/9/14 14:20:10.000] [Info] [BaseAgent:craft] [req_2]  notifyStepEnd, step: 1, requestId: req_2, messageId: msg_2, usage: {\"inputTokens\":2000,\"outputTokens\":200,\"totalTokens\":2200,\"cacheTokens\":0}
[2026/9/14 14:25:00.000] [Info] [CraftInvokableAgent] [tr_3]  Preparing model: Deepseek-V4.1-Flash (custom-local:Deepseek-V4.1-Flash)
[2026/9/14 14:25:05.000] [Info] [AgentReporter] conversationId: conv_3, requestId: req_3, traceId: tr_3
[2026/9/14 14:25:10.000] [Info] [BaseAgent:craft] [req_3]  notifyStepEnd, step: 1, requestId: req_3, messageId: msg_3, usage: {\"inputTokens\":3000,\"outputTokens\":300,\"totalTokens\":3300,\"cacheTokens\":0}
";
        std::fs::write(&log_path, log_content).expect("write log file");

        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_cn_logs_dir(&db, temp.path());
        assert_eq!(result.imported, 3, "all 3 steps imported");
        assert_eq!(result.skipped, 0);

        let conn = lock_conn!(db.conn);

        // Anti-"the" check: strict verification that model 'the' is 0
        let the_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'codebuddy' AND model = 'the'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(the_count, 0, "model 'the' must NEVER be imported");

        // Verify model names for each step
        let model_1: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:tr_1:msg_1'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(model_1, "glm-5.3");

        let model_2: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:tr_2:msg_2'",
            [],
            |r| r.get(0),
        )?;
        assert_ne!(model_2, "the", "model for step 2 must not be 'the'");
        assert_eq!(model_2, "glm-5.3", "falls back to previous valid model");

        let model_3: String = conn.query_row(
            "SELECT model FROM proxy_request_logs WHERE request_id = 'codebuddy:tr_3:msg_3'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(model_3, "Deepseek-V4.1-Flash");

        Ok(())
    }

    // ── Test 11: CodeBuddy CN replay is idempotent ──

    #[test]
    fn codebuddy_cn_replay_is_idempotent() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let date_dir = temp.path().join("2026-09-14");
        std::fs::create_dir_all(&date_dir).expect("mkdir date dir");
        let log_path = date_dir.join("idempotent.log");

        let log_content = "\
[2026/9/14 14:16:25.216] [Info] [CraftInvokableAgent] [tr_idem]  Preparing model: qwen3.8-max
[2026/9/14 14:16:25.236] [Info] [AgentReporter] conversationId: conv_idem, requestId: req_idem, traceId: tr_idem
[2026/9/14 14:16:48.699] [Info] [BaseAgent:craft] [req_idem]  notifyStepEnd, step: 1, requestId: req_idem, messageId: msg_idem, usage: {\"inputTokens\":500,\"outputTokens\":50,\"totalTokens\":550,\"cacheTokens\":0}
";
        std::fs::write(&log_path, log_content).expect("write log file");

        let db = Database::memory().expect("memory db");
        let first = sync_codebuddy_cn_logs_dir(&db, temp.path());
        assert_eq!(first.imported, 1, "first run imports exactly 1 row");
        assert_eq!(first.skipped, 0);

        let second = sync_codebuddy_cn_logs_dir(&db, temp.path());
        assert_eq!(second.imported, 0, "second run must import 0 rows");
        assert_eq!(second.skipped, 1, "second run marks existing row as skipped");

        let total_rows: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE app_type = 'codebuddy'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(total_rows, 1, "database contains exactly 1 row after replay");

        Ok(())
    }

    // ── Test 12: CodeBuddy CN corrupted lines skipped gracefully ──

    #[test]
    fn codebuddy_cn_corrupted_lines_skipped_gracefully() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let date_dir = temp.path().join("2026-09-14");
        std::fs::create_dir_all(&date_dir).expect("mkdir date dir");
        let log_path = date_dir.join("corrupt.log");

        let log_content = "\
[2026/9/14 14:16:25.216] [Info] [CraftInvokableAgent] [tr_c]  Preparing model: step-3.7-flash
[2026/9/14 14:16:25.236] [Info] [AgentReporter] conversationId: conv_c, requestId: req_c, traceId: tr_c
[2026/9/14 14:16:30.000] [Info] [BaseAgent:craft] [req_c]  notifyStepEnd, step: 1, requestId: req_c, messageId: msg_bad_json, usage: NOT_JSON
[2026/9/14 14:16:35.000] [Info] [BaseAgent:craft] [req_c]  notifyStepEnd, step: 2, requestId: req_c, messageId: msg_zero_tokens, usage: {\"inputTokens\":0,\"outputTokens\":0,\"totalTokens\":0,\"cacheTokens\":0}
[2026/9/14 14:16:40.000] [Info] [BaseAgent:craft] [req_c]  notifyStepEnd, step: 3, requestId: req_c, messageId: msg_valid, usage: {\"inputTokens\":1234,\"outputTokens\":56,\"totalTokens\":1290,\"cacheTokens\":0}
";
        std::fs::write(&log_path, log_content).expect("write log file");

        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_cn_logs_dir(&db, temp.path());
        assert_eq!(result.imported, 1, "only the 1 valid step with tokens is imported");
        assert_eq!(result.skipped, 2, "corrupt and zero-token steps are skipped");
        assert!(result.errors.is_empty(), "no crash or unhandled errors");

        let imported_req_id: String = lock_conn!(db.conn).query_row(
            "SELECT request_id FROM proxy_request_logs WHERE app_type = 'codebuddy'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(imported_req_id, "codebuddy:tr_c:msg_valid");

        Ok(())
    }

    // ── Test 13: sync_codebuddy_usage merges legacy traces and CodeBuddy CN IDE logs ──

    #[test]
    fn sync_codebuddy_usage_merges_traces_and_cn_logs() -> Result<(), AppError> {
        let _guard = codebuddy_env_lock().lock().expect("env lock");
        let temp_cb = tempfile::tempdir().expect("tempdir cb");
        let temp_cblogs = tempfile::tempdir().expect("tempdir cblogs");

        // 1. Write legacy trace
        let trace = make_trace(
            "trace_legacy_merge",
            "sess_legacy",
            &["hy3"],
            &[("generation", "ok", TOOL_INPUT_EXACT, TOOL_OUTPUT_WITH_USAGE)],
        );
        write_trace_file(temp_cb.path(), "legacy_dir", "merged", &trace);

        // 2. Write CodeBuddy CN IDE log
        let date_dir = temp_cblogs.path().join("2026-09-14");
        std::fs::create_dir_all(&date_dir).expect("mkdir date dir");
        let log_path = date_dir.join("cn_merged.log");
        let log_content = "\
[2026/9/14 14:16:25.216] [Info] [CraftInvokableAgent] [tr_cn_merged]  Preparing model: glm-5.3
[2026/9/14 14:16:25.236] [Info] [AgentReporter] conversationId: conv_cn, requestId: req_cn, traceId: tr_cn_merged
[2026/9/14 14:16:48.699] [Info] [BaseAgent:craft] [req_cn]  notifyStepEnd, step: 1, requestId: req_cn, messageId: msg_cn, usage: {\"inputTokens\":8888,\"outputTokens\":88,\"totalTokens\":8976,\"cacheTokens\":0}
";
        std::fs::write(&log_path, log_content).expect("write log file");

        let orig_cb = std::env::var_os("CODEBUDDY_DATA_DIR");
        let orig_cblogs = std::env::var_os("CODEBUDDY_LOGS_DIR");
        std::env::set_var("CODEBUDDY_DATA_DIR", temp_cb.path());
        std::env::set_var("CODEBUDDY_LOGS_DIR", temp_cblogs.path());

        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_usage(&db);

        match orig_cb {
            Some(value) => std::env::set_var("CODEBUDDY_DATA_DIR", value),
            None => std::env::remove_var("CODEBUDDY_DATA_DIR"),
        }
        match orig_cblogs {
            Some(value) => std::env::set_var("CODEBUDDY_LOGS_DIR", value),
            None => std::env::remove_var("CODEBUDDY_LOGS_DIR"),
        }

        let sync_res = result.expect("sync_codebuddy_usage must succeed");
        assert_eq!(sync_res.imported, 2, "both legacy trace and modern CN log must be imported");
        assert_eq!(sync_res.files_scanned, 2);
        assert!(sync_res.errors.is_empty());

        let conn = lock_conn!(db.conn);
        let rows: Vec<(String, String, i64)> = {
            let mut stmt = conn.prepare(
                "SELECT request_id, model, input_tokens FROM proxy_request_logs WHERE app_type = 'codebuddy' ORDER BY request_id",
            )?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            mapped.filter_map(Result::ok).collect()
        };

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "codebuddy:tr_cn_merged:msg_cn");
        assert_eq!(rows[0].1, "glm-5.3");
        assert_eq!(rows[0].2, 8888);

        assert_eq!(rows[1].0, "codebuddy:trace_legacy_merge:span_0000");
        assert_eq!(rows[1].1, "hy3");
        assert_eq!(rows[1].2, 15874);

        Ok(())
    }

    // ── Test 15: CodeBuddy CN output token recovery from message file ──
    #[test]
    fn codebuddy_cn_output_tokens_recovered_from_message_file() -> Result<(), AppError> {
        let _guard = codebuddy_env_lock().lock().expect("env lock");
        let temp_logs = tempfile::TempDir::new().expect("temp logs dir");
        let temp_data = tempfile::TempDir::new().expect("temp data dir");

        let msgs_dir = temp_data.path().join("history").join("ws").join("conv").join("messages");
        std::fs::create_dir_all(&msgs_dir).expect("create messages dir");
        let msg_file = msgs_dir.join("msg_zero_out.json");
        let sample_text = "这是一段用于测试输出Token智能补齐的中文回复文本。".repeat(9);
        let msg_json = serde_json::json!({
            "role": "assistant",
            "message": serde_json::json!({
                "role": "assistant",
                "content": [
                    {
                        "type": "reasoning",
                        "text": sample_text
                    }
                ]
            }).to_string()
        });
        std::fs::write(&msg_file, serde_json::to_string(&msg_json).unwrap()).expect("write msg");

        let log_file = temp_logs.path().join("test.log");
        let log_content = "
[2026/9/15 15:38:17.382] [Info] [ModelManager]  OpenAI Compatible ModelProvider initialized for custom model, modelId: custom-local:glm-5.3, modelName: glm-5.3
[2026/9/15 15:40:03.288] [Info] [BaseAgent:craft] [req_test]  notifyStepEnd, step: 1, requestId: req_test, messageId: msg_zero_out, usage: {\"inputTokens\":5000,\"outputTokens\":0,\"totalTokens\":5000,\"cacheTokens\":0}
";
        std::fs::write(&log_file, log_content).expect("write log");

        let orig_data = std::env::var_os("CODEBUDDY_EXTENSION_DATA_DIR");
        std::env::set_var("CODEBUDDY_EXTENSION_DATA_DIR", temp_data.path());

        let db = Database::memory().expect("memory db");
        let result = sync_codebuddy_cn_logs_dir(&db, temp_logs.path());

        match orig_data {
            Some(v) => std::env::set_var("CODEBUDDY_EXTENSION_DATA_DIR", v),
            None => std::env::remove_var("CODEBUDDY_EXTENSION_DATA_DIR"),
        }

        assert_eq!(result.imported, 1);
        let conn = lock_conn!(db.conn);
        let row: (i64, i64) = conn.query_row(
            "SELECT input_tokens, output_tokens FROM proxy_request_logs WHERE request_id = 'codebuddy:req_test:msg_zero_out'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        assert_eq!(row.0, 5000);
        assert!(row.1 > 0, "output_tokens must be recovered from message file, got {}", row.1);

        Ok(())
    }
}
