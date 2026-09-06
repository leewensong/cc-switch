//! 本地模型记录（`local_models` 表）
//!
//! 模型只建立**路径引用**与元数据索引，绝不复制/移动模型文件；
//! 外部引用（LM Studio / 自定义目录）只能"移除引用"。

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::local::settings::{physical_memory_bytes, recommended_prompt_cache_bytes};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 模型来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelSource {
    LmStudio,
    HuggingFace,
    Custom,
}

impl LocalModelSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            LocalModelSource::LmStudio => "lm_studio",
            LocalModelSource::HuggingFace => "hugging_face",
            LocalModelSource::Custom => "custom",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "lm_studio" => LocalModelSource::LmStudio,
            "hugging_face" => LocalModelSource::HuggingFace,
            _ => LocalModelSource::Custom,
        }
    }
}

/// 内存适配分级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryFit {
    Comfortable,
    Tight,
    TooLarge,
}

impl MemoryFit {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryFit::Comfortable => "comfortable",
            MemoryFit::Tight => "tight",
            MemoryFit::TooLarge => "too_large",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value {
            "tight" => MemoryFit::Tight,
            "too_large" => MemoryFit::TooLarge,
            _ => MemoryFit::Comfortable,
        }
    }
}

/// 本地模型记录。`path` 为唯一键（模型目录绝对路径，symlink 已解析）。
/// 注意：对外序列化为 camelCase（前端 `model.sizeBytes` 等），
/// DB 列名保持 snake_case，仅 serde 层转换。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelRecord {
    pub path: String,
    pub name: String,
    pub source: LocalModelSource,
    pub size_bytes: i64,
    pub model_type: String,
    pub quantization: String,
    pub context_length: Option<u32>,
    pub kv_bytes_per_token: Option<i64>,
    pub has_tokenizer: bool,
    pub has_chat_template: bool,
    pub memory_fit: MemoryFit,
    /// 分级验证状态：0 元数据完整 → 5 Claude Code 真实请求已验证。
    #[serde(default)]
    pub verify_level: u8,
    /// 模型目录内容指纹（metadata 变化时失效重验）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

impl LocalModelRecord {
    /// 估算模型基础工作集（权重 + 余量 + 2 GiB 内核/运行时）。
    pub fn estimated_model_working_set_bytes(&self) -> i64 {
        (self.size_bytes as f64 * 1.20) as i64 + 2 * 1_073_741_824
    }

    /// 按模型结构估算的每 token KV 成本（bfloat16 上下文）。
    pub fn kv_cache_per_10k_tokens_bytes(&self) -> Option<i64> {
        self.kv_bytes_per_token.map(|b| b * 10_000)
    }

    /// 模型声明上下文对应的最大 KV 占用。
    pub fn maximum_context_kv_bytes(&self) -> Option<i64> {
        match (self.kv_bytes_per_token, self.context_length) {
            (Some(kv), Some(ctx)) => Some(kv * ctx as i64),
            _ => None,
        }
    }

    /// 估算配置峰值（工作集 + 保留缓存上限）。
    pub fn estimated_configured_footprint_bytes(&self) -> i64 {
        self.estimated_model_working_set_bytes()
            + recommended_prompt_cache_bytes(self.size_bytes)
    }
}

/// 内存适配分级（与 mlx-gateway-studio 的 memoryFit 一致）。
pub fn classify_memory_fit(weight_bytes: i64) -> MemoryFit {
    let physical = physical_memory_bytes() as f64;
    let estimate = (weight_bytes as f64 * 1.20) + 2.0 * 1_073_741_824.0
        + recommended_prompt_cache_bytes(weight_bytes) as f64;
    if estimate < physical * 0.65 {
        MemoryFit::Comfortable
    } else if estimate < physical * 0.86 {
        MemoryFit::Tight
    } else {
        MemoryFit::TooLarge
    }
}

impl Database {
    /// 列出全部本地模型（按内存适配优先排序）。
    pub fn list_local_models(&self) -> Result<Vec<LocalModelRecord>, AppError> {
        let conn = lock_conn!(self.conn);
        let mut stmt = conn
            .prepare(
                "SELECT path, name, source, size_bytes, model_type, quantization,
                        context_length, kv_bytes_per_token, has_tokenizer, has_chat_template,
                        memory_fit, verify_level, fingerprint, last_used_at
                 FROM local_models ORDER BY memory_fit ASC, size_bytes ASC, name ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(LocalModelRecord {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    source: LocalModelSource::from_str(&row.get::<_, String>(2)?),
                    size_bytes: row.get(3)?,
                    model_type: row.get(4)?,
                    quantization: row.get(5)?,
                    context_length: row.get(6)?,
                    kv_bytes_per_token: row.get(7)?,
                    has_tokenizer: row.get(8)?,
                    has_chat_template: row.get(9)?,
                    memory_fit: MemoryFit::from_str(&row.get::<_, String>(10)?),
                    verify_level: row.get(11)?,
                    fingerprint: row.get(12)?,
                    last_used_at: row.get(13)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut models = Vec::new();
        for row in rows {
            models.push(row.map_err(|e| AppError::Database(e.to_string()))?);
        }
        Ok(models)
    }

    /// 按路径获取本地模型。
    pub fn get_local_model(&self, path: &str) -> Result<Option<LocalModelRecord>, AppError> {
        Ok(self
            .list_local_models()?
            .into_iter()
            .find(|m| m.path == path))
    }

    /// 插入或更新本地模型记录。
    pub fn upsert_local_model(&self, model: &LocalModelRecord) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "INSERT INTO local_models (path, name, source, size_bytes, model_type, quantization,
                    context_length, kv_bytes_per_token, has_tokenizer, has_chat_template,
                    memory_fit, verify_level, fingerprint, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(path) DO UPDATE SET
                    name = excluded.name, source = excluded.source,
                    size_bytes = excluded.size_bytes, model_type = excluded.model_type,
                    quantization = excluded.quantization,
                    context_length = excluded.context_length,
                    kv_bytes_per_token = excluded.kv_bytes_per_token,
                    has_tokenizer = excluded.has_tokenizer,
                    has_chat_template = excluded.has_chat_template,
                    memory_fit = excluded.memory_fit,
                    fingerprint = excluded.fingerprint,
                    last_used_at = COALESCE(excluded.last_used_at, local_models.last_used_at)",
            rusqlite::params![
                model.path,
                model.name,
                model.source.as_str(),
                model.size_bytes,
                model.model_type,
                model.quantization,
                model.context_length,
                model.kv_bytes_per_token,
                model.has_tokenizer,
                model.has_chat_template,
                model.memory_fit.as_str(),
                model.verify_level,
                model.fingerprint,
                model.last_used_at,
            ],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 批量 upsert（扫描结果合并；last_used_at 保留旧值）。
    pub fn upsert_local_models(&self, models: &[LocalModelRecord]) -> Result<(), AppError> {
        for model in models {
            self.upsert_local_model(model)?;
        }
        Ok(())
    }

    /// 移除本地模型引用（不删除磁盘文件）。
    pub fn remove_local_model(&self, path: &str) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "DELETE FROM local_models WHERE path = ?1",
            rusqlite::params![path],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 批量移除本地模型引用（按路径集合）。
    /// 清理不再被扫描到的模型记录（外部引用只删记录，不删磁盘文件）。
    pub fn prune_local_models_not_in(&self, keep_paths: &[String]) -> Result<(), AppError> {
        let keep: HashMap<&str, ()> =
            keep_paths.iter().map(|s| (s.as_str(), ())).collect();
        let existing = self.list_local_models()?;
        for model in existing {
            if !keep.contains_key(model.path.as_str()) {
                self.remove_local_model(&model.path)?;
            }
        }
        Ok(())
    }

    /// 更新模型验证等级。
    pub fn set_local_model_verify_level(
        &self,
        path: &str,
        level: u8,
    ) -> Result<(), AppError> {
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE local_models SET verify_level = ?1 WHERE path = ?2",
            rusqlite::params![level, path],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }

    /// 记录使用时间。
    pub fn touch_local_model(&self, path: &str) -> Result<(), AppError> {
        let now = chrono::Utc::now().timestamp();
        let conn = lock_conn!(self.conn);
        conn.execute(
            "UPDATE local_models SET last_used_at = ?1 WHERE path = ?2",
            rusqlite::params![now, path],
        )
        .map_err(|e| AppError::Database(e.to_string()))?;
        Ok(())
    }
}
