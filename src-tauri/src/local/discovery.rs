//! 本地 MLX 模型发现
//!
//! 移植自 `mlx-gateway-studio` 的 `ModelDiscovery` / `ModelMetadataParser`：
//! 只做第 1 级元数据验证（目录含 `config.json` + 至少一个 `*.safetensors`），
//! 绝不为了建立列表而加载几十 GB 权重。

use crate::local::model::{classify_memory_fit, LocalModelRecord, LocalModelSource};
use crate::local::settings::LocalSettings;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 记录模型文件内容的轻量指纹（用于验证缓存失效）。
pub fn model_fingerprint(directory: &Path) -> Option<String> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    for name in ["config.json", "tokenizer_config.json", "tokenizer.json"] {
        if let Ok(bytes) = std::fs::read(directory.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(bytes);
        }
    }
    Some(format!("{:x}", hasher.finalize()))
}

/// 扫描所有根目录并合并结果（去重按解析后的绝对路径）。
pub fn discover_models(settings: &LocalSettings) -> Vec<LocalModelRecord> {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut roots: Vec<(PathBuf, LocalModelSource)> = vec![
        (
            home.join(".cache/lm-studio/models"),
            LocalModelSource::LmStudio,
        ),
        (
            home.join("Library/Application Support/LM Studio/models"),
            LocalModelSource::LmStudio,
        ),
        (
            home.join(".cache/huggingface/hub"),
            LocalModelSource::HuggingFace,
        ),
    ];
    for root in &settings.custom_model_roots {
        if !root.is_empty() {
            roots.push((PathBuf::from(root), LocalModelSource::Custom));
        }
    }

    let mut seen = HashSet::new();
    let mut records = Vec::new();
    for (root, source) in roots {
        if !root.is_dir() {
            continue;
        }
        if let Ok(dir) = root.canonicalize() {
            if let Some(record) = parse_model_dir(&dir, source) {
                if seen.insert(record.path.clone()) {
                    records.push(record);
                }
            }
            scan_recursive(&root, source, &mut seen, &mut records);
        }
    }
    records
}

/// 扫描单个根目录（深度 ≤ 8，跳过隐藏文件与符号链接目标）。
pub fn scan_root(root: &Path, source: LocalModelSource) -> Vec<LocalModelRecord> {
    let mut seen = HashSet::new();
    let mut records = Vec::new();
    if root.is_dir() {
        if let Some(record) = parse_model_dir(root, source) {
            seen.insert(record.path.clone());
            records.push(record);
        }
        scan_recursive(root, source, &mut seen, &mut records);
    }
    records
}

fn scan_recursive(
    root: &Path,
    source: LocalModelSource,
    seen: &mut HashSet<String>,
    records: &mut Vec<LocalModelRecord>,
) {
    let root_depth = root.components().count();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.is_symlink() {
            // Hugging Face cache 的 snapshot 语义：符号链接不递归展开
            // （共享 blob 去重交给 canonicalize 后的 path 去重）。
            continue;
        }
        let components = dir.components().count();
        if components - root_depth > 8 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                if path.join("config.json").is_file() {
                    if let Some(record) = parse_model_dir(&path, source) {
                        if seen.insert(record.path.clone()) {
                            records.push(record);
                        }
                    }
                    // 模型目录之下不再递归（避免把分片目录当模型）
                    continue;
                }
                stack.push(path);
            }
        }
    }
}

/// 解析一个模型目录：`config.json` + 至少一个 `.safetensors`。
///
/// 过滤规则（M0 为 Agent 对话模型服务）：
/// - 必须同时有 tokenizer 与有效 chat template（排除 FLUX 子目录、wav2vec 等非对话模型）；
/// - 权重体积 ≥ 100 MB（真实大小，跟随 HF 缓存的 symlink 解析）。
pub fn parse_model_dir(directory: &Path, source: LocalModelSource) -> Option<LocalModelRecord> {
    let canonical = directory.canonicalize().ok()?;
    let config_path = canonical.join("config.json");
    let config_data = std::fs::read(&config_path).ok()?;
    let json: Value = serde_json::from_slice(&config_data).ok()?;

    let mut weight_bytes: i64 = 0;
    let files = std::fs::read_dir(&canonical).ok()?;
    let has_weights: Vec<i64> = files
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("safetensors"))
                .unwrap_or(false)
        })
        // Path::metadata 会跟随符号链接（HF 缓存 snapshot 里的权重是 symlink）
        .map(|entry| {
            std::fs::metadata(entry.path())
                .map(|m| m.len() as i64)
                .unwrap_or(0)
        })
        .collect();
    if has_weights.is_empty() {
        return None;
    }
    for size in has_weights {
        weight_bytes += size;
    }

    let model_type = find_string(&json, "model_type").unwrap_or_else(|| "未知架构".to_string());
    let context_length = find_number(&json, "max_position_embeddings").map(|n| n as u32);
    let kv_bytes_per_token = estimate_kv_bytes_per_token(&json);
    let quantization = quantization_bits(&json)
        .map(|bits| format!("{bits}-bit"))
        .unwrap_or_else(|| infer_quantization(&canonical));
    let has_tokenizer = ["tokenizer.json", "tokenizer.model", "tokenizer_config.json"]
        .iter()
        .any(|name| canonical.join(name).is_file());
    let has_chat_template = chat_template_exists(&canonical);

    // M0 过滤：只有带 tokenizer + chat template 的对话模型入模型库
    if !has_tokenizer || !has_chat_template || weight_bytes < 100 * 1_048_576 {
        return None;
    }

    Some(LocalModelRecord {
        path: canonical.to_string_lossy().to_string(),
        name: display_name(&canonical, source),
        source,
        size_bytes: weight_bytes,
        model_type,
        quantization,
        context_length,
        kv_bytes_per_token,
        has_tokenizer,
        has_chat_template,
        memory_fit: classify_memory_fit(weight_bytes),
        verify_level: 0,
        fingerprint: model_fingerprint(&canonical),
        last_used_at: None,
    })
}

fn find_number(value: &Value, key: &str) -> Option<i64> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                if let Some(n) = v.as_i64() {
                    return Some(n);
                }
            }
            for nested in map.values() {
                if let Some(found) = find_number(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_number(v, key)),
        _ => None,
    }
}

fn find_string(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                if let Some(s) = v.as_str() {
                    return Some(s.to_string());
                }
            }
            for nested in map.values() {
                if let Some(found) = find_string(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_string(v, key)),
        _ => None,
    }
}

fn find_string_array(value: &Value, key: &str) -> Option<Vec<String>> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                if let Some(arr) = v.as_array() {
                    return Some(
                        arr.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect(),
                    );
                }
            }
            for nested in map.values() {
                if let Some(found) = find_string_array(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_string_array(v, key)),
        _ => None,
    }
}

/// 估算每 token KV 缓存字节（与 studio estimateKVBytesPerToken 一致）。
fn estimate_kv_bytes_per_token(json: &Value) -> Option<i64> {
    let total_layers = find_number(json, "num_hidden_layers")?;
    let kv_heads = find_number(json, "num_key_value_heads")?;

    let head_dimension = if let Some(direct) = find_number(json, "head_dim") {
        direct
    } else {
        let hidden = find_number(json, "hidden_size")?;
        let heads = find_number(json, "num_attention_heads").unwrap_or(1).max(1);
        hidden / heads
    };

    let full_attention_layers = if let Some(layer_types) = find_string_array(json, "layer_types") {
        layer_types
            .iter()
            .filter(|t| *t == "full_attention")
            .count() as i64
    } else if let Some(interval) = find_number(json, "full_attention_interval") {
        if interval > 0 {
            (total_layers / interval).max(1)
        } else {
            total_layers
        }
    } else {
        total_layers
    };

    let data_type = find_string(json, "dtype")
        .unwrap_or_else(|| "float16".to_string())
        .to_lowercase();
    let bytes_per_element: i64 = if data_type.contains("32") { 4 } else { 2 };
    Some(full_attention_layers * 2 * kv_heads * head_dimension * bytes_per_element)
}

fn quantization_bits(value: &Value) -> Option<i64> {
    let obj = value.as_object()?;
    for key in ["quantization", "quantization_config"] {
        if let Some(config) = obj.get(key) {
            if let Some(config_obj) = config.as_object() {
                if let Some(bits) = config_obj.get("bits").and_then(|b| b.as_i64()) {
                    return Some(bits);
                }
            }
        }
    }
    for nested in obj.values() {
        if let Some(bits) = quantization_bits(nested) {
            return Some(bits);
        }
    }
    None
}

/// 模型显示名：
/// - Hugging Face 缓存：`models--org--name/snapshots/<hash>` → 取仓库名（如 `Qwen3.8-27B-MLX-4bit`）
/// - 其他：`<父目录>/<目录>`（LM Studio 结构），根目录则用目录名
fn display_name(canonical: &Path, _source: LocalModelSource) -> String {
    let path_str = canonical.to_string_lossy();
    if path_str.contains("/hub/models--") {
        // 提取 models--org--name 中的 name（HF cache 规范段名）
        if let Some(idx) = path_str.find("models--") {
            let rest = &path_str[idx + "models--".len()..];
            // 格式：org--name/snapshots/...
            let repo_path = rest.split("/snapshots/").next().unwrap_or(rest);
            if let Some((org, name)) = repo_path.split_once("--") {
                if !org.is_empty() && !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    let parent = canonical
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string());
    let base = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path_str.to_string());
    match parent {
        Some(parent) if !parent.is_empty() => format!("{parent}/{base}"),
        _ => base,
    }
}

fn infer_quantization(directory: &Path) -> String {
    let lower = directory
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for bits in [2, 3, 4, 5, 6, 8] {
        if lower.contains(&format!("{bits}bit")) || lower.contains(&format!("{bits}-bit")) {
            return format!("{bits}-bit");
        }
    }
    "未标注".to_string()
}

fn chat_template_exists(directory: &Path) -> bool {
    // 优先内嵌字段；部分模型用独立 chat_template.jinja（如 Ornith）
    if directory.join("chat_template.jinja").is_file() {
        return true;
    }
    let Ok(data) = std::fs::read(directory.join("tokenizer_config.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<Value>(&data) else {
        return false;
    };
    match json.get("chat_template") {
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(arr)) => !arr.is_empty(),
        _ => false,
    }
}
