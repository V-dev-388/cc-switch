//! WorkBuddy (腾讯桌面 AI 助手) session usage importer — traces JSON reader.
//!
//! WorkBuddy v5.3.14+ no longer writes per-request token rows to the legacy
//! `com.tencent.mac.marvis/.../data.db`; instead it stores them as generation
//! spans inside `~/.workbuddy/traces/*/trace_*.json`. Each trace file has the
//! shape `{ trace: { traceId, sessionId, modelInfo, … }, spans: […] }` where
//! every span with `name == "generation"` carries a `toolOutput` JSON string
//! whose nested `usage` object holds `prompt_tokens`, `completion_tokens`,
//! `total_tokens`, and `prompt_tokens_details.cached_tokens`.
//!
//! The importer globs every trace file, parses the JSON, walks the spans, and
//! inserts only generation spans whose `toolOutput` yields a usage object.
//! Spans that are still `running` or whose `toolOutput` has no parseable usage
//! are counted as skipped. Idempotence is guaranteed by the shared
//! `session_usage_dedup` ledger keyed by `workbuddy:<traceId>:<spanId>`.
//!
//! Field decisions recorded with the task brief:
//! - `model` is extracted from `span.toolInput` via the regex
//!   `powered by ([A-Za-z0-9.-]+)` (≈97 % hit-rate); on miss it falls back to
//!   `trace.modelInfo.models[0]`, then `"unknown"`.
//! - `reasoning_tokens` (inside `completion_tokens_details`) is discarded — it
//!   is a subset of `completion_tokens` and the dashboard has no bucket for it.
//! - `cached_tokens` (from `prompt_tokens_details.cached_tokens`) maps to
//!   cache-read; `cache_creation` is always 0.
//! - `startedAt` carries a `Z` suffix and is parsed as UTC (the old database
//!   stored naive local time; the two sources are kept separate).
//! - Cost is computed locally via `find_model_pricing` + `CostCalculator`;
//!   rows without a matching pricing entry get zero cost.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::SessionSyncResult;
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL;
use crate::services::usage_stats::find_model_pricing;
use rust_decimal::Decimal;
use std::path::{Path, PathBuf};

const APP_TYPE: &str = "workbuddy";
const DATA_SOURCE: &str = "workbuddy_session";
/// Display-name placeholder surfaced by `usage_stats::provider_name_coalesce`.
/// WorkBuddy rows never carry a real provider_id, so every import uses this
/// synthetic key; the display-name mapping is exercised by the test below.
const PROVIDER_PLACEHOLDER: &str = "_workbuddy_session";
const REQUEST_ID_PREFIX: &str = "workbuddy:";
/// Trace files larger than 32 MB are almost certainly corrupt or truncated;
/// skip them and let the next sync pass try again.
const MAX_TRACE_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MIN_SQLITE_UNIX_SECONDS: i64 = -62_167_219_200;
const MAX_SQLITE_UNIX_SECONDS: i64 = 253_402_300_799;
const WORKBUDDY_REQUEST_DEDUP_SQL: &str = "SELECT EXISTS(
         SELECT 1 FROM session_usage_dedup
         WHERE data_source = ?1 AND request_id = ?2
     )";

/// One generation span mapped to the dashboard schema.
struct WorkBuddyUsageRecord {
    request_id: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    session_id: String,
    created_at: i64,
}

/// Parsed usage buckets from a `toolOutput` JSON string.
struct ExtractedUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
    cached_tokens: i64,
}

/// Import usage from every WorkBuddy trace file. A missing traces directory
/// simply means there is nothing to import yet.
pub fn sync_workbuddy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let traces_dir = workbuddy_traces_dir();
    Ok(sync_workbuddy_traces_dir(db, &traces_dir))
}

/// Resolve the WorkBuddy traces root. `WORKBUDDY_DATA_DIR` overrides the
/// `.workbuddy` base (tests use tempdirs instead); when unset the importer
/// reads `dirs::home_dir()/.workbuddy/traces`.
fn workbuddy_traces_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("WORKBUDDY_DATA_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("traces");
        }
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join(".workbuddy")
        .join("traces")
}

/// Path-injectable core: `traces_dir` is the directory whose `*/trace_*.json`
/// children should be imported. A missing or unreadable directory yields an
/// empty result without errors.
fn sync_workbuddy_traces_dir(db: &Database, traces_dir: &Path) -> SessionSyncResult {
    let mut result = SessionSyncResult::default();

    let trace_files = match collect_trace_files(traces_dir) {
        Ok(files) => files,
        Err(_) => {
            // The traces directory does not exist (or cannot be read). This is
            // the normal case when WorkBuddy has never been run on this machine.
            return result;
        }
    };

    result.files_scanned = trace_files.len().min(u32::MAX as usize) as u32;

    for file_path in &trace_files {
        match sync_single_trace_file(db, file_path) {
            Ok(per_file) => result.merge(per_file),
            Err(error) => {
                let message = format!("{}: {error}", file_path.display());
                log::warn!("[WORKBUDDY-SYNC] {message}");
                result.errors.push(message);
                result.deferred_files = result.deferred_files.saturating_add(1);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[WORKBUDDY-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
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
            AppError::Database(format!("启动 WorkBuddy 用量导入事务失败: {error}"))
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

            let record = WorkBuddyUsageRecord {
                request_id,
                model,
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                cache_read_tokens: usage.cached_tokens,
                session_id: session_id.to_string(),
                created_at: created_at.clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS),
            };

            let inserted = insert_workbuddy_record(&tx, &record)?;
            if inserted {
                result.imported = result.imported.saturating_add(1);
            } else {
                result.skipped = result.skipped.saturating_add(1);
            }
        }

        tx.commit().map_err(|error| {
            AppError::Database(format!("提交 WorkBuddy 用量导入事务失败: {error}"))
        })?;
    }

    if result.imported > 0 {
        log::info!(
            "[WORKBUDDY-SYNC] {}: 导入 {} 条, 跳过 {} 条",
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
            return Some(ExtractedUsage {
                prompt_tokens,
                completion_tokens,
                cached_tokens,
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

/// Extract the model name from `span.toolInput` using the regex
/// `powered by ([A-Za-z0-9.-]+)`. Falls back to the first entry in
/// `trace.modelInfo.models`, then to `"unknown"`.
fn extract_model(tool_input_str: &str, model_info_models: &[String]) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"powered by ([A-Za-z0-9.-]+)").expect("valid regex"));
    if let Some(caps) = re.captures(tool_input_str) {
        if let Some(m) = caps.get(1) {
            return m.as_str().to_string();
        }
    }
    if let Some(first) = model_info_models.first() {
        return first.clone();
    }
    "unknown".to_string()
}

/// Parse `startedAt` (e.g. `"2026-08-07T09:32:41.930Z"`) as UTC and return
/// unix seconds. Handles the `Z` suffix via RFC 3339, with a manual fallback
/// for fractional seconds.
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

/// Insert one WorkBuddy record into the dashboard. Returns `true` when a new
/// row was written, `false` when the ledger already had it.
fn insert_workbuddy_record(
    conn: &rusqlite::Connection,
    record: &WorkBuddyUsageRecord,
) -> Result<bool, AppError> {
    let already_seen: bool = conn
        .query_row(
            WORKBUDDY_REQUEST_DEDUP_SQL,
            rusqlite::params![DATA_SOURCE, record.request_id],
            |row| row.get(0),
        )
        .map_err(|error| AppError::Database(format!("查询 WorkBuddy 用量去重账本失败: {error}")))?;
    if already_seen {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup
         (data_source, request_id, semantic_id, has_entry_id)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![DATA_SOURCE, record.request_id, record.request_id, 1i64],
    )
    .map_err(|error| AppError::Database(format!("写入 WorkBuddy 用量去重账本失败: {error}")))?;

    let usage = TokenUsage {
        input_tokens: record.input_tokens.max(0).min(u32::MAX as i64) as u32,
        output_tokens: record.output_tokens.max(0).min(u32::MAX as i64) as u32,
        cache_read_tokens: record.cache_read_tokens.max(0).min(u32::MAX as i64) as u32,
        cache_creation_tokens: 0,
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
            0i64,
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
    .map_err(|error| AppError::Database(format!("插入 WorkBuddy 会话用量失败: {error}")))
}

#[cfg(test)]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a minimal trace JSON value with the given spans. Each span is a
    /// `(name, status, tool_input, tool_output)` tuple where `tool_input` and
    /// `tool_output` are already-serialised JSON strings (matching the real
    /// trace format where these fields are embedded strings, not nested
    /// objects).
    fn make_trace(
        trace_id: &str,
        session_id: &str,
        model_info_models: &[&str],
        spans: &[(&str, &str, &str, &str)],
    ) -> serde_json::Value {
        let spans_arr: Vec<serde_json::Value> = spans
            .iter()
            .enumerate()
            .map(|(i, (name, status, tool_input, tool_output))| {
                json!({
                    "spanId": format!("span_{i:04x}"),
                    "name": name,
                    "status": status,
                    "startedAt": "2026-08-05T21:39:44.890Z",
                    "toolInput": tool_input,
                    "toolOutput": tool_output,
                })
            })
            .collect();
        json!({
            "trace": {
                "traceId": trace_id,
                "sessionId": session_id,
                "modelInfo": {
                    "models": model_info_models,
                },
            },
            "spans": spans_arr,
        })
    }

    /// Standard toolInput containing "powered by Deepseek-V4-Flash".
    const TOOL_INPUT_POWERED: &str =
        r#"[{"content":"This conversation is powered by Deepseek-V4-Flash\nHello."}]"#;

    /// Standard toolOutput containing a usage object with prompt/completion/
    /// cached tokens.
    const TOOL_OUTPUT_WITH_USAGE: &str = r#"[
        {
            "id": "resp_001",
            "model": "deepseek-v4-flash",
            "object": "chat.completion",
            "choices": [],
            "usage": {
                "prompt_tokens": 15874,
                "completion_tokens": 101,
                "total_tokens": 15975,
                "prompt_tokens_details": {
                    "cached_tokens": 10368,
                    "reasoning_tokens": 0
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 21
                }
            }
        }
    ]"#;

    /// Write a trace file into `<base>/traces/<sub>/trace_<name>.json`.
    fn write_trace_file(base: &Path, sub: &str, name: &str, trace: &serde_json::Value) -> PathBuf {
        let dir = base.join("traces").join(sub);
        std::fs::create_dir_all(&dir).expect("mkdir trace dir");
        let path = dir.join(format!("trace_{name}.json"));
        std::fs::write(&path, trace.to_string()).expect("write trace file");
        path
    }

    /// Compute the expected UTC unix seconds for a startedAt string.
    fn expected_utc_unix_seconds(started_at: &str) -> i64 {
        parse_started_at_utc(started_at).expect("test timestamp parses")
    }

    // ── Test 1: span with usage is imported with per-column assertions ──

    #[test]
    fn imports_generation_span_with_usage_per_column_assertions() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let trace = make_trace(
            "trace_abc123",
            "sess-aaa",
            &["deepseek-v4-flash"],
            &[(
                "generation",
                "ok",
                TOOL_INPUT_POWERED,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "sess-dir-1", "abc123", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.imported, 1);
        assert_eq!(result.files_scanned, 1);
        assert!(result.errors.is_empty());

        let conn = lock_conn!(db.conn);
        let row: (
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
        ) = conn
            .query_row(
                "SELECT request_id, provider_id, app_type, model, request_model,
                        input_tokens, output_tokens, cache_read_tokens,
                        cache_creation_tokens, input_token_semantics, status_code,
                        session_id, data_source
                 FROM proxy_request_logs
                 WHERE request_id = 'workbuddy:trace_abc123:span_0000'",
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
                    ))
                },
            )
            .expect("read row");

        assert_eq!(row.0, "workbuddy:trace_abc123:span_0000");
        assert_eq!(row.1, "_workbuddy_session");
        assert_eq!(row.2, "workbuddy");
        assert_eq!(row.3, "Deepseek-V4-Flash");
        assert_eq!(row.4, "Deepseek-V4-Flash");
        assert_eq!((row.5, row.6, row.7, row.8), (15874, 101, 10368, 0));
        assert_eq!(row.9, INPUT_TOKEN_SEMANTICS_TOTAL);
        assert_eq!(row.10, 200);
        assert_eq!(row.11, "sess-aaa");
        assert_eq!(row.12, DATA_SOURCE);

        // Verify created_at matches UTC parsing of startedAt.
        let created_at: i64 = conn
            .query_row(
                "SELECT created_at FROM proxy_request_logs
                 WHERE request_id = 'workbuddy:trace_abc123:span_0000'",
                [],
                |row| row.get(0),
            )
            .expect("read created_at");
        assert_eq!(
            created_at,
            expected_utc_unix_seconds("2026-08-05T21:39:44.890Z")
        );

        // Verify the dedup ledger.
        let (req_id, semantic_id, has_entry): (String, String, i64) = conn
            .query_row(
                "SELECT request_id, semantic_id, has_entry_id FROM session_usage_dedup
                 WHERE data_source = 'workbuddy_session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read dedup row");
        assert_eq!(req_id, "workbuddy:trace_abc123:span_0000");
        assert_eq!(semantic_id, req_id);
        assert_eq!(has_entry, 1);
        Ok(())
    }

    // ── Test 2: second sync of same file is idempotent ──

    #[test]
    fn replay_is_idempotent_zero_new_imports() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let trace = make_trace(
            "trace_replay",
            "sess-replay",
            &["deepseek-v4-flash"],
            &[
                (
                    "generation",
                    "ok",
                    TOOL_INPUT_POWERED,
                    TOOL_OUTPUT_WITH_USAGE,
                ),
                (
                    "generation",
                    "ok",
                    TOOL_INPUT_POWERED,
                    TOOL_OUTPUT_WITH_USAGE,
                ),
            ],
        );
        write_trace_file(temp.path(), "replay-dir", "replay", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let first = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!((first.imported, first.skipped), (2, 0));

        let second = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(
            (second.imported, second.skipped),
            (0, 2),
            "二次同步必须全走台账 skipped，不得重复导入"
        );

        let count: i64 = lock_conn!(db.conn)
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'workbuddy_session'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 2);
        Ok(())
    }

    // ── Test 3: spans without usage / running status are skipped ──

    #[test]
    fn spans_without_usage_or_running_are_skipped_not_imported() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        // toolOutput without any usage object
        let tool_output_no_usage = r#"[{"id":"resp","choices":[],"object":"chat.completion"}]"#;
        // toolOutput that is empty string
        let trace = make_trace(
            "trace_skip",
            "sess-skip",
            &["deepseek-v4-flash"],
            &[
                // generation with no usage in toolOutput → skipped
                ("generation", "ok", TOOL_INPUT_POWERED, tool_output_no_usage),
                // generation with empty toolOutput → skipped
                ("generation", "running", TOOL_INPUT_POWERED, ""),
                // non-generation span → ignored entirely (not counted)
                ("mcp_tools", "ok", "", ""),
                // generation with valid usage → imported
                (
                    "generation",
                    "ok",
                    TOOL_INPUT_POWERED,
                    TOOL_OUTPUT_WITH_USAGE,
                ),
            ],
        );
        write_trace_file(temp.path(), "skip-dir", "skip", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(
            (result.imported, result.skipped),
            (1, 2),
            "无 usage 和 running 的 generation span 计 skipped，非 generation span 不计"
        );
        assert!(result.errors.is_empty());

        let count: i64 = lock_conn!(db.conn)
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'workbuddy_session'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 1, "仅一条有效 generation span 被导入");
        Ok(())
    }

    // ── Test 4: file > 32 MB is deferred without crashing ──

    #[test]
    fn oversized_trace_file_is_deferred_without_crashing() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let trace = make_trace(
            "trace_big",
            "sess-big",
            &["deepseek-v4-flash"],
            &[(
                "generation",
                "ok",
                TOOL_INPUT_POWERED,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        let path = write_trace_file(temp.path(), "big-dir", "big", &trace);

        // Pad the file to exceed 32 MB.
        let mut content = std::fs::read_to_string(&path).expect("read trace");
        let padding_needed = (MAX_TRACE_FILE_BYTES as usize) + 1 - content.len();
        if padding_needed > 0 {
            content.push_str(&" ".repeat(padding_needed));
        }
        std::fs::write(&path, &content).expect("write padded trace");

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.files_scanned, 1);
        assert_eq!(result.deferred_files, 1, "超 32MB 文件计 deferred");
        assert_eq!(result.imported, 0);
        assert_eq!(result.errors.len(), 1);
        Ok(())
    }

    // ── Test 5: model extraction failure falls to "unknown" ──

    #[test]
    fn model_extraction_failure_falls_to_unknown() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        // toolInput without "powered by X" and no modelInfo models
        let tool_input_no_powered = r#"[{"content":"Just a regular prompt"}]"#;
        let trace = make_trace(
            "trace_unknown",
            "sess-unknown",
            &[], // empty modelInfo models
            &[(
                "generation",
                "ok",
                tool_input_no_powered,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "unknown-dir", "unknown", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.imported, 1);

        let model: String = lock_conn!(db.conn)
            .query_row(
                "SELECT model FROM proxy_request_logs
                 WHERE request_id = 'workbuddy:trace_unknown:span_0000'",
                [],
                |row| row.get(0),
            )
            .expect("read model");
        assert_eq!(
            model, "unknown",
            "无 powered by 且无 modelInfo 时 model 落 unknown"
        );
        Ok(())
    }

    // ── Test 6: multiple files with multiple spans all import ──

    #[test]
    fn multiple_files_multiple_spans_all_imported() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");

        // File 1: two generation spans with usage
        let trace1 = make_trace(
            "trace_multi_1",
            "sess-multi-1",
            &["deepseek-v4-flash"],
            &[
                (
                    "generation",
                    "ok",
                    TOOL_INPUT_POWERED,
                    TOOL_OUTPUT_WITH_USAGE,
                ),
                (
                    "generation",
                    "ok",
                    TOOL_INPUT_POWERED,
                    TOOL_OUTPUT_WITH_USAGE,
                ),
            ],
        );
        write_trace_file(temp.path(), "multi-dir-1", "multi1", &trace1);

        // File 2: one generation span with usage, plus a non-generation span
        let trace2 = make_trace(
            "trace_multi_2",
            "sess-multi-2",
            &["deepseek-v4-flash"],
            &[
                ("cli", "ok", "", ""),
                (
                    "generation",
                    "ok",
                    TOOL_INPUT_POWERED,
                    TOOL_OUTPUT_WITH_USAGE,
                ),
            ],
        );
        write_trace_file(temp.path(), "multi-dir-2", "multi2", &trace2);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(
            result.imported, 3,
            "两个文件共 3 条 generation span 全部导入"
        );
        assert_eq!(result.files_scanned, 2);
        assert!(result.errors.is_empty());

        let count: i64 = lock_conn!(db.conn)
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'workbuddy_session'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 3);
        Ok(())
    }

    // ── Test 7: modelInfo fallback when toolInput lacks "powered by" ──

    #[test]
    fn model_falls_back_to_model_info_when_no_powered_by() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool_input_no_powered = r#"[{"content":"Just a regular prompt"}]"#;
        let trace = make_trace(
            "trace_fallback",
            "sess-fallback",
            &["deepseek-v4-pro-external"],
            &[(
                "generation",
                "ok",
                tool_input_no_powered,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "fallback-dir", "fallback", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.imported, 1);

        let model: String = lock_conn!(db.conn)
            .query_row(
                "SELECT model FROM proxy_request_logs
                 WHERE request_id = 'workbuddy:trace_fallback:span_0000'",
                [],
                |row| row.get(0),
            )
            .expect("read model");
        assert_eq!(
            model, "deepseek-v4-pro-external",
            "无 powered by 时退回 trace.modelInfo.models[0]"
        );
        Ok(())
    }

    // ── Test 8: missing traces directory yields zero files without error ──

    #[test]
    fn missing_traces_dir_reports_zero_files_without_error() {
        let db = Database::memory().expect("memory db");
        let temp = tempfile::tempdir().expect("tempdir");
        let traces_dir = temp.path().join("traces"); // does not exist
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());
        assert_eq!(result.deferred_files, 0);
    }

    // ── Test 9: empty traces directory yields zero files without error ──

    #[test]
    fn empty_traces_dir_yields_zero_files_without_error() {
        let db = Database::memory().expect("memory db");
        let temp = tempfile::tempdir().expect("tempdir");
        let traces_dir = temp.path().join("traces");
        std::fs::create_dir_all(&traces_dir).expect("mkdir traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());
    }

    // ── Test 10: reverse validation ② — usage field deleted → span skipped ──

    #[test]
    fn reverse_validation_usage_deleted_span_skipped_import_zero() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        // toolOutput with the usage object completely removed
        let tool_output_no_usage = r#"[{"id":"resp","model":"deepseek-v4-flash","choices":[],"object":"chat.completion"}]"#;
        let trace = make_trace(
            "trace_no_usage",
            "sess-no-usage",
            &["deepseek-v4-flash"],
            &[("generation", "ok", TOOL_INPUT_POWERED, tool_output_no_usage)],
        );
        write_trace_file(temp.path(), "no-usage-dir", "no_usage", &trace);

        let db = Database::memory().expect("memory db");
        let traces_dir = temp.path().join("traces");
        let result = sync_workbuddy_traces_dir(&db, &traces_dir);
        assert_eq!(result.imported, 0, "usage 字段被删除后导入数必须为 0");
        assert_eq!(result.skipped, 1, "该 span 计 skipped");
        assert!(result.errors.is_empty());

        let count: i64 = lock_conn!(db.conn)
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'workbuddy_session'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 0);
        Ok(())
    }

    // ── Test 11: WorkBuddy step is registered in sync_all_unlocked ──

    fn workbuddy_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// The WorkBuddy step must be wired into the shared sync kernel: driving
    /// `sync_all_unlocked` against a fixture traces root must land the row.
    /// This is the guard that turns "someone commented out the merge_sync_step"
    /// into a red test instead of silent data loss.
    #[test]
    #[allow(deprecated)] // set_var/remove_var deprecated since Rust 1.81; safe under mutex
    fn workbuddy_step_is_registered_in_sync_all_unlocked() -> Result<(), AppError> {
        let _guard = workbuddy_env_lock().lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let trace = make_trace(
            "trace_registered",
            "sess-registered",
            &["deepseek-v4-flash"],
            &[(
                "generation",
                "ok",
                TOOL_INPUT_POWERED,
                TOOL_OUTPUT_WITH_USAGE,
            )],
        );
        write_trace_file(temp.path(), "registered-dir", "registered", &trace);

        let original = std::env::var_os("WORKBUDDY_DATA_DIR");
        std::env::set_var("WORKBUDDY_DATA_DIR", temp.path());
        let db = Database::memory().expect("memory db");
        let result = crate::services::session_usage::sync_all_unlocked(&db);
        match original {
            Some(value) => std::env::set_var("WORKBUDDY_DATA_DIR", value),
            None => std::env::remove_var("WORKBUDDY_DATA_DIR"),
        }

        let count: i64 = lock_conn!(db.conn).query_row(
            "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'workbuddy_session'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 1, "sync_all_unlocked 必须执行 WorkBuddy 导入步骤");
        assert!(
            result
                .errors
                .iter()
                .all(|error| !error.contains("WorkBuddy")),
            "WorkBuddy 步骤不应产生错误: {:?}",
            result.errors
        );
        Ok(())
    }

    // ── Test 12: placeholder provider resolves to display name ──

    /// WorkBuddy rows carry the synthetic `_workbuddy_session` provider_id;
    /// `provider_name_coalesce` must resolve it to a readable display name so
    /// the dashboard's provider filter shows "WorkBuddy (Session)".
    #[test]
    fn placeholder_provider_resolves_to_workbuddy_session_display_name() -> Result<(), AppError> {
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
                    "display-name-test",
                    PROVIDER_PLACEHOLDER,
                    APP_TYPE,
                    "deepseek-v4-flash",
                    "deepseek-v4-flash",
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
                    && provider.provider_name == "WorkBuddy (Session)"
            }),
            "_workbuddy_session 必须解析为 'WorkBuddy (Session)': {providers:?}"
        );
        Ok(())
    }
}
