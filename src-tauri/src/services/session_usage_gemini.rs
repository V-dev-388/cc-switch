//! Antigravity (Gemini CLI) 会话日志使用追踪
//!
//! 扫描并解析 ~/.gemini/{antigravity,antigravity-local,antigravity-cli,antigravity-ide}/conversations/*.db
//! SQLite 数据库，提取 steps 与 gen_metadata 表中的 Protobuf Wire Format 数据，
//! 导入到 proxy_request_logs 中。
//!
//! ## 数据流
//! ```text
//! ~/.gemini/*/conversations/*.db → 只读 SQLite 连接 → steps (时间戳) + gen_metadata (模型与 Token)
//! → 轻量 Wire Format 解析 → 费用计算 → 去重账本 → proxy_request_logs 表
//! ```

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::calculator::CostCalculator;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::session_usage::{
    metadata_modified_nanos, update_sync_state, SessionSyncResult,
};
use crate::services::sql_helpers::INPUT_TOKEN_SEMANTICS_LEGACY;
use crate::services::usage_stats::{find_model_pricing, should_skip_session_insert, DedupKey};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const DATA_SOURCE: &str = "antigravity_session";
pub const APP_TYPE: &str = "gemini";
pub const PROVIDER_ID: &str = "_antigravity_session";
pub const PROVIDER_TYPE: &str = "antigravity_session";
pub const INPUT_TOKEN_SEMANTICS_UNCACHED: i64 = INPUT_TOKEN_SEMANTICS_LEGACY; // 0

// ============================================================================
// Protobuf Wire Format Parser (纯 Rust 轻量解析，无 C / protoc 依赖)
// ============================================================================

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    UnexpectedEof,
    VarintOverflow,
    InvalidString,
    UnknownWireType(u8),
}

#[derive(Debug, Clone)]
pub struct WireField<'a> {
    pub tag: u32,
    pub wire_type: u8,
    pub varint: u64,
    pub data: &'a [u8],
}

pub fn read_varint(buf: &[u8], offset: &mut usize) -> Result<u64, WireError> {
    let mut val: u64 = 0;
    let mut shift = 0;
    while *offset < buf.len() {
        let b = buf[*offset];
        *offset += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if (b & 0x80) == 0 {
            return Ok(val);
        }
        shift += 7;
        if shift >= 64 {
            return Err(WireError::VarintOverflow);
        }
    }
    Err(WireError::UnexpectedEof)
}

pub fn next_wire_field<'a>(
    buf: &'a [u8],
    offset: &mut usize,
) -> Result<Option<WireField<'a>>, WireError> {
    if *offset >= buf.len() {
        return Ok(None);
    }
    let key = read_varint(buf, offset)?;
    let tag = (key >> 3) as u32;
    let wire_type = (key & 0x07) as u8;
    match wire_type {
        0 => {
            let start = *offset;
            let val = read_varint(buf, offset)?;
            Ok(Some(WireField {
                tag,
                wire_type,
                varint: val,
                data: &buf[start..*offset],
            }))
        }
        1 => {
            if *offset + 8 > buf.len() {
                return Err(WireError::UnexpectedEof);
            }
            let data = &buf[*offset..*offset + 8];
            *offset += 8;
            Ok(Some(WireField {
                tag,
                wire_type,
                varint: 0,
                data,
            }))
        }
        2 => {
            let len = read_varint(buf, offset)? as usize;
            if *offset + len > buf.len() {
                return Err(WireError::UnexpectedEof);
            }
            let data = &buf[*offset..*offset + len];
            *offset += len;
            Ok(Some(WireField {
                tag,
                wire_type,
                varint: 0,
                data,
            }))
        }
        5 => {
            if *offset + 4 > buf.len() {
                return Err(WireError::UnexpectedEof);
            }
            let data = &buf[*offset..*offset + 4];
            *offset += 4;
            Ok(Some(WireField {
                tag,
                wire_type,
                varint: 0,
                data,
            }))
        }
        _ => Err(WireError::UnknownWireType(wire_type)),
    }
}

pub fn parse_wire_fields<'a>(buf: &'a [u8]) -> Result<Vec<WireField<'a>>, WireError> {
    let mut offset = 0;
    let mut fields = Vec::new();
    while offset < buf.len() {
        if let Some(field) = next_wire_field(buf, &mut offset)? {
            fields.push(field);
        }
    }
    Ok(fields)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AntigravityTokens {
    pub input: u32,
    pub output: u32,
    pub cached: u32,
    pub thoughts: u32,
}

impl AntigravityTokens {
    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cached == 0 && self.thoughts == 0
    }
}

pub fn parse_token_block(buf: &[u8]) -> Result<AntigravityTokens, WireError> {
    let fields = parse_wire_fields(buf)?;
    let mut tokens = AntigravityTokens::default();
    for f in fields {
        if f.wire_type == 0 {
            match f.tag {
                2 => tokens.input = f.varint as u32,
                3 => tokens.output = f.varint as u32,
                5 => tokens.cached = f.varint as u32,
                6 => tokens.thoughts = f.varint as u32,
                _ => {}
            }
        }
    }
    Ok(tokens)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedGenMetadata {
    pub uuid: String,
    pub model: String,
    pub tokens: AntigravityTokens,
}

pub fn parse_gen_metadata(buf: &[u8]) -> Result<ParsedGenMetadata, WireError> {
    let fields = parse_wire_fields(buf)?;
    let mut uuid = String::new();
    let mut model = "unknown".to_string();
    let mut tokens = AntigravityTokens::default();

    for f in fields {
        if f.tag == 4 && f.wire_type == 2 {
            uuid = std::str::from_utf8(f.data)
                .map_err(|_| WireError::InvalidString)?
                .to_string();
        } else if f.tag == 1 && f.wire_type == 2 {
            let subfields = parse_wire_fields(f.data)?;
            for sf in subfields {
                if sf.tag == 19 && sf.wire_type == 2 {
                    model = std::str::from_utf8(sf.data)
                        .map_err(|_| WireError::InvalidString)?
                        .to_string();
                } else if sf.tag == 4 && sf.wire_type == 2 {
                    tokens = parse_token_block(sf.data)?;
                }
            }
        }
    }

    Ok(ParsedGenMetadata {
        uuid,
        model,
        tokens,
    })
}

pub fn parse_step_metadata(buf: &[u8]) -> Result<Option<(String, i64)>, WireError> {
    let fields = parse_wire_fields(buf)?;
    let mut uuid = None;
    let mut timestamp = None;

    for f in fields {
        if f.tag == 12 && f.wire_type == 2 {
            if let Ok(s) = std::str::from_utf8(f.data) {
                uuid = Some(s.to_string());
            }
        } else if f.tag == 1 && f.wire_type == 2 {
            if let Ok(subfields) = parse_wire_fields(f.data) {
                for sf in subfields {
                    if sf.tag == 1 && sf.wire_type == 0 {
                        timestamp = Some(sf.varint as i64);
                    }
                }
            }
        }
    }

    match (uuid, timestamp) {
        (Some(u), Some(t)) => Ok(Some((u, t))),
        _ => Ok(None),
    }
}

// ============================================================================
// Antigravity 会话目录发现与文件收集
// ============================================================================

/// 获取默认扫描的 Antigravity 会话目录列表（覆盖三端产品及本地目录）
pub fn get_default_antigravity_conversations_dirs() -> Vec<PathBuf> {
    if let Some(custom) = std::env::var_os("ANTIGRAVITY_CONVERSATIONS_DIRS") {
        return std::env::split_paths(&custom).collect();
    }
    let home = crate::config::get_home_dir();
    let gemini = home.join(".gemini");
    vec![
        gemini.join("antigravity").join("conversations"),
        gemini.join("antigravity-local").join("conversations"),
        gemini.join("antigravity-cli").join("conversations"),
        gemini.join("antigravity-ide").join("conversations"),
    ]
}

/// 扫描目录列表，收集所有 unique *.db 数据库文件（软链去重，缺失目录静默返回空）
pub fn collect_antigravity_db_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen_canonical = HashSet::new();

    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("db") {
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                if seen_canonical.insert(canonical) {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    files
}

// ============================================================================
// 数据库连接与同步核心
// ============================================================================

/// 以只读模式打开 SQLite 会话库，带 busy_timeout 避免并发写锁死
fn open_antigravity_db_readonly(path: &Path) -> Result<rusqlite::Connection, AppError> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        AppError::Database(format!(
            "无法以只读方式打开会话数据库 {}: {e}",
            path.display()
        ))
    })?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))
        .map_err(|e| AppError::Database(format!("设置 busy_timeout 失败: {e}")))?;
    Ok(conn)
}

fn table_exists(conn: &rusqlite::Connection, table_name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [table_name],
        |row| row.get::<_, bool>(0),
    )
    .unwrap_or(false)
}

/// 同步 Antigravity 会话用量数据（默认扫描本机 ~/.gemini 三端目录）
pub fn sync_gemini_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let dirs = get_default_antigravity_conversations_dirs();
    sync_antigravity_usage_from_dirs(db, &dirs)
}

/// 从指定目录列表同步 Antigravity 会话数据（便于测试直接传入 TempDir 目录）
pub fn sync_antigravity_usage_from_dirs(
    db: &Database,
    dirs: &[PathBuf],
) -> Result<SessionSyncResult, AppError> {
    let files = collect_antigravity_db_files(dirs);

    let mut result = SessionSyncResult {
        imported: 0,
        skipped: 0,
        files_scanned: files.len() as u32,
        suspected_duplicates: 0,
        deferred_files: 0,
        errors: vec![],
    };

    if files.is_empty() {
        return Ok(result);
    }

    let cursors = crate::services::session_usage::load_sync_cursors(db)?;

    for file_path in &files {
        let cursor = cursors.get(file_path.to_string_lossy().as_ref()).copied();
        match sync_single_antigravity_db(db, file_path, cursor) {
            Ok((imported, skipped)) => {
                result.imported = result.imported.saturating_add(imported);
                result.skipped = result.skipped.saturating_add(skipped);
            }
            Err(e) => {
                let msg = format!(
                    "Antigravity 会话数据库解析失败 {}: {e}",
                    file_path.display()
                );
                log::warn!("[ANTIGRAVITY-SYNC] {msg}");
                result.errors.push(msg);
                result.deferred_files = result.deferred_files.saturating_add(1);
            }
        }
    }

    if result.imported > 0 {
        log::info!(
            "[ANTIGRAVITY-SYNC] 同步完成: 导入 {} 条, 跳过 {} 条, 扫描 {} 个文件",
            result.imported,
            result.skipped,
            result.files_scanned
        );
    }

    Ok(result)
}

/// 同步单个 Antigravity SQLite 数据库
pub fn sync_single_antigravity_db(
    db: &Database,
    file_path: &Path,
    cursor: Option<crate::services::session_usage::SyncCursor>,
) -> Result<(u32, u32), AppError> {
    let file_path_str = file_path.to_string_lossy().to_string();

    let metadata = fs::metadata(file_path)
        .map_err(|e| AppError::Config(format!("无法读取文件元数据: {e}")))?;
    let file_modified = metadata_modified_nanos(&metadata);

    let last_modified = cursor.map_or(0, |c| c.last_modified);
    let last_offset = cursor.map_or(0, |c| c.last_line_offset);

    // 文件 mtime 未改变则直接跳过，将已同步记录数计入 skipped 以维护对账总数
    if file_modified <= last_modified {
        return Ok((0, last_offset as u32));
    }

    let conn = open_antigravity_db_readonly(file_path)?;

    // 检查表是否存在，不存在直接记同步游标并跳过
    if !table_exists(&conn, "gen_metadata") {
        update_sync_state(db, &file_path_str, file_modified, 0)?;
        return Ok((0, 0));
    }

    // 确定 conv_id（优先从 trajectory_meta 读取 cascade_id，否则取文件名 stem）
    let conv_id = if table_exists(&conn, "trajectory_meta") {
        conn.query_row(
            "SELECT cascade_id FROM trajectory_meta WHERE cascade_id IS NOT NULL AND cascade_id != '' LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .unwrap_or_else(|| {
            file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string()
        })
    } else {
        file_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    };

    // 从 steps 表建立 (uuid -> timestamp_secs) 映射
    let mut uuid_to_timestamp = HashMap::new();
    if table_exists(&conn, "steps") {
        if let Ok(mut stmt) =
            conn.prepare("SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL")
        {
            if let Ok(rows) = stmt.query_map([], |row| {
                let idx: i64 = row.get(0)?;
                let meta: Vec<u8> = row.get(1)?;
                Ok((idx, meta))
            }) {
                for r in rows.flatten() {
                    if let Ok(Some((u, ts))) = parse_step_metadata(&r.1) {
                        uuid_to_timestamp.insert(u, ts);
                    }
                }
            }
        }
    }

    let file_mtime_secs = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // 读取增量 gen_metadata 行
    let mut stmt = conn
        .prepare("SELECT idx, data FROM gen_metadata WHERE idx >= ?1 ORDER BY idx ASC")
        .map_err(|e| AppError::Database(format!("准备查询 gen_metadata 失败: {e}")))?;

    let gen_rows = stmt
        .query_map([last_offset], |row| {
            let idx: i64 = row.get(0)?;
            let data: Vec<u8> = row.get(1)?;
            Ok((idx, data))
        })
        .map_err(|e| AppError::Database(format!("查询 gen_metadata 失败: {e}")))?;

    let mut imported: u32 = 0;
    let mut skipped: u32 = 0;
    let mut max_idx = last_offset;
    let mut seen_uuids_in_conv = HashSet::new();

    {
        let target_conn = lock_conn!(db.conn);
        let tx = target_conn
            .unchecked_transaction()
            .map_err(|e| AppError::Database(format!("启动导入事务失败: {e}")))?;

        for r in gen_rows {
            let (idx, data) = match r {
                Ok(pair) => pair,
                Err(e) => {
                    log::warn!("[ANTIGRAVITY-SYNC] 读取 gen_metadata 行失败: {e}");
                    skipped += 1;
                    continue;
                }
            };

            if idx >= max_idx {
                max_idx = idx + 1;
            }

            let parsed = match parse_gen_metadata(&data) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("[ANTIGRAVITY-SYNC] 解析 gen_metadata 行 (idx={idx}) 失败: {e:?}");
                    skipped += 1;
                    continue;
                }
            };

            if parsed.tokens.is_empty() {
                continue;
            }

            let uuid = if parsed.uuid.is_empty() {
                format!("gen-{idx}")
            } else {
                parsed.uuid
            };

            // 一轮多步交互时首步保持标准 request_id，后续模型调用追加 :idx 以保证 100% 统计不漏且无主键冲突
            let request_id = if seen_uuids_in_conv.insert(uuid.clone()) {
                format!("antigravity:{conv_id}:{uuid}")
            } else {
                format!("antigravity:{conv_id}:{uuid}:{idx}")
            };

            let created_at = uuid_to_timestamp
                .get(&uuid)
                .copied()
                .unwrap_or(file_mtime_secs);

            match insert_antigravity_entry(
                &tx,
                &request_id,
                &conv_id,
                &parsed.model,
                &parsed.tokens,
                created_at,
            ) {
                Ok(true) => imported += 1,
                Ok(false) => skipped += 1,
                Err(e) => {
                    log::warn!("[ANTIGRAVITY-SYNC] 插入用量日志失败 ({}): {e}", request_id);
                    skipped += 1;
                }
            }
        }

        tx.commit()
            .map_err(|e| AppError::Database(format!("提交导入事务失败: {e}")))?;
    }

    // 更新游标状态
    update_sync_state(db, &file_path_str, file_modified, max_idx)?;

    Ok((imported, skipped))
}

/// 写入单条 Antigravity 会话记录到 proxy_request_logs
fn insert_antigravity_entry(
    conn: &rusqlite::Connection,
    request_id: &str,
    conv_id: &str,
    model: &str,
    tokens: &AntigravityTokens,
    created_at: i64,
) -> Result<bool, AppError> {
    let output_tokens = tokens.output + tokens.thoughts;

    // 1. 查询 session_usage_dedup 账本
    let already_seen: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM session_usage_dedup WHERE data_source = ?1 AND request_id = ?2)",
            rusqlite::params![DATA_SOURCE, request_id],
            |row| row.get(0),
        )
        .map_err(|e| AppError::Database(format!("查询去重账本失败: {e}")))?;
    if already_seen {
        return Ok(false);
    }

    // 2. 跨代理去重判断
    let dedup_key = DedupKey {
        app_type: APP_TYPE,
        model,
        input_tokens: tokens.input,
        output_tokens,
        cache_read_tokens: tokens.cached,
        cache_creation_tokens: 0,
        created_at,
    };
    if should_skip_session_insert(conn, request_id, &dedup_key)? {
        return Ok(false);
    }

    // 3. 记录至去重账本
    conn.execute(
        "INSERT OR IGNORE INTO session_usage_dedup (data_source, request_id, semantic_id, has_entry_id) VALUES (?1, ?2, ?3, 1)",
        rusqlite::params![DATA_SOURCE, request_id, request_id],
    )
    .map_err(|e| AppError::Database(format!("写入去重账本失败: {e}")))?;

    // 4. 费用计算
    let usage = TokenUsage {
        input_tokens: tokens.input,
        output_tokens,
        cache_read_tokens: tokens.cached,
        cache_creation_tokens: 0,
        model: Some(model.to_string()),
        message_id: None,
    };
    let pricing = find_model_pricing(conn, model);
    let (input_cost, output_cost, cache_read_cost, cache_creation_cost, total_cost) = match pricing
    {
        Some(p) => {
            let cost = CostCalculator::calculate_for_app(APP_TYPE, &usage, &p, Decimal::ONE);
            (
                cost.input_cost.to_string(),
                cost.output_cost.to_string(),
                cost.cache_read_cost.to_string(),
                cost.cache_creation_cost.to_string(),
                cost.total_cost.to_string(),
            )
        }
        None => (
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
            "0".to_string(),
        ),
    };

    // 5. 写入 proxy_request_logs
    let rows_affected = conn.execute(
        "INSERT OR IGNORE INTO proxy_request_logs (
            request_id, provider_id, app_type, model, request_model, pricing_model,
            input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
            input_token_semantics,
            input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
            latency_ms, first_token_ms, status_code, error_message, session_id,
            provider_type, is_streaming, cost_multiplier, created_at, data_source
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10,
            ?11,
            ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21,
            ?22, ?23, ?24, ?25, ?26
        )",
        rusqlite::params![
            request_id,
            PROVIDER_ID,
            APP_TYPE,
            model,
            model,
            model,
            tokens.input,
            output_tokens,
            tokens.cached,
            0i64,
            INPUT_TOKEN_SEMANTICS_UNCACHED,
            input_cost,
            output_cost,
            cache_read_cost,
            cache_creation_cost,
            total_cost,
            0i64,
            Option::<i64>::None,
            200i64,
            Option::<String>::None,
            Some(conv_id),
            Some(PROVIDER_TYPE),
            1i64,
            "1.0",
            created_at,
            DATA_SOURCE,
        ],
    )
    .map_err(|e| AppError::Database(format!("插入 Antigravity 会话日志失败: {e}")))?;

    Ok(rows_affected > 0)
}

// ============================================================================
// 单元测试与验证套件
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Wire Format 编码辅助工具函数，用于测试构造 Protobuf 二进制
    fn write_varint(buf: &mut Vec<u8>, mut val: u64) {
        while val >= 0x80 {
            buf.push((val as u8 & 0x7F) | 0x80);
            val >>= 7;
        }
        buf.push(val as u8);
    }

    fn write_key(buf: &mut Vec<u8>, tag: u32, wire_type: u8) {
        write_varint(buf, ((tag as u64) << 3) | (wire_type as u64));
    }

    fn write_length_delimited(buf: &mut Vec<u8>, tag: u32, data: &[u8]) {
        write_key(buf, tag, 2);
        write_varint(buf, data.len() as u64);
        buf.extend_from_slice(data);
    }

    fn write_varint_field(buf: &mut Vec<u8>, tag: u32, val: u64) {
        write_key(buf, tag, 0);
        write_varint(buf, val);
    }

    fn build_test_gen_metadata(
        uuid: &str,
        model: &str,
        input: u32,
        output: u32,
        cached: u32,
        thoughts: u32,
    ) -> Vec<u8> {
        let mut token_buf = Vec::new();
        write_varint_field(&mut token_buf, 2, input as u64);
        write_varint_field(&mut token_buf, 3, output as u64);
        write_varint_field(&mut token_buf, 5, cached as u64);
        write_varint_field(&mut token_buf, 6, thoughts as u64);

        let mut tag1_buf = Vec::new();
        write_length_delimited(&mut tag1_buf, 19, model.as_bytes());
        write_length_delimited(&mut tag1_buf, 4, &token_buf);

        let mut out = Vec::new();
        write_length_delimited(&mut out, 4, uuid.as_bytes());
        write_length_delimited(&mut out, 1, &tag1_buf);
        out
    }

    fn build_test_step_metadata(uuid: &str, timestamp: i64) -> Vec<u8> {
        let mut ts_buf = Vec::new();
        write_varint_field(&mut ts_buf, 1, timestamp as u64);

        let mut out = Vec::new();
        write_length_delimited(&mut out, 1, &ts_buf);
        write_length_delimited(&mut out, 12, uuid.as_bytes());
        out
    }

    fn setup_test_sqlite_db(
        path: &Path,
        gen_rows: &[(i64, Vec<u8>)],
        step_rows: &[(i64, Vec<u8>)],
    ) {
        let conn = rusqlite::Connection::open(path).expect("open test db");
        conn.execute_batch(
            "CREATE TABLE trajectory_meta (cascade_id TEXT, trajectory_id TEXT);
             CREATE TABLE steps (idx INTEGER PRIMARY KEY, metadata BLOB);
             CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB, size INTEGER DEFAULT 0);",
        )
        .expect("create test tables");

        conn.execute(
            "INSERT INTO trajectory_meta (cascade_id, trajectory_id) VALUES ('test-cascade-id', 'test-traj-id')",
            [],
        )
        .expect("insert meta");

        for (idx, meta) in step_rows {
            conn.execute(
                "INSERT INTO steps (idx, metadata) VALUES (?1, ?2)",
                rusqlite::params![idx, meta],
            )
            .expect("insert step");
        }

        for (idx, data) in gen_rows {
            conn.execute(
                "INSERT INTO gen_metadata (idx, data, size) VALUES (?1, ?2, ?3)",
                rusqlite::params![idx, data, data.len() as i64],
            )
            .expect("insert gen");
        }
    }

    #[test]
    fn test_single_step_parse() {
        let bytes = build_test_gen_metadata("req-uuid-1", "gemini-3.8-flash", 120, 30, 80, 5);
        let parsed = parse_gen_metadata(&bytes).expect("parse gen metadata");
        assert_eq!(parsed.uuid, "req-uuid-1");
        assert_eq!(parsed.model, "gemini-3.8-flash");
        assert_eq!(parsed.tokens.input, 120);
        assert_eq!(parsed.tokens.output, 30);
        assert_eq!(parsed.tokens.cached, 80);
        assert_eq!(parsed.tokens.thoughts, 5);
    }

    #[test]
    fn test_three_client_paths_scan() {
        let temp = tempfile::tempdir().expect("tempdir");
        let base = temp.path();

        let dir_ag = base.join("antigravity").join("conversations");
        let dir_local = base.join("antigravity-local").join("conversations");
        let dir_cli = base.join("antigravity-cli").join("conversations");
        let dir_ide = base.join("antigravity-ide").join("conversations");

        fs::create_dir_all(&dir_local).expect("create local");
        fs::create_dir_all(&dir_cli).expect("create cli");
        fs::create_dir_all(&dir_ide).expect("create ide");

        // 创建软链接 antigravity -> antigravity-local
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("antigravity-local"), base.join("antigravity"))
            .expect("symlink");
        #[cfg(not(unix))]
        fs::create_dir_all(&dir_ag).expect("create ag");

        // 在各目录放置 .db 文件及混淆文件
        let db1 = dir_local.join("c1.db");
        let db2 = dir_cli.join("c2.db");
        let db3 = dir_ide.join("c3.db");
        let ignored_pb = dir_ide.join("old.pb");
        let ignored_wal = dir_ide.join("c3.db-wal");

        fs::write(&db1, b"").expect("write db1");
        fs::write(&db2, b"").expect("write db2");
        fs::write(&db3, b"").expect("write db3");
        fs::write(&ignored_pb, b"").expect("write pb");
        fs::write(&ignored_wal, b"").expect("write wal");

        let dirs = vec![dir_ag, dir_local, dir_cli, dir_ide];
        let files = collect_antigravity_db_files(&dirs);

        // 软链去重后，总共应收集到正好 3 个不同的 .db 文件，.pb 和 .db-wal 被过滤
        assert_eq!(files.len(), 3);
        let stems: HashSet<_> = files
            .iter()
            .map(|f| f.file_stem().unwrap().to_str().unwrap())
            .collect();
        assert!(stems.contains("c1"));
        assert!(stems.contains("c2"));
        assert!(stems.contains("c3"));
    }

    #[test]
    fn test_column_assertions() -> Result<(), AppError> {
        let temp = tempfile::tempdir().map_err(|e| AppError::Config(e.to_string()))?;
        let db_file = temp.path().join("c-test.db");

        let gen_bytes =
            build_test_gen_metadata("req-col-assert", "gemini-3.8-flash", 100, 50, 40, 10);
        let step_bytes = build_test_step_metadata("req-col-assert", 1_788_000_123);

        setup_test_sqlite_db(&db_file, &[(0, gen_bytes)], &[(0, step_bytes)]);

        let app_db = Database::memory()?;
        let (imported, skipped) = sync_single_antigravity_db(&app_db, &db_file, None)?;
        assert_eq!(imported, 1);
        assert_eq!(skipped, 0);

        struct AssertRow {
            request_id: String,
            provider_id: String,
            app_type: String,
            model: String,
            input_tokens: i64,
            output_tokens: i64,
            cache_read_tokens: i64,
            cache_creation_tokens: i64,
            input_token_semantics: i64,
            status_code: i64,
            session_id: Option<String>,
            provider_type: String,
            created_at: i64,
            data_source: String,
        }

        let conn = lock_conn!(app_db.conn);
        let row: AssertRow = conn.query_row(
            "SELECT request_id, provider_id, app_type, model, input_tokens, output_tokens,
                    cache_read_tokens, cache_creation_tokens, input_token_semantics,
                    status_code, session_id, provider_type, created_at, data_source
             FROM proxy_request_logs
             WHERE request_id = 'antigravity:test-cascade-id:req-col-assert'",
            [],
            |r| {
                Ok(AssertRow {
                    request_id: r.get(0)?,
                    provider_id: r.get(1)?,
                    app_type: r.get(2)?,
                    model: r.get(3)?,
                    input_tokens: r.get(4)?,
                    output_tokens: r.get(5)?,
                    cache_read_tokens: r.get(6)?,
                    cache_creation_tokens: r.get(7)?,
                    input_token_semantics: r.get(8)?,
                    status_code: r.get(9)?,
                    session_id: r.get(10)?,
                    provider_type: r.get(11)?,
                    created_at: r.get(12)?,
                    data_source: r.get(13)?,
                })
            },
        )?;

        assert_eq!(row.request_id, "antigravity:test-cascade-id:req-col-assert");
        assert_eq!(row.provider_id, "_antigravity_session");
        assert_eq!(row.app_type, "gemini");
        assert_eq!(row.model, "gemini-3.8-flash");
        assert_eq!(row.input_tokens, 100);
        assert_eq!(row.output_tokens, 60); // output 50 + thoughts 10 = 60
        assert_eq!(row.cache_read_tokens, 40); // cached
        assert_eq!(row.cache_creation_tokens, 0); // cache_creation
        assert_eq!(row.input_token_semantics, 0); // semantics = 0 (UNCACHED)
        assert_eq!(row.status_code, 200);
        assert_eq!(row.session_id, Some("test-cascade-id".to_string()));
        assert_eq!(row.provider_type, "antigravity_session");
        assert_eq!(row.created_at, 1_788_000_123);
        assert_eq!(row.data_source, "antigravity_session");

        Ok(())
    }

    #[test]
    fn test_idempotent_dedup() -> Result<(), AppError> {
        let temp = tempfile::tempdir().map_err(|e| AppError::Config(e.to_string()))?;
        let db_file = temp.path().join("c-idempotent.db");

        let gen_bytes = build_test_gen_metadata("req-idemp", "gemini-3.8-flash", 200, 80, 50, 0);
        setup_test_sqlite_db(&db_file, &[(0, gen_bytes)], &[]);

        let app_db = Database::memory()?;
        // 首次同步
        let dirs = vec![temp.path().to_path_buf()];
        let res1 = sync_antigravity_usage_from_dirs(&app_db, &dirs)?;
        assert_eq!(res1.imported, 1);
        assert_eq!(res1.skipped, 0);

        // 二次同步：完全幂等，imported 必须为 0
        let res2 = sync_antigravity_usage_from_dirs(&app_db, &dirs)?;
        assert_eq!(res2.imported, 0);
        Ok(())
    }

    #[test]
    fn test_missing_directory_skipped() -> Result<(), AppError> {
        let app_db = Database::memory()?;
        let non_existent = vec![
            PathBuf::from("/non/existent/path/one"),
            PathBuf::from("/non/existent/path/two"),
        ];
        let result = sync_antigravity_usage_from_dirs(&app_db, &non_existent)?;
        assert_eq!(result.imported, 0);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.files_scanned, 0);
        assert!(result.errors.is_empty());
        Ok(())
    }

    #[test]
    fn test_malformed_data_tolerance() -> Result<(), AppError> {
        let temp = tempfile::tempdir().map_err(|e| AppError::Config(e.to_string()))?;
        let db_file = temp.path().join("c-malformed.db");

        let good_bytes = build_test_gen_metadata("req-good", "gemini-3.8-flash", 50, 10, 5, 0);
        // 畸形数据：坏 wire type 与残缺字节
        let corrupted_bytes = vec![0xFF, 0xFF, 0x7F, 0x00, 0xAA];

        setup_test_sqlite_db(&db_file, &[(0, corrupted_bytes), (1, good_bytes)], &[]);

        let app_db = Database::memory()?;
        let (imported, skipped) = sync_single_antigravity_db(&app_db, &db_file, None)?;
        // 畸形行被容错跳过，良性行正常导入，整体不 panic
        assert_eq!(imported, 1);
        assert_eq!(skipped, 1);
        Ok(())
    }

    #[test]
    fn test_reverse_validation_tampered_token_bytes() {
        // 构造一个顶层正常、但 token 块字节流被故意破坏的 payload
        let mut tampered_token = Vec::new();
        write_key(&mut tampered_token, 2, 2); // 声明为 length-delimited
        write_varint(&mut tampered_token, 100); // 声明长度 100 字节
        tampered_token.extend_from_slice(b"short"); // 实际只给 5 字节，制造 UnexpectedEof

        let mut tag1_buf = Vec::new();
        write_length_delimited(&mut tag1_buf, 19, b"gemini-3.8-flash");
        write_length_delimited(&mut tag1_buf, 4, &tampered_token);

        let mut out = Vec::new();
        write_length_delimited(&mut out, 4, b"req-tampered");
        write_length_delimited(&mut out, 1, &tag1_buf);

        // 断言轻量解析器返回 Err 而不是 panic
        let parse_result = parse_gen_metadata(&out);
        assert!(parse_result.is_err(), "篡改字节流必须返回 Err");

        // 进一步断言：即便将此行置于 SQLite 数据库中，导入器也会优雅跳过而不 panic
        let temp = tempfile::tempdir().expect("tempdir");
        let db_file = temp.path().join("c-tampered.db");
        setup_test_sqlite_db(&db_file, &[(0, out)], &[]);

        let app_db = Database::memory().expect("app_db");
        let res = sync_single_antigravity_db(&app_db, &db_file, None);
        assert!(res.is_ok(), "数据库同步遇到篡改数据必须保持 Ok 并不崩溃");
        let (imported, skipped) = res.unwrap();
        assert_eq!(imported, 0);
        assert_eq!(skipped, 1);
    }

    #[test]
    fn test_real_antigravity_scan_and_idempotency() {
        let app_db = Database::memory().expect("app_db");
        // 第一轮：针对本机 ~/.gemini 下真实 Antigravity SQLite 数据库跑 sync_gemini_usage
        let t1 = std::time::Instant::now();
        let res1 = sync_gemini_usage(&app_db).expect("first sync failed");
        let d1 = t1.elapsed();
        println!(
            "\n[REAL-SCAN-RUN-1] imported: {}, skipped: {}, deferred: {}, errors: {}, duration: {:?}",
            res1.imported, res1.skipped, res1.deferred_files, res1.errors.len(), d1
        );

        // 第二轮：紧接着再跑一次 sync_gemini_usage（相同数据库环境）
        let t2 = std::time::Instant::now();
        let res2 = sync_gemini_usage(&app_db).expect("second sync failed");
        let d2 = t2.elapsed();
        println!(
            "[REAL-SCAN-RUN-2] imported: {}, skipped: {}, deferred: {}, errors: {}, duration: {:?}",
            res2.imported,
            res2.skipped,
            res2.deferred_files,
            res2.errors.len(),
            d2
        );

        let dirs = get_default_antigravity_conversations_dirs();
        let real_dbs = collect_antigravity_db_files(&dirs);
        if !real_dbs.is_empty() {
            println!(
                "[REAL-SCAN-INFO] Found {} real Antigravity db files.",
                real_dbs.len()
            );
            assert!(
                res1.imported > 0,
                "本机存在真实 Antigravity 数据库，第一轮必须成功导入有效用量"
            );
            assert_eq!(res2.imported, 0, "第二轮幂等扫描导入数必须为 0");
            assert!(
                res2.skipped >= res1.imported,
                "第二轮跳过数 ({}) 必须大于等于第一轮导入数 ({})",
                res2.skipped,
                res1.imported
            );
        }
    }
}
