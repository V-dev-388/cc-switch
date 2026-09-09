//! Antigravity (Gemini) 会话管理器
//!
//! 扫描并管理 ~/.gemini/{antigravity,antigravity-local,antigravity-cli,antigravity-ide}/conversations/*.db
//! 走只读 SQLite 提取 steps 用户首条提问、消息总数与时间戳，支持消息详情查看与安全删除（含 -wal/-shm 及 brain 目录）。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::Value;

use crate::services::session_usage_gemini::parse_wire_fields;
use crate::session_manager::{SessionMessage, SessionMeta};

use super::utils::{parse_timestamp_to_ms, truncate_summary};

pub const PROVIDER_ID: &str = "gemini";

/// 获取 Gemini / Antigravity 会话根目录（用于删除时的路径白名单安全校验）
pub fn session_roots() -> Vec<PathBuf> {
    if let Some(custom) = std::env::var_os("ANTIGRAVITY_CONVERSATIONS_DIRS") {
        return std::env::split_paths(&custom).collect();
    }
    let gemini_dir = crate::gemini_config::get_gemini_dir();
    vec![
        gemini_dir.join("antigravity").join("conversations"),
        gemini_dir.join("antigravity-local").join("conversations"),
        gemini_dir.join("antigravity-cli").join("conversations"),
        gemini_dir.join("antigravity-ide").join("conversations"),
        gemini_dir.join("antigravity"),
        gemini_dir.join("antigravity-local"),
        gemini_dir.join("antigravity-cli"),
        gemini_dir.join("antigravity-ide"),
        gemini_dir.join("tmp"),
        gemini_dir,
    ]
}

/// 获取默认扫描的 Antigravity 会话目录列表（覆盖三端产品）
pub fn get_conversation_dirs() -> Vec<PathBuf> {
    if let Some(custom) = std::env::var_os("ANTIGRAVITY_CONVERSATIONS_DIRS") {
        return std::env::split_paths(&custom).collect();
    }
    let gemini_dir = crate::gemini_config::get_gemini_dir();
    vec![
        gemini_dir.join("antigravity").join("conversations"),
        gemini_dir.join("antigravity-local").join("conversations"),
        gemini_dir.join("antigravity-cli").join("conversations"),
        gemini_dir.join("antigravity-ide").join("conversations"),
    ]
}

/// 扫描三端 conversations/*.db 规范路径，收集会话元数据
pub fn scan_sessions() -> Vec<SessionMeta> {
    let dirs = get_conversation_dirs();
    let mut sessions = Vec::new();
    let mut seen_canonical = HashSet::new();

    for dir in &dirs {
        if !dir.is_dir() {
            continue;
        }

        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }

            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            if !seen_canonical.insert(canonical) {
                continue;
            }

            if let Some(meta) = parse_session(&path) {
                sessions.push(meta);
            }
        }
    }

    // 兼容历史 legacy tmp 目录 (tmp/<project_name>/chats/session-*.json)
    let gemini_dir = crate::gemini_config::get_gemini_dir();
    let tmp_dir = gemini_dir.join("tmp");
    if tmp_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&tmp_dir) {
            for entry in entries.flatten() {
                let chats_dir = entry.path().join("chats");
                if !chats_dir.is_dir() {
                    continue;
                }
                let project_root_file = entry.path().join(".project_root");
                let project_dir = fs::read_to_string(project_root_file).ok();

                if let Ok(chat_files) = fs::read_dir(&chats_dir) {
                    for file_entry in chat_files.flatten() {
                        let path = file_entry.path();
                        if path.extension().and_then(|e| e.to_str()) != Some("json") {
                            continue;
                        }
                        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                        if !seen_canonical.insert(canonical) {
                            continue;
                        }
                        if let Some(meta) = parse_session(&path) {
                            sessions.push(SessionMeta {
                                project_dir: project_dir.clone().or(meta.project_dir),
                                ..meta
                            });
                        }
                    }
                }
            }
        }
    }

    sessions.sort_by(|a, b| {
        let a_ts = a.last_active_at.or(a.created_at).unwrap_or(0);
        let b_ts = b.last_active_at.or(b.created_at).unwrap_or(0);
        b_ts.cmp(&a_ts)
    });

    sessions
}

/// 读取会话消息详情
pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    if !path.exists() {
        return Err(format!("Session file does not exist: {}", path.display()));
    }

    // 历史 legacy JSON 会话格式
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        return load_messages_json(path);
    }

    load_messages_sqlite(path)
}

/// 删除会话：删除 .db、-wal、-shm 及 brain/<session_id> 目录
pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    if !path.exists() {
        return Err(format!("Session file not found: {}", path.display()));
    }

    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        return delete_session_json(path, session_id);
    }

    delete_session_sqlite(root, path, session_id)
}

// ============================================================================
// SQLite 会话解析核心
// ============================================================================

/// 解析会话元数据（SQLite 或历史 JSON）
pub fn parse_session(path: &Path) -> Option<SessionMeta> {
    if !path.exists() {
        return None;
    }

    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        return parse_session_json(path);
    }

    parse_session_sqlite(path)
}

fn open_readonly_db(path: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(5000));
    Some(conn)
}

fn parse_session_sqlite(path: &Path) -> Option<SessionMeta> {
    let conn = open_readonly_db(path)?;

    // 检查 steps 表是否存在
    let has_steps: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'steps')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !has_steps {
        return None;
    }

    // 提取 session_id（优先取 trajectory_meta 的 cascade_id，否则取文件名无后缀）
    let file_stem = path.file_stem().and_then(|s| s.to_str())?.to_string();
    let mut session_id = file_stem.clone();

    let has_trajectory_meta: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'trajectory_meta')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if has_trajectory_meta {
        if let Ok((cascade, traj)) = conn.query_row(
            "SELECT cascade_id, trajectory_id FROM trajectory_meta LIMIT 1",
            [],
            |row| {
                let c: Option<String> = row.get(0).ok();
                let t: Option<String> = row.get(1).ok();
                Ok((c, t))
            },
        ) {
            if let Some(c) = cascade.filter(|s| !s.trim().is_empty()) {
                session_id = c;
            } else if let Some(t) = traj.filter(|s| !s.trim().is_empty()) {
                session_id = t;
            }
        }
    }

    // 提取工作区路径
    let project_dir = extract_project_dir_from_db(&conn);

    // 查询 steps
    let mut stmt = conn
        .prepare("SELECT idx, step_type, metadata, step_payload FROM steps ORDER BY idx ASC")
        .ok()?;

    let rows = stmt
        .query_map([], |row| {
            let idx: i64 = row.get(0)?;
            let step_type: i64 = row.get(1)?;
            let metadata: Option<Vec<u8>> = row.get(2)?;
            let payload: Option<Vec<u8>> = row.get(3)?;
            Ok((idx, step_type, metadata, payload))
        })
        .ok()?;

    let mut first_ts: Option<i64> = None;
    let mut last_ts: Option<i64> = None;
    let mut first_prompt: Option<String> = None;
    let mut total_steps = 0;

    for row in rows.flatten() {
        let (_idx, step_type, metadata, payload) = row;
        total_steps += 1;

        if let Some(ref meta_blob) = metadata {
            if let Some(ts) = extract_step_timestamp(meta_blob) {
                if first_ts.is_none() {
                    first_ts = Some(ts);
                }
                last_ts = Some(ts);
            }
        }

        if first_prompt.is_none() && step_type == 14 {
            if let Some(ref pay_blob) = payload {
                if let Some(prompt) = extract_user_prompt_from_payload(pay_blob) {
                    first_prompt = Some(truncate_summary(&prompt, 160));
                }
            }
        }
    }

    // 时间戳优雅回退到文件 mtime
    if first_ts.is_none() {
        if let Ok(meta) = fs::metadata(path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                    let ms = duration.as_millis() as i64;
                    first_ts = Some(ms);
                    last_ts = Some(ms);
                }
            }
        }
    }

    let title = first_prompt.or_else(|| {
        if total_steps > 0 {
            Some(format!("Session {session_id}"))
        } else {
            None
        }
    });

    let source_path = path.to_string_lossy().to_string();

    Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: session_id.clone(),
        title: title.clone(),
        summary: title,
        project_dir,
        created_at: first_ts,
        last_active_at: last_ts.or(first_ts),
        source_path: Some(source_path),
        resume_command: Some(format!("agy --conversation {session_id}")),
    })
}

fn load_messages_sqlite(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let conn = open_readonly_db(path)
        .ok_or_else(|| format!("Failed to open SQLite database: {}", path.display()))?;

    let mut stmt = conn
        .prepare("SELECT idx, step_type, metadata, step_payload FROM steps ORDER BY idx ASC")
        .map_err(|e| format!("Failed to prepare query on steps: {e}"))?;

    let rows = stmt
        .query_map([], |row| {
            let idx: i64 = row.get(0)?;
            let step_type: i64 = row.get(1)?;
            let metadata: Option<Vec<u8>> = row.get(2)?;
            let payload: Option<Vec<u8>> = row.get(3)?;
            Ok((idx, step_type, metadata, payload))
        })
        .map_err(|e| format!("Failed to query steps: {e}"))?;

    let mut result = Vec::new();

    for row in rows.flatten() {
        let (_idx, step_type, metadata, payload) = row;
        let ts = metadata.as_deref().and_then(extract_step_timestamp);

        match step_type {
            14 => {
                // 用户提问
                if let Some(pay) = payload.as_deref() {
                    if let Some(content) = extract_user_prompt_from_payload(pay) {
                        result.push(SessionMessage {
                            role: "user".to_string(),
                            content,
                            ts,
                        });
                    }
                }
            }
            15 => {
                // 助手回复及工具调用
                if let Some(pay) = payload.as_deref() {
                    if let Some(content) = extract_assistant_content_from_payload(pay) {
                        result.push(SessionMessage {
                            role: "assistant".to_string(),
                            content,
                            ts,
                        });
                    }
                }
            }
            17 => {
                // 异常/终止消息
                if let Some(pay) = payload.as_deref() {
                    if let Some(content) = extract_error_content_from_payload(pay) {
                        result.push(SessionMessage {
                            role: "assistant".to_string(),
                            content,
                            ts,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    Ok(result)
}

fn delete_session_sqlite(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    // 验证 session_id 归属性
    let file_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let mut matches = file_stem == session_id;

    if !matches {
        if let Some(conn) = open_readonly_db(path) {
            let cascade_id: Option<String> = conn
                .query_row(
                    "SELECT cascade_id FROM trajectory_meta LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .ok();
            if cascade_id.as_deref() == Some(session_id) {
                matches = true;
            }
        }
    }

    if !matches {
        return Err(format!(
            "Gemini session ID mismatch: expected {session_id}, file stem is {file_stem}"
        ));
    }

    // 1. 删除主 .db 文件
    fs::remove_file(path).map_err(|e| {
        format!(
            "Failed to delete Gemini session file {}: {e}",
            path.display()
        )
    })?;

    // 2. 删除 -wal / -shm 文件（支持两种常见命名格式）
    let wal1 = path.with_extension("db-wal");
    let wal2 = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    if wal1.exists() {
        let _ = fs::remove_file(&wal1);
    }
    if wal2.exists() {
        let _ = fs::remove_file(&wal2);
    }

    let shm1 = path.with_extension("db-shm");
    let shm2 = PathBuf::from(format!("{}-shm", path.to_string_lossy()));
    if shm1.exists() {
        let _ = fs::remove_file(&shm1);
    }
    if shm2.exists() {
        let _ = fs::remove_file(&shm2);
    }

    // 3. 删除关联的 brain/<session_id> 目录
    // 3.1 当前 conversations/ 同级或父级 brain 目录
    if let Some(parent) = path.parent().and_then(|p| p.parent()) {
        let brain_dir = parent.join("brain").join(session_id);
        if brain_dir.is_dir() {
            let _ = fs::remove_dir_all(&brain_dir);
        }
    }

    // 3.2 根目录测试环境下的 brain 目录
    let root_brain = root.join("brain").join(session_id);
    if root_brain.is_dir() {
        let _ = fs::remove_dir_all(&root_brain);
    }

    // 3.3 全局三端目录下对应的 brain 目录
    let gemini_dir = crate::gemini_config::get_gemini_dir();
    for client in &[
        "antigravity",
        "antigravity-local",
        "antigravity-cli",
        "antigravity-ide",
    ] {
        let b = gemini_dir.join(client).join("brain").join(session_id);
        if b.is_dir() {
            let _ = fs::remove_dir_all(&b);
        }
    }

    Ok(true)
}

// ============================================================================
// Wire Format 与 Protobuf 字段提取辅助
// ============================================================================

/// 提取用户输入提问（过滤内部 <USER_REQUEST> 包装标签）
pub fn extract_user_prompt_from_payload(payload: &[u8]) -> Option<String> {
    let fields = parse_wire_fields(payload).ok()?;
    for f in fields {
        if f.tag == 19 && f.wire_type == 2 {
            if let Ok(subfields) = parse_wire_fields(f.data) {
                for sf in subfields {
                    if sf.tag == 2 && sf.wire_type == 2 {
                        if let Ok(s) = std::str::from_utf8(sf.data) {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                return Some(clean_user_request(trimmed));
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// 清洗 <USER_REQUEST> 与其它标签，提取真实用户提问
pub fn clean_user_request(text: &str) -> String {
    let mut s = text.trim();
    if let Some(start) = s.find("<USER_REQUEST>") {
        let after = &s[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            s = after[..end].trim();
        } else {
            s = after.trim();
        }
    }
    s.to_string()
}

/// 提取步骤的时间戳（秒 + 纳秒转毫秒）
pub fn extract_step_timestamp(metadata: &[u8]) -> Option<i64> {
    let fields = parse_wire_fields(metadata).ok()?;
    for f in fields {
        if f.tag == 1 && f.wire_type == 2 {
            if let Ok(subfields) = parse_wire_fields(f.data) {
                let mut sec = 0i64;
                let mut nanos = 0i64;
                for sf in subfields {
                    if sf.tag == 1 && sf.wire_type == 0 {
                        sec = sf.varint as i64;
                    } else if sf.tag == 2 && sf.wire_type == 0 {
                        nanos = sf.varint as i64;
                    }
                }
                if sec > 0 {
                    return Some(sec * 1000 + nanos / 1_000_000);
                }
            }
        }
    }
    None
}

/// 提取助手正文、工具调用与思考内容
pub fn extract_assistant_content_from_payload(payload: &[u8]) -> Option<String> {
    let fields = parse_wire_fields(payload).ok()?;
    let mut text = String::new();
    let mut tools = Vec::new();
    let mut thinking = String::new();

    for f in fields {
        if f.tag == 20 && f.wire_type == 2 {
            if let Ok(subfields) = parse_wire_fields(f.data) {
                for sf in subfields {
                    if (sf.tag == 1 || sf.tag == 8) && sf.wire_type == 2 && text.is_empty() {
                        if let Ok(s) = std::str::from_utf8(sf.data) {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                text = trimmed.to_string();
                            }
                        }
                    } else if sf.tag == 3 && sf.wire_type == 2 && thinking.is_empty() {
                        if let Ok(s) = std::str::from_utf8(sf.data) {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                thinking = trimmed.to_string();
                            }
                        }
                    } else if sf.tag == 7 && sf.wire_type == 2 {
                        if let Ok(call_fields) = parse_wire_fields(sf.data) {
                            for cf in call_fields {
                                if cf.tag == 2 && cf.wire_type == 2 {
                                    if let Ok(tname) = std::str::from_utf8(cf.data) {
                                        let trimmed = tname.trim();
                                        if !trimmed.is_empty()
                                            && !tools.contains(&trimmed.to_string())
                                        {
                                            tools.push(trimmed.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(text);
    } else if !thinking.is_empty() && tools.is_empty() {
        parts.push(thinking);
    }

    for t in tools {
        parts.push(format!("[Tool: {t}]"));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// 提取错误或终止提示内容
pub fn extract_error_content_from_payload(payload: &[u8]) -> Option<String> {
    let fields = parse_wire_fields(payload).ok()?;
    let mut parts = Vec::new();

    for f in fields {
        if f.tag == 24 && f.wire_type == 2 {
            if let Ok(subfields) = parse_wire_fields(f.data) {
                for sf in subfields {
                    if (sf.tag == 1 || sf.tag == 2) && sf.wire_type == 2 {
                        if let Ok(s) = std::str::from_utf8(sf.data) {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() && !parts.contains(&trimmed.to_string()) {
                                parts.push(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// 从数据库 trajectory_metadata_blob 提取项目目录
pub fn extract_project_dir_from_db(conn: &Connection) -> Option<String> {
    let has_blob: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'trajectory_metadata_blob')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !has_blob {
        return None;
    }

    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    if let Some(data) = blob {
        if let Ok(fields) = parse_wire_fields(&data) {
            for f in fields {
                if f.tag == 7 && f.wire_type == 2 {
                    if let Ok(s) = std::str::from_utf8(f.data) {
                        if let Ok(url) = url::Url::parse(s) {
                            if let Ok(path) = url.to_file_path() {
                                return Some(path.to_string_lossy().to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

// ============================================================================
// 兼容历史 JSON 格式
// ============================================================================

fn parse_session_json(path: &Path) -> Option<SessionMeta> {
    let data = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&data).ok()?;

    let session_id = value.get("sessionId").and_then(Value::as_str)?.to_string();

    let created_at = value.get("startTime").and_then(parse_timestamp_to_ms);
    let last_active_at = value.get("lastUpdated").and_then(parse_timestamp_to_ms);

    let title = value
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m.get("type").and_then(Value::as_str) == Some("user"))
                .and_then(|m| m.get("content").and_then(Value::as_str))
                .filter(|s| !s.trim().is_empty())
                .map(|s| truncate_summary(s, 160))
        });

    let source_path = path.to_string_lossy().to_string();

    Some(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: session_id.clone(),
        title: title.clone(),
        summary: title,
        project_dir: None,
        created_at,
        last_active_at: last_active_at.or(created_at),
        source_path: Some(source_path),
        resume_command: Some(format!("agy --conversation {session_id}")),
    })
}

fn load_messages_json(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let data = fs::read_to_string(path).map_err(|e| format!("Failed to read session: {e}"))?;
    let value: Value =
        serde_json::from_str(&data).map_err(|e| format!("Failed to parse session JSON: {e}"))?;

    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "No messages array found".to_string())?;

    let mut result = Vec::new();
    for msg in messages {
        let role = match msg.get("type").and_then(Value::as_str) {
            Some("gemini") => "assistant",
            Some("user") => "user",
            Some(_) | None => continue,
        };

        let mut content = match msg.get("content") {
            Some(Value::String(s)) => s.to_string(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };

        if let Some(Value::Array(calls)) = msg.get("toolCalls") {
            for call in calls {
                if let Some(name) = call.get("name").and_then(Value::as_str) {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&format!("[Tool: {name}]"));
                }
            }
        }

        if content.trim().is_empty() {
            continue;
        }

        let ts = msg.get("timestamp").and_then(parse_timestamp_to_ms);

        result.push(SessionMessage {
            role: role.to_string(),
            content,
            ts,
        });
    }

    Ok(result)
}

fn delete_session_json(path: &Path, session_id: &str) -> Result<bool, String> {
    let meta = parse_session_json(path).ok_or_else(|| {
        format!(
            "Failed to parse Gemini session metadata: {}",
            path.display()
        )
    })?;

    if meta.session_id != session_id {
        return Err(format!(
            "Gemini session ID mismatch: expected {session_id}, found {}",
            meta.session_id
        ));
    }

    fs::remove_file(path).map_err(|e| {
        format!(
            "Failed to delete Gemini session file {}: {e}",
            path.display()
        )
    })?;

    Ok(true)
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn encode_varint(mut val: u64, buf: &mut Vec<u8>) {
        while val >= 0x80 {
            buf.push(((val & 0x7F) as u8) | 0x80);
            val >>= 7;
        }
        buf.push(val as u8);
    }

    fn encode_varint_field(tag: u32, val: u64, buf: &mut Vec<u8>) {
        encode_varint((tag as u64) << 3, buf);
        encode_varint(val, buf);
    }

    fn encode_len_delimited_field(tag: u32, data: &[u8], buf: &mut Vec<u8>) {
        encode_varint(((tag as u64) << 3) | 2, buf);
        encode_varint(data.len() as u64, buf);
        buf.extend_from_slice(data);
    }

    fn build_test_metadata(sec: i64, nanos: i64) -> Vec<u8> {
        let mut sub = Vec::new();
        encode_varint_field(1, sec as u64, &mut sub);
        encode_varint_field(2, nanos as u64, &mut sub);

        let mut out = Vec::new();
        encode_len_delimited_field(1, &sub, &mut out);
        out
    }

    fn build_test_user_payload(prompt: &str) -> Vec<u8> {
        let mut sub = Vec::new();
        encode_len_delimited_field(2, prompt.as_bytes(), &mut sub);

        let mut top = Vec::new();
        encode_len_delimited_field(19, &sub, &mut top);
        top
    }

    fn build_test_assistant_payload(text: &str, tool: Option<&str>) -> Vec<u8> {
        let mut sub = Vec::new();
        if !text.is_empty() {
            encode_len_delimited_field(1, text.as_bytes(), &mut sub);
        }
        if let Some(tname) = tool {
            let mut tool_sub = Vec::new();
            encode_len_delimited_field(2, tname.as_bytes(), &mut tool_sub);
            encode_len_delimited_field(7, &tool_sub, &mut sub);
        }

        let mut top = Vec::new();
        encode_len_delimited_field(20, &sub, &mut top);
        top
    }

    fn build_test_error_payload(err_msg: &str) -> Vec<u8> {
        let mut sub = Vec::new();
        encode_len_delimited_field(1, err_msg.as_bytes(), &mut sub);

        let mut top = Vec::new();
        encode_len_delimited_field(24, &sub, &mut top);
        top
    }

    fn create_test_db(path: &Path, session_id: &str) -> Connection {
        let conn = Connection::open(path).expect("create test db");
        conn.execute_batch(
            "
            CREATE TABLE trajectory_meta (
                trajectory_id TEXT,
                cascade_id TEXT,
                trajectory_type INTEGER,
                source INTEGER
            );
            CREATE TABLE steps (
                idx INTEGER PRIMARY KEY,
                step_type INTEGER,
                status INTEGER,
                metadata BLOB,
                step_payload BLOB
            );
            CREATE TABLE trajectory_metadata_blob (
                id TEXT PRIMARY KEY,
                data BLOB
            );
            ",
        )
        .expect("create tables");

        conn.execute(
            "INSERT INTO trajectory_meta (trajectory_id, cascade_id, trajectory_type, source) VALUES (?1, ?2, 4, 17)",
            [format!("traj-{session_id}"), session_id.to_string()],
        )
        .expect("insert trajectory_meta");

        conn
    }

    #[test]
    fn test_multiclient_scan_deduplication() {
        let _guard = test_lock().lock().unwrap();
        let temp = tempdir().expect("tempdir");

        let cli_dir = temp.path().join("antigravity-cli").join("conversations");
        let ide_dir = temp.path().join("antigravity-ide").join("conversations");
        let local_dir = temp.path().join("antigravity-local").join("conversations");

        fs::create_dir_all(&cli_dir).unwrap();
        fs::create_dir_all(&ide_dir).unwrap();
        fs::create_dir_all(&local_dir).unwrap();

        // 创建 cli 会话
        let db1 = cli_dir.join("session-cli-1.db");
        let conn1 = create_test_db(&db1, "session-cli-1");
        conn1
            .execute(
                "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (0, 14, ?1, ?2)",
                (
                    build_test_metadata(1700000000, 0),
                    build_test_user_payload("Hello from CLI"),
                ),
            )
            .unwrap();

        // 创建 ide 会话
        let db2 = ide_dir.join("session-ide-2.db");
        let conn2 = create_test_db(&db2, "session-ide-2");
        conn2
            .execute(
                "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (0, 14, ?1, ?2)",
                (
                    build_test_metadata(1700000100, 0),
                    build_test_user_payload("Hello from IDE"),
                ),
            )
            .unwrap();

        // 创建 local 会话，并在 antigravity 建立软链以验证去重
        let db3 = local_dir.join("session-local-3.db");
        let conn3 = create_test_db(&db3, "session-local-3");
        conn3
            .execute(
                "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (0, 14, ?1, ?2)",
                (
                    build_test_metadata(1700000200, 0),
                    build_test_user_payload("Hello from Local"),
                ),
            )
            .unwrap();

        let link_dir = temp.path().join("antigravity").join("conversations");
        fs::create_dir_all(temp.path().join("antigravity")).unwrap();
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&local_dir, &link_dir);

        let env_dirs = std::env::join_paths([&cli_dir, &ide_dir, &local_dir, &link_dir]).unwrap();
        std::env::set_var("ANTIGRAVITY_CONVERSATIONS_DIRS", &env_dirs);

        let sessions = scan_sessions();
        std::env::remove_var("ANTIGRAVITY_CONVERSATIONS_DIRS");

        // 验证去重：3 个唯一会话，无软链重复项
        assert_eq!(sessions.len(), 3);
        let ids: HashSet<_> = sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert!(ids.contains("session-cli-1"));
        assert!(ids.contains("session-ide-2"));
        assert!(ids.contains("session-local-3"));
    }

    #[test]
    fn test_prompt_summary_extraction() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("session-test-summary.db");
        let conn = create_test_db(&db_path, "session-test-summary");

        // 插入带 <USER_REQUEST> 包装标签的首条提问
        let wrapped_prompt = "<USER_REQUEST>\n请问 Antigravity 插件如何配置？\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>";
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (0, 14, ?1, ?2)",
            (
                build_test_metadata(1720000000, 123456789),
                build_test_user_payload(wrapped_prompt),
            ),
        )
        .unwrap();

        // 插入第二步助手回答
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (1, 15, ?1, ?2)",
            (
                build_test_metadata(1720000010, 0),
                build_test_assistant_payload("配置说明如下", Some("read_config")),
            ),
        )
        .unwrap();

        let meta = parse_session(&db_path).expect("parse session");
        assert_eq!(meta.session_id, "session-test-summary");
        assert_eq!(
            meta.title.as_deref(),
            Some("请问 Antigravity 插件如何配置？")
        );
        assert_eq!(
            meta.summary.as_deref(),
            Some("请问 Antigravity 插件如何配置？")
        );
        assert_eq!(meta.created_at, Some(1720000000123));
        assert_eq!(meta.last_active_at, Some(1720000010000));
        assert_eq!(
            meta.resume_command.as_deref(),
            Some("agy --conversation session-test-summary")
        );
    }

    #[test]
    fn test_nonexistent_and_corrupt_files_tolerance() {
        let temp = tempdir().expect("tempdir");
        let nonexistent = temp.path().join("does-not-exist.db");

        // 不存在的文件不报错、不崩溃
        assert!(parse_session(&nonexistent).is_none());
        assert!(load_messages(&nonexistent).is_err());
        assert!(delete_session(temp.path(), &nonexistent, "any-id").is_err());

        // 损坏或非 SQLite 文件容错
        let corrupt = temp.path().join("corrupt.db");
        fs::write(&corrupt, b"not a real sqlite database").unwrap();
        assert!(parse_session(&corrupt).is_none());
        assert!(load_messages(&corrupt).is_err());
    }

    #[test]
    fn test_delete_session_cleans_db_wal_shm_and_brain() {
        let temp = tempdir().expect("tempdir");
        let conv_dir = temp.path().join("conversations");
        let brain_dir = temp.path().join("brain").join("test-del-123");
        fs::create_dir_all(&conv_dir).unwrap();
        fs::create_dir_all(&brain_dir).unwrap();

        // 写入 brain 目录内容
        fs::write(brain_dir.join("task.json"), b"{\"state\":\"done\"}").unwrap();

        // 创建 .db 及 -wal, -shm 文件
        let db_path = conv_dir.join("test-del-123.db");
        let _conn = create_test_db(&db_path, "test-del-123");
        let wal_path = conv_dir.join("test-del-123.db-wal");
        let shm_path = conv_dir.join("test-del-123.db-shm");
        fs::write(&wal_path, b"wal data").unwrap();
        fs::write(&shm_path, b"shm data").unwrap();

        assert!(db_path.exists());
        assert!(wal_path.exists());
        assert!(shm_path.exists());
        assert!(brain_dir.exists());

        let res = delete_session(temp.path(), &db_path, "test-del-123");
        assert!(res.is_ok());

        assert!(!db_path.exists());
        assert!(!wal_path.exists());
        assert!(!shm_path.exists());
        assert!(!brain_dir.exists());
    }

    #[test]
    fn test_load_messages_from_steps() {
        let temp = tempdir().expect("tempdir");
        let db_path = temp.path().join("session-msgs.db");
        let conn = create_test_db(&db_path, "session-msgs");

        // 插入用户消息
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (0, 14, ?1, ?2)",
            (
                build_test_metadata(1700000001, 0),
                build_test_user_payload("你好，帮我写个代码"),
            ),
        )
        .unwrap();

        // 插入助手消息（带工具调用）
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (1, 15, ?1, ?2)",
            (
                build_test_metadata(1700000005, 0),
                build_test_assistant_payload("好，我正在读取文件", Some("view_file")),
            ),
        )
        .unwrap();

        // 插入错误/终止消息
        conn.execute(
            "INSERT INTO steps (idx, step_type, metadata, step_payload) VALUES (2, 17, ?1, ?2)",
            (
                build_test_metadata(1700000010, 0),
                build_test_error_payload("执行失败: 400 Bad Request"),
            ),
        )
        .unwrap();

        let msgs = load_messages(&db_path).expect("load messages");
        assert_eq!(msgs.len(), 3);

        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content, "你好，帮我写个代码");
        assert_eq!(msgs[0].ts, Some(1700000001000));

        assert_eq!(msgs[1].role, "assistant");
        assert!(msgs[1].content.contains("好，我正在读取文件"));
        assert!(msgs[1].content.contains("[Tool: view_file]"));
        assert_eq!(msgs[1].ts, Some(1700000005000));

        assert_eq!(msgs[2].role, "assistant");
        assert!(msgs[2].content.contains("执行失败: 400 Bad Request"));
        assert_eq!(msgs[2].ts, Some(1700000010000));
    }

    #[test]
    fn test_legacy_json_backward_compatibility() {
        let temp = tempdir().expect("tempdir");
        let path = temp.path().join("session-legacy.json");
        fs::write(
            &path,
            r#"{
              "sessionId": "gemini-legacy-123",
              "startTime": "2026-03-06T10:17:58.000Z",
              "lastUpdated": "2026-03-06T10:20:00.000Z",
              "messages": [
                {
                  "id": "msg-1",
                  "timestamp": "2026-03-06T10:17:58.000Z",
                  "type": "user",
                  "content": "hello legacy"
                },
                {
                  "id": "msg-2",
                  "timestamp": "2026-03-06T10:18:00.000Z",
                  "type": "gemini",
                  "content": "hello response"
                }
              ]
            }"#,
        )
        .expect("write session");

        let meta = parse_session(&path).expect("parse legacy session");
        assert_eq!(meta.session_id, "gemini-legacy-123");
        assert_eq!(meta.title.as_deref(), Some("hello legacy"));

        let msgs = load_messages(&path).expect("load legacy messages");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");

        let del = delete_session(temp.path(), &path, "gemini-legacy-123");
        assert!(del.is_ok());
        assert!(!path.exists());
    }

    #[test]
    fn test_live_scan_antigravity_sessions() {
        let roots = session_roots();
        let sessions = scan_sessions();
        println!("\n>>> LIVE_SCAN_ANTIGRAVITY_COUNT: {}", sessions.len());
        for (i, s) in sessions.iter().take(5).enumerate() {
            println!(
                "  [{}] id: {}, title: {:?}, project: {:?}, created: {:?}, active: {:?}",
                i + 1,
                s.session_id,
                s.title,
                s.project_dir,
                s.created_at,
                s.last_active_at
            );
        }
        let has_real_dbs = roots.iter().any(|r| {
            if let Ok(entries) = fs::read_dir(r) {
                entries
                    .flatten()
                    .any(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("db"))
            } else {
                false
            }
        });
        if has_real_dbs {
            assert!(
                !sessions.is_empty(),
                "Expected > 0 live Antigravity sessions on local machine"
            );
        }
    }
}
