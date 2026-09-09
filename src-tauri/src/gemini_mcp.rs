use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::atomic_write;
use crate::error::AppError;
use crate::gemini_config::get_gemini_settings_path;

/// 获取 Gemini / Antigravity settings.json 配置文件路径
pub fn user_config_path() -> PathBuf {
    get_gemini_settings_path()
}

/// 获取 Antigravity IDE 配置目录
pub fn get_antigravity_ide_dir() -> PathBuf {
    crate::gemini_config::get_gemini_dir().join("antigravity-ide")
}

/// 获取 Antigravity IDE mcp_config.json 文件路径
pub fn get_antigravity_ide_mcp_path() -> PathBuf {
    get_antigravity_ide_dir().join("mcp_config.json")
}

fn read_json_value(path: &Path) -> Result<Value, AppError> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let content = fs::read_to_string(path).map_err(|e| AppError::io(path, e))?;
    let value: Value = serde_json::from_str(&content).map_err(|e| AppError::json(path, e))?;
    Ok(value)
}

fn write_json_value(path: &Path, value: &Value) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    let json =
        serde_json::to_string_pretty(value).map_err(|e| AppError::JsonSerialize { source: e })?;
    atomic_write(path, json.as_bytes())
}

/// 递归解包并修复任何多层嵌套的 mcpServers 结构，过滤非对象值
pub fn unwrap_nested_mcp_servers(obj: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in obj {
        if k == "mcpServers" {
            if let Some(nested) = v.as_object() {
                let unnested = unwrap_nested_mcp_servers(nested);
                for (nk, nv) in unnested {
                    if nk != "mcpServers" {
                        out.insert(nk, nv);
                    }
                }
                continue;
            }
        }
        // MCP 服务器规范必须是对象类型（过滤 timeout 等顶层标量污染）
        if v.is_object() {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// 读取并合并 Gemini / Antigravity MCP 配置
///
/// 从 settings.json 与 antigravity-ide/mcp_config.json 读取：
/// - 导入时优先合并 antigravity-ide 的配置项
/// - 修复/扁平化任何可能的双重嵌套
/// - 补齐统一 MCP 格式所需要的 type 字段
pub fn read_mcp_servers_map() -> Result<HashMap<String, Value>, AppError> {
    read_mcp_servers_map_from_paths(&user_config_path(), &get_antigravity_ide_mcp_path())
}

/// 指定路径读取并合并 MCP 配置（便于测试与多路径调用）
pub fn read_mcp_servers_map_from_paths(
    settings_path: &Path,
    ide_mcp_path: &Path,
) -> Result<HashMap<String, Value>, AppError> {
    let mut servers: HashMap<String, Value> = HashMap::new();

    // 1. 读取 settings.json (若存在)
    if settings_path.exists() {
        let root = read_json_value(settings_path)?;
        if let Some(obj) = root.get("mcpServers").and_then(Value::as_object) {
            let unnested = unwrap_nested_mcp_servers(obj);
            for (k, v) in unnested {
                servers.insert(k, v);
            }
        }
    }

    // 2. 读取 antigravity-ide/mcp_config.json（优先合并）
    if ide_mcp_path.exists() {
        let ide_root = read_json_value(ide_mcp_path)?;
        let ide_obj = if let Some(obj) = ide_root.get("mcpServers").and_then(Value::as_object) {
            Some(obj)
        } else if ide_root.is_object() {
            ide_root.as_object()
        } else {
            None
        };

        if let Some(obj) = ide_obj {
            let unnested = unwrap_nested_mcp_servers(obj);
            for (k, v) in unnested {
                // IDE 端配置优先覆盖
                servers.insert(k, v);
            }
        }
    }

    // 3. 反向格式转换：Gemini / Antigravity 特有格式 → 统一 MCP 格式
    for (_, spec) in servers.iter_mut() {
        if let Some(obj) = spec.as_object_mut() {
            // httpUrl → url + type: "http"
            if let Some(http_url) = obj.remove("httpUrl") {
                obj.insert("url".to_string(), http_url);
                obj.insert("type".to_string(), Value::String("http".to_string()));
            }

            // Antigravity / Gemini CLI 不使用 type 字段：补齐便于统一校验与管理
            if obj.get("type").is_none() {
                if obj.contains_key("command") {
                    obj.insert("type".to_string(), Value::String("stdio".to_string()));
                } else if obj.contains_key("url") {
                    obj.insert("type".to_string(), Value::String("sse".to_string()));
                }
            }
        }
    }

    Ok(servers)
}

/// 将已启用的 MCP 服务器写入 settings.json，若 antigravity-ide 存在则同步原子写入 mcp_config.json
pub fn set_mcp_servers_map(servers: &HashMap<String, Value>) -> Result<(), AppError> {
    let ide_dir = get_antigravity_ide_dir();
    let ide_mcp_path = if ide_dir.is_dir() {
        Some(get_antigravity_ide_mcp_path())
    } else {
        None
    };

    set_mcp_servers_map_to_paths(servers, &user_config_path(), ide_mcp_path.as_deref())
}

/// 指定路径写入 MCP 服务器配置（支持原子双写与测试隔离）
pub fn set_mcp_servers_map_to_paths(
    servers: &HashMap<String, Value>,
    settings_path: &Path,
    ide_mcp_path: Option<&Path>,
) -> Result<(), AppError> {
    // 1. 构建标准化的 mcpServers 对象
    let mut out: Map<String, Value> = Map::new();
    for (id, spec) in servers.iter() {
        if id == "mcpServers" {
            continue;
        }

        let mut obj = if let Some(map) = spec.as_object() {
            map.clone()
        } else {
            return Err(AppError::McpValidation(format!(
                "MCP 服务器 '{id}' 不是对象"
            )));
        };

        // 提取 server 字段（如果存在）
        if let Some(server_val) = obj.remove("server") {
            let server_obj = server_val.as_object().cloned().ok_or_else(|| {
                AppError::McpValidation(format!("MCP 服务器 '{id}' server 字段不是对象"))
            })?;
            obj = server_obj;
        }

        // Antigravity / Gemini 格式转换：
        // - 不使用 "type" 字段（从字段名推断传输类型）
        // - HTTP 使用 "httpUrl" 字段，SSE 使用 "url" 字段
        let transport_type = obj.get("type").and_then(|v| v.as_str());
        if transport_type == Some("http") {
            if let Some(url_value) = obj.remove("url") {
                obj.insert("httpUrl".to_string(), url_value);
            }
        }

        // 移除 UI 辅助字段和 type 字段
        obj.remove("type");
        obj.remove("enabled");
        obj.remove("source");
        obj.remove("id");
        obj.remove("name");
        obj.remove("description");
        obj.remove("tags");
        obj.remove("homepage");
        obj.remove("docs");

        const DEFAULT_STARTUP_MS: u64 = 10_000;
        const DEFAULT_TOOL_MS: u64 = 60_000;

        let extract_timeout =
            |obj: &mut Map<String, Value>, key: &str, multiplier: u64| -> Option<u64> {
                obj.remove(key).and_then(|val| {
                    val.as_u64()
                        .map(|n| n * multiplier)
                        .or_else(|| val.as_f64().map(|f| (f * multiplier as f64) as u64))
                })
            };

        let startup_ms = extract_timeout(&mut obj, "startup_timeout_sec", 1000)
            .or_else(|| extract_timeout(&mut obj, "startup_timeout_ms", 1))
            .unwrap_or(DEFAULT_STARTUP_MS);
        let tool_ms = extract_timeout(&mut obj, "tool_timeout_sec", 1000)
            .or_else(|| extract_timeout(&mut obj, "tool_timeout_ms", 1))
            .unwrap_or(DEFAULT_TOOL_MS);

        let final_timeout = startup_ms.max(tool_ms);
        obj.insert("timeout".to_string(), Value::Number(final_timeout.into()));

        out.insert(id.clone(), Value::Object(obj));
    }

    // 2. 写入 ~/.gemini/settings.json
    let mut root = if settings_path.exists() {
        read_json_value(settings_path)?
    } else {
        serde_json::json!({})
    };

    if !root.is_object() {
        root = serde_json::json!({});
    }

    {
        let obj = root
            .as_object_mut()
            .ok_or_else(|| AppError::Config("~/.gemini/settings.json 根必须是对象".into()))?;
        obj.insert("mcpServers".into(), Value::Object(out.clone()));
    }
    write_json_value(settings_path, &root)?;

    // 3. 若 antigravity-ide 目录存在，原子同步写入 mcp_config.json
    if let Some(ide_path) = ide_mcp_path {
        let mut ide_root = if ide_path.exists() {
            read_json_value(ide_path).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        if !ide_root.is_object() {
            ide_root = serde_json::json!({});
        }

        if let Some(obj) = ide_root.as_object_mut() {
            obj.insert("mcpServers".into(), Value::Object(out));
        }

        write_json_value(ide_path, &ide_root)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_unwrap_nested_mcp_servers_fixes_double_and_triple_nesting() {
        // 测试双重嵌套修复
        let double_nested: Value = serde_json::json!({
            "mcpServers": {
                "server-a": {
                    "command": "uvx",
                    "args": ["server-a"]
                }
            },
            "timeout": 60000
        });

        let unnested = unwrap_nested_mcp_servers(double_nested.as_object().unwrap());
        assert_eq!(unnested.len(), 1);
        assert!(unnested.contains_key("server-a"));
        assert!(!unnested.contains_key("mcpServers"));
        assert!(!unnested.contains_key("timeout"));

        // 测试三重嵌套修复
        let triple_nested: Value = serde_json::json!({
            "mcpServers": {
                "mcpServers": {
                    "server-b": {
                        "command": "npx",
                        "args": ["server-b"]
                    }
                }
            }
        });

        let unnested3 = unwrap_nested_mcp_servers(triple_nested.as_object().unwrap());
        assert_eq!(unnested3.len(), 1);
        assert!(unnested3.contains_key("server-b"));
        assert!(!unnested3.contains_key("mcpServers"));
    }

    #[test]
    fn test_read_mcp_servers_map_ide_priority_merge() {
        let temp = tempdir().expect("tempdir");
        let settings_path = temp.path().join("settings.json");
        let ide_path = temp.path().join("mcp_config.json");

        // 写入 settings.json（包含共享 server 与仅 settings server，且带双重嵌套）
        fs::write(
            &settings_path,
            serde_json::json!({
                "mcpServers": {
                    "mcpServers": {
                        "shared-server": {
                            "command": "old-cmd",
                            "args": ["v1"]
                        },
                        "settings-only": {
                            "command": "settings-cmd"
                        }
                    }
                },
                "security": { "auth": { "selectedType": "oauth-personal" } }
            })
            .to_string(),
        )
        .unwrap();

        // 写入 ide mcp_config.json（覆盖 shared-server，并新增 ide-only）
        fs::write(
            &ide_path,
            serde_json::json!({
                "mcpServers": {
                    "shared-server": {
                        "command": "new-ide-cmd",
                        "args": ["v2"]
                    },
                    "ide-only": {
                        "command": "ide-cmd"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let map = read_mcp_servers_map_from_paths(&settings_path, &ide_path).expect("read");
        assert_eq!(map.len(), 3);

        // 验证 IDE 优先级
        let shared = map.get("shared-server").unwrap();
        assert_eq!(shared["command"], "new-ide-cmd");
        assert_eq!(shared["type"], "stdio");

        // 验证其余项被完整合并
        assert_eq!(map.get("settings-only").unwrap()["command"], "settings-cmd");
        assert_eq!(map.get("ide-only").unwrap()["command"], "ide-cmd");
    }

    #[test]
    fn test_set_mcp_servers_map_writes_both_and_preserves_other_keys() {
        let temp = tempdir().expect("tempdir");
        let settings_path = temp.path().join("settings.json");
        let ide_path = temp.path().join("mcp_config.json");

        // 预设 settings.json 带有 security 字段
        fs::write(
            &settings_path,
            serde_json::json!({
                "security": {
                    "auth": { "selectedType": "gemini-api-key" }
                }
            })
            .to_string(),
        )
        .unwrap();

        let mut servers = HashMap::new();
        servers.insert(
            "PaddleOCR-VL".to_string(),
            serde_json::json!({
                "command": "uvx",
                "args": ["--from", "paddleocr-mcp", "paddleocr_mcp"],
                "type": "stdio"
            }),
        );

        set_mcp_servers_map_to_paths(&servers, &settings_path, Some(&ide_path)).expect("write");

        // 验证 settings.json
        let settings_val: Value =
            serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            settings_val["security"]["auth"]["selectedType"],
            "gemini-api-key"
        );
        let s_mcp = settings_val["mcpServers"].as_object().unwrap();
        assert!(s_mcp.contains_key("PaddleOCR-VL"));
        assert!(!s_mcp.contains_key("mcpServers")); // 无双重嵌套

        // 验证 ide mcp_config.json
        let ide_val: Value = serde_json::from_str(&fs::read_to_string(&ide_path).unwrap()).unwrap();
        let ide_mcp = ide_val["mcpServers"].as_object().unwrap();
        assert!(ide_mcp.contains_key("PaddleOCR-VL"));
        assert_eq!(ide_mcp["PaddleOCR-VL"]["command"], "uvx");
    }
}
