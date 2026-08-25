//! WorkBuddy (腾讯桌面 AI 助手) session usage importer.
//!
//! WorkBuddy records per-request token accounting in the `llm_token_usage`
//! table of every `data.db` under
//! `~/Library/Application Support/com.tencent.mac.marvis/MarvisData/User/<user-dir>/database/`.
//! Each row already carries the final token buckets, so the importer is a
//! straight table-to-table copy gated by the shared `session_usage_dedup`
//! ledger keyed by `workbuddy:<user-dir>:<id>`. The source database runs in
//! WAL mode (its main-file mtime does not advance on every write), so — same
//! as the ZCode importer — we skip mtime fast-paths entirely and rely on full
//! reparses plus the ledger for idempotence.
//!
//! Field decisions recorded with the task brief:
//! - `model_id` is stored verbatim (`main-auto` etc. are internal routing
//!   names; `metadata` is always `{}` and cannot recover a real model id).
//! - `thinking_tokens` has no dashboard bucket; full-table checks show
//!   `total_tokens = input_tokens + output_tokens`, so thinking is already
//!   inside output and is NOT stored separately.
//! - `created_at` carries no timezone and is interpreted as local time.
//! - `cached_tokens` maps to cache reads under total-input semantics (the
//!   `total = input + output` identity only holds when the cached portion is
//!   part of input), mirrored by adding `workbuddy` to both sides of
//!   `CACHE_INCLUSIVE_APP_TYPES`.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::SessionSyncResult;
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL;
use crate::services::usage_stats::find_model_pricing;
use chrono::{Local, TimeZone};
use rust_decimal::Decimal;
use std::path::{Path, PathBuf};

const APP_TYPE: &str = "workbuddy";
const DATA_SOURCE: &str = "workbuddy_session";
/// Display-name placeholder surfaced by `usage_stats::provider_name_coalesce`.
/// WorkBuddy rows never carry a real provider_id, so every import uses this
/// synthetic key; the display-name mapping is exercised by the test below.
const PROVIDER_PLACEHOLDER: &str = "_workbuddy_session";
const REQUEST_ID_PREFIX: &str = "workbuddy:";
const MIN_SQLITE_UNIX_SECONDS: i64 = -62_167_219_200;
const MAX_SQLITE_UNIX_SECONDS: i64 = 253_402_300_799;
const WORKBUDDY_REQUEST_DEDUP_SQL: &str = "SELECT EXISTS(
         SELECT 1 FROM session_usage_dedup
         WHERE data_source = ?1 AND request_id = ?2
     )";
const CREATED_AT_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.f";
const USAGE_DATE_FORMAT: &str = "%Y-%m-%d";

/// One `llm_token_usage` row mapped to the dashboard schema. Rows whose
/// timestamps cannot be resolved at all are dropped during loading and only
/// counted as skipped.
struct WorkBuddyUsageRecord {
    request_id: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cached_tokens: i64,
    session_id: String,
    created_at: i64,
}

/// A loaded source row that either produced a [`WorkBuddyUsageRecord`] or was
/// rejected (unparseable timestamps) and must be counted as skipped.
enum LoadedRecord {
    Record(Box<WorkBuddyUsageRecord>),
    Unusable,
}

/// Import usage from every WorkBuddy user database. A missing Marvis data
/// directory simply means there is nothing to import yet.
pub fn sync_workbuddy_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let roots = workbuddy_user_roots();
    Ok(sync_workbuddy_user_roots(db, &roots))
}

/// Resolve every WorkBuddy per-user database path. `WORKBUDDY_DATA_DIR`
/// overrides the MarvisData root (tests use tempdirs instead); missing or
/// unreadable directories yield an empty list without errors.
fn workbuddy_user_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let users_dir = match workbuddy_users_dir() {
        Some(dir) => dir,
        None => return roots,
    };
    let Ok(entries) = std::fs::read_dir(&users_dir) else {
        return roots;
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join("database").join("data.db");
        if candidate.is_file() {
            roots.push(entry.path());
        }
    }
    roots.sort();
    roots
}

fn workbuddy_users_dir() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("WORKBUDDY_DATA_DIR") {
        let trimmed = custom.to_string_lossy().trim().to_string();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    Some(
        dirs::config_dir()?
            .join("com.tencent.mac.marvis")
            .join("MarvisData")
            .join("User"),
    )
}

/// Path-injectable core: each root is one `<MarvisData>/User/<name>` directory
/// whose `database/data.db` should be imported. The directory name namespaces
/// request ids so identical row ids across user databases cannot swallow each
/// other.
fn sync_workbuddy_user_roots(db: &Database, roots: &[PathBuf]) -> SessionSyncResult {
    let mut result = SessionSyncResult {
        files_scanned: roots.len().min(u32::MAX as usize) as u32,
        ..Default::default()
    };
    for root in roots {
        match sync_single_workbuddy_db(db, root) {
            Ok(per_db) => result.merge(per_db),
            Err(error) => {
                let message = format!("{}: {error}", root.display());
                log::warn!("[WORKBUDDY-SYNC] {message}");
                result.errors.push(message);
                result.deferred_files = result.deferred_files.saturating_add(1);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[WORKBUDDY-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个库",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }
    result
}

/// Import one user database. Open/read failures defer to the next sync pass
/// rather than failing the whole run.
fn sync_single_workbuddy_db(
    db: &Database,
    user_root: &Path,
) -> Result<SessionSyncResult, AppError> {
    let db_path = user_root.join("database").join("data.db");
    let user_name = user_root
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();

    let conn = open_workbuddy_db_readonly(&db_path)?;
    let loaded = load_workbuddy_records(&conn, &user_name)?;

    let mut result = SessionSyncResult::default();
    {
        let conn = lock_conn!(db.conn);
        let tx = conn.unchecked_transaction().map_err(|error| {
            AppError::Database(format!("启动 WorkBuddy 用量导入事务失败: {error}"))
        })?;
        for item in &loaded {
            let inserted = match item {
                LoadedRecord::Record(record) => insert_workbuddy_record(&tx, record)?,
                LoadedRecord::Unusable => false,
            };
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
            user_name,
            result.imported,
            result.skipped
        );
    }
    Ok(result)
}

/// Open the WorkBuddy database read-only with a busy timeout so a concurrent
/// WorkBuddy writer never blocks us indefinitely. The source database is never
/// written to.
fn open_workbuddy_db_readonly(path: &Path) -> Result<rusqlite::Connection, AppError> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| AppError::Database(format!("无法打开 WorkBuddy 数据库: {error}")))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(|error| {
            AppError::Database(format!("设置 WorkBuddy busy_timeout 失败: {error}"))
        })?;
    Ok(conn)
}

/// Read every `llm_token_usage` row. A missing table defers to the next sync
/// pass — WorkBuddy upgrades may reshape the schema between releases.
fn load_workbuddy_records(
    conn: &rusqlite::Connection,
    user_name: &str,
) -> Result<Vec<LoadedRecord>, AppError> {
    let table_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'llm_token_usage')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| {
            AppError::Database(format!("查询 WorkBuddy llm_token_usage 表存在性失败: {error}"))
        })?;
    if !table_exists {
        log::info!("[WORKBUDDY-SYNC] llm_token_usage 表不存在，跳过该库");
        return Ok(Vec::new());
    }

    let mut stmt = conn
        .prepare(
            "SELECT id, usage_date, conversation_id, model_id,
                    input_tokens, output_tokens, cached_tokens, created_at
             FROM llm_token_usage ORDER BY id",
        )
        .map_err(|error| {
            AppError::Database(format!("准备 WorkBuddy llm_token_usage 查询失败: {error}"))
        })?;

    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let usage_date: String = row.get(1)?;
            let conversation_id: String = row.get(2)?;
            let model_id: String = row.get(3)?;
            let input_tokens: i64 = row.get(4)?;
            let output_tokens: i64 = row.get(5)?;
            let cached_tokens: i64 = row.get(6)?;
            let created_at: String = row.get(7)?;

            // created_at is naive local time with microseconds; fall back to
            // the usage_date midnight before giving up on the row entirely.
            let timestamp =
                parse_created_at(&created_at).or_else(|| parse_usage_date_midnight(&usage_date));

            Ok((
                id,
                conversation_id,
                model_id,
                input_tokens,
                output_tokens,
                cached_tokens,
                timestamp,
            ))
        })
        .map_err(|error| {
            AppError::Database(format!("读取 WorkBuddy llm_token_usage 行失败: {error}"))
        })?;

    let mut loaded = Vec::new();
    for row in rows {
        let (id, conversation_id, model_id, input_tokens, output_tokens, cached_tokens, timestamp) =
            row.map_err(|error| {
                AppError::Database(format!("解析 WorkBuddy llm_token_usage 行失败: {error}"))
            })?;
        let Some(created_at) = timestamp else {
            log::warn!("[WORKBUDDY-SYNC] 行 {id} 的 created_at/usage_date 均无法解析，跳过");
            loaded.push(LoadedRecord::Unusable);
            continue;
        };
        loaded.push(LoadedRecord::Record(Box::new(WorkBuddyUsageRecord {
            request_id: format!("{REQUEST_ID_PREFIX}{user_name}:{id}"),
            model: model_id,
            input_tokens,
            output_tokens,
            cached_tokens,
            session_id: conversation_id,
            created_at: created_at.clamp(MIN_SQLITE_UNIX_SECONDS, MAX_SQLITE_UNIX_SECONDS),
        })));
    }
    Ok(loaded)
}

/// Parse the naive-local `created_at` ("2026-06-07T11:30:27.003343") into unix
/// seconds using this machine's timezone.
fn parse_created_at(value: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(value.trim(), CREATED_AT_FORMAT).ok()?;
    naive_local_to_unix_seconds(naive)
}

/// Parse `usage_date` ("2026-06-07") into local-midnight unix seconds.
fn parse_usage_date_midnight(value: &str) -> Option<i64> {
    let date = chrono::NaiveDate::parse_from_str(value.trim(), USAGE_DATE_FORMAT).ok()?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    naive_local_to_unix_seconds(midnight)
}

fn naive_local_to_unix_seconds(naive: chrono::NaiveDateTime) -> Option<i64> {
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        // DST fold: both instants share the local wall clock; pick the earlier.
        chrono::LocalResult::Ambiguous(earliest, _) => Some(earliest.timestamp()),
        chrono::LocalResult::None => None,
    }
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
        cache_read_tokens: record.cached_tokens.max(0).min(u32::MAX as i64) as u32,
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
            record.cached_tokens,
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

    /// Build a throwaway WorkBuddy fixture database with the real
    /// `llm_token_usage` schema so the importer exercises the same SQL it sees
    /// in production. Fixtures live strictly in tempdirs; the real user
    /// database is never touched.
    fn create_fixture_db(user_root: &Path) -> PathBuf {
        let db_path = user_root.join("database").join("data.db");
        std::fs::create_dir_all(db_path.parent().expect("database parent")).expect("mkdir");
        let conn = rusqlite::Connection::open(&db_path).expect("open fixture db");
        conn.execute_batch(
            "CREATE TABLE llm_token_usage (
                id               INTEGER PRIMARY KEY AUTOINCREMENT,
                usage_date       TEXT    NOT NULL,
                conversation_id  TEXT    NOT NULL,
                response_id      TEXT,
                model_id         TEXT    NOT NULL,
                is_local         INTEGER NOT NULL DEFAULT 0,
                input_tokens     INTEGER NOT NULL DEFAULT 0,
                output_tokens    INTEGER NOT NULL DEFAULT 0,
                thinking_tokens  INTEGER NOT NULL DEFAULT 0,
                cached_tokens    INTEGER NOT NULL DEFAULT 0,
                total_tokens     INTEGER NOT NULL DEFAULT 0,
                created_at       TEXT    NOT NULL,
                metadata         TEXT    DEFAULT '{}'
             );",
        )
        .expect("create fixture schema");
        db_path
    }

    fn insert_usage_row(
        conn: &rusqlite::Connection,
        usage_date: &str,
        conversation_id: &str,
        response_id: Option<&str>,
        model_id: &str,
        input: i64,
        output: i64,
        thinking: i64,
        cached: i64,
        created_at: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO llm_token_usage (
                usage_date, conversation_id, response_id, model_id,
                input_tokens, output_tokens, thinking_tokens, cached_tokens,
                total_tokens, created_at, metadata
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '{}')",
            rusqlite::params![
                usage_date,
                conversation_id,
                response_id,
                model_id,
                input,
                output,
                thinking,
                cached,
                input + output,
                created_at,
            ],
        )
        .expect("insert llm_token_usage row");
        conn.last_insert_rowid()
    }

    fn expected_local_unix_seconds(created_at: &str) -> i64 {
        parse_created_at(created_at).expect("test timestamp parses")
    }

    #[test]
    fn imports_rows_with_per_column_mapping_and_total_semantics() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let user_root = temp.path().join("oAN1i2ewwfUIfsu_cPifwsiPGqNg");
        create_fixture_db(&user_root);
        let db_path = user_root.join("database").join("data.db");
        let created_a = "2026-06-07T11:30:27.003343";
        let created_b = "2026-08-05T21:39:44.890636";
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open fixture");
            let id1 = insert_usage_row(
                &conn,
                "2026-06-07",
                "conv_alpha",
                Some("resp_dup"),
                "main-auto",
                15874,
                101,
                21,
                10368,
                created_a,
            );
            assert_eq!(id1, 1);
            // Same response_id repeats across rows — it must not deduplicate.
            insert_usage_row(
                &conn,
                "2026-08-05",
                "conv_beta",
                Some("resp_dup"),
                "deepseek-v4-pro-external",
                928,
                240,
                57,
                512,
                created_b,
            );
        }

        let db = Database::memory().expect("memory db");
        let result = sync_workbuddy_user_roots(&db, &[user_root]);
        assert_eq!(result.imported, 2);
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
            i64,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT request_id, provider_id, app_type, model, request_model,
                        input_tokens, output_tokens, cache_read_tokens,
                        cache_creation_tokens, input_token_semantics, status_code,
                        created_at, session_id, data_source
                 FROM proxy_request_logs WHERE request_id = 'workbuddy:oAN1i2ewwfUIfsu_cPifwsiPGqNg:1'",
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
                    ))
                },
            )
            .expect("read row");

        assert_eq!(row.0, "workbuddy:oAN1i2ewwfUIfsu_cPifwsiPGqNg:1");
        assert_eq!(row.1, "_workbuddy_session");
        assert_eq!(row.2, "workbuddy");
        assert_eq!(row.3, "main-auto");
        assert_eq!(row.4, "main-auto");
        assert_eq!((row.5, row.6, row.7, row.8), (15874, 101, 10368, 0));
        assert_eq!(row.9, INPUT_TOKEN_SEMANTICS_TOTAL);
        assert_eq!(row.10, 200);
        assert_eq!(row.11, expected_local_unix_seconds(created_a));
        assert_eq!(row.12, "conv_alpha");
        assert_eq!(row.13, DATA_SOURCE);

        let second_ts: i64 = conn
            .query_row(
                "SELECT created_at FROM proxy_request_logs
                 WHERE request_id = 'workbuddy:oAN1i2ewwfUIfsu_cPifwsiPGqNg:2'",
                [],
                |row| row.get(0),
            )
            .expect("read second row ts");
        assert_eq!(second_ts, expected_local_unix_seconds(created_b));

        let mut stmt = conn
            .prepare(
                "SELECT request_id, semantic_id FROM session_usage_dedup
                 WHERE data_source = 'workbuddy_session' ORDER BY request_id",
            )
            .expect("prepare");
        let dedup_rows: Vec<(String, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query dedup")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect dedup");
        drop(stmt);
        assert_eq!(dedup_rows.len(), 2);
        assert!(dedup_rows.iter().all(|(id, semantic)| id == semantic));
        Ok(())
    }

    #[test]
    fn replay_is_idempotent_zero_new_imports() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let user_root = temp.path().join("solo-user");
        create_fixture_db(&user_root);
        let db_path = user_root.join("database").join("data.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open fixture");
            insert_usage_row(
                &conn,
                "2026-06-07",
                "conv-a",
                None,
                "main-auto",
                100,
                10,
                5,
                50,
                "2026-06-07T09:00:00.000000",
            );
            insert_usage_row(
                &conn,
                "2026-06-07",
                "conv-a",
                None,
                "file-auto",
                200,
                20,
                3,
                60,
                "2026-06-07T09:00:01.000000",
            );
        }

        let db = Database::memory().expect("memory db");
        let first = sync_workbuddy_user_roots(&db, std::slice::from_ref(&user_root));
        assert_eq!((first.imported, first.skipped), (2, 0));

        let second = sync_workbuddy_user_roots(&db, &[user_root]);
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

    #[test]
    fn same_row_id_across_user_databases_both_imported() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let user_a = temp.path().join("user-alpha");
        let user_b = temp.path().join("user-beta");
        for root in [&user_a, &user_b] {
            let db_path = create_fixture_db(root);
            let conn = rusqlite::Connection::open(&db_path).expect("open fixture");
            insert_usage_row(
                &conn,
                "2026-07-01",
                "shared-conversation",
                None,
                "main-auto",
                11,
                7,
                1,
                2,
                "2026-07-01T08:00:00.000000",
            );
        }

        let db = Database::memory().expect("memory db");
        let result = sync_workbuddy_user_roots(&db, &[user_a.clone(), user_b]);
        assert_eq!(result.imported, 2, "同 rowid 跨用户库必须都导入");
        assert_eq!(result.files_scanned, 2);

        let count: i64 = lock_conn!(db.conn)
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_logs WHERE data_source = 'workbuddy_session'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(count, 2);

        for user in ["alpha", "beta"] {
            let exists: bool = lock_conn!(db.conn)
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM proxy_request_logs WHERE request_id = ?1)",
                    rusqlite::params![format!("workbuddy:user-{user}:1")],
                    |row| row.get(0),
                )
                .expect("exists check");
            assert!(exists, "workbuddy:user-{user}:1 必须存在");
        }
        Ok(())
    }

    #[test]
    fn dedup_ledger_is_namespaced_per_user_directory() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let user_root = temp.path().join("ledger-user");
        create_fixture_db(&user_root);
        let db_path = user_root.join("database").join("data.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open fixture");
            insert_usage_row(
                &conn,
                "2026-07-02",
                "conv-ledger",
                None,
                "search-auto",
                5,
                5,
                0,
                0,
                "2026-07-02T10:00:00.123456",
            );
        }

        let db = Database::memory().expect("memory db");
        assert_eq!(sync_workbuddy_user_roots(&db, &[user_root]).imported, 1);

        let (request_id, semantic_id, has_entry): (String, String, i64) = lock_conn!(db.conn)
            .query_row(
                "SELECT request_id, semantic_id, has_entry_id FROM session_usage_dedup
                 WHERE data_source = 'workbuddy_session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read dedup row");
        assert_eq!(request_id, "workbuddy:ledger-user:1");
        assert_eq!(semantic_id, "workbuddy:ledger-user:1");
        assert_eq!(has_entry, 1);
        Ok(())
    }

    #[test]
    fn bad_created_at_falls_back_to_usage_date_then_skips_without_aborting() -> Result<(), AppError>
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let user_root = temp.path().join("fallback-user");
        create_fixture_db(&user_root);
        let db_path = user_root.join("database").join("data.db");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open fixture");
            // Good row keeps the sync alive.
            insert_usage_row(
                &conn,
                "2026-06-07",
                "conv-good",
                None,
                "main-auto",
                10,
                5,
                0,
                1,
                "2026-06-07T12:00:00.000000",
            );
            // Broken created_at → usage_date midnight fallback.
            insert_usage_row(
                &conn,
                "2026-06-08",
                "conv-fallback",
                None,
                "main-auto",
                20,
                6,
                0,
                2,
                "not-a-timestamp",
            );
            // Broken everything → counted skipped, sync continues.
            insert_usage_row(
                &conn,
                "garbage-date",
                "conv-dead",
                None,
                "main-auto",
                30,
                7,
                0,
                3,
                "also-garbage",
            );
        }

        let db = Database::memory().expect("memory db");
        let result = sync_workbuddy_user_roots(&db, &[user_root]);
        assert_eq!(
            (result.imported, result.skipped),
            (2, 1),
            "坏时间戳行计 skipped，其余行照常导入"
        );
        assert!(result.errors.is_empty());

        let conn = lock_conn!(db.conn);
        let good_ts: i64 = conn
            .query_row(
                "SELECT created_at FROM proxy_request_logs WHERE session_id = 'conv-good'",
                [],
                |row| row.get(0),
            )
            .expect("good ts");
        assert_eq!(
            good_ts,
            expected_local_unix_seconds("2026-06-07T12:00:00.000000")
        );

        let fallback_ts: i64 = conn
            .query_row(
                "SELECT created_at FROM proxy_request_logs WHERE session_id = 'conv-fallback'",
                [],
                |row| row.get(0),
            )
            .expect("fallback ts");
        assert_eq!(
            fallback_ts,
            expected_local_unix_seconds("2026-06-08T00:00:00.000000"),
            "created_at 解析失败时退回 usage_date 本机零点"
        );

        let dead_exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM proxy_request_logs WHERE session_id = 'conv-dead')",
                [],
                |row| row.get(0),
            )
            .expect("dead exists");
        assert!(!dead_exists);
        Ok(())
    }

    #[test]
    fn missing_marvis_dir_reports_zero_files_without_error() {
        let db = Database::memory().expect("memory db");
        let result = sync_workbuddy_user_roots(&db, &[]);
        assert_eq!(result.files_scanned, 0);
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());

        // A User directory whose data.db vanished between listing and open is
        // treated like any other unopenable database: warn + deferred.
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join("empty-user")).expect("mkdir");
        let empty = sync_workbuddy_user_roots(&db, &[temp.path().join("empty-user")]);
        assert_eq!(
            (empty.files_scanned, empty.imported, empty.deferred_files),
            (1, 0, 1),
            "列到的用户目录缺库按打不开计 deferred"
        );
        assert_eq!(empty.errors.len(), 1);
    }

    #[test]
    fn unreadable_database_defers_and_remaining_dbs_still_import() {
        let temp = tempfile::tempdir().expect("tempdir");
        let broken_root = temp.path().join("broken-user");
        let broken_db = create_fixture_db(&broken_root);
        std::fs::write(&broken_db, b"this is definitely not sqlite").expect("corrupt fixture");

        let good_root = temp.path().join("good-user");
        let good_db = create_fixture_db(&good_root);
        {
            let conn = rusqlite::Connection::open(&good_db).expect("open fixture");
            insert_usage_row(
                &conn,
                "2026-07-03",
                "conv-good-db",
                None,
                "browser-auto",
                42,
                24,
                2,
                8,
                "2026-07-03T06:30:00.000000",
            );
        }

        let db = Database::memory().expect("memory db");
        let result = sync_workbuddy_user_roots(&db, &[broken_root, good_root]);
        assert_eq!(result.files_scanned, 2);
        assert_eq!(result.deferred_files, 1, "打不开的库计 deferred");
        assert_eq!(result.imported, 1, "其余库继续正常导入");
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn missing_llm_token_usage_table_yields_empty_import() {
        let temp = tempfile::tempdir().expect("tempdir");
        let user_root = temp.path().join("schema-drift-user");
        let db_path = user_root.join("database").join("data.db");
        std::fs::create_dir_all(db_path.parent().expect("parent")).expect("mkdir");
        rusqlite::Connection::open(&db_path).expect("open empty db");

        let db = Database::memory().expect("memory db");
        let result = sync_workbuddy_user_roots(&db, &[user_root]);
        assert_eq!(
            (result.imported, result.skipped, result.deferred_files),
            (0, 0, 0)
        );
        assert!(result.errors.is_empty());
    }

    fn workbuddy_env_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// The WorkBuddy step must be wired into the shared sync kernel: driving
    /// `sync_all_unlocked` against a fixture root must land the row. This is
    /// the guard that turns "someone commented out the merge_sync_step" into a
    /// red test instead of silent data loss.
    #[test]
    #[allow(deprecated)] // set_var/remove_var deprecated since Rust 1.81; safe under mutex
    fn workbuddy_step_is_registered_in_sync_all_unlocked() -> Result<(), AppError> {
        let _guard = workbuddy_env_lock().lock().expect("env lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let user_root = temp.path().join("registered-user");
        let db_path = create_fixture_db(&user_root);
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open fixture");
            insert_usage_row(
                &conn,
                "2026-07-04",
                "conv-registered",
                None,
                "main-auto",
                12,
                8,
                1,
                3,
                "2026-07-04T07:00:00.000000",
            );
        }

        let original = std::env::var_os("WORKBUDDY_DATA_DIR");
        std::env::set_var("WORKBUDDY_DATA_DIR", temp.path());
        let db = Database::memory().expect("memory db");
        let result = crate::services::session_usage::sync_all_unlocked(&db);
        match original {
            Some(value) => std::env::set_var("WORKBUDDY_DATA_DIR", value),
            None => std::env::remove_var("WORKBUDDY_DATA_DIR"),
        }

        // Other importers may legitimately contribute rows/errors in this
        // shared run; only the WorkBuddy ledger is asserted here.
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
}
