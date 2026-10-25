//! 本地服务设置（`~/.cc-switch/local-settings.json`）
//!
//! 原子写 + schemaVersion，与项目其他 JSON 配置文件保持一致。
//! 敏感信息不入库；`local-settings.json` 不参与云同步（模型路径是本机路径）。

use crate::config::{get_app_config_dir, read_json_file, write_json_file};
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const LOCAL_SETTINGS_FILE: &str = "local-settings.json";

/// macOS TCC 保护目录：未授权 app 派生进程对这些目录的访问会
/// **阻塞在 open() 等待系统授权弹窗**（headless/无交互时永久挂起）。
/// 详见 `docs/local-model-kiro-spec-zh.md` §11.1。
pub const TCC_PROTECTED_DIRS: [&str; 3] = ["Documents", "Desktop", "Downloads"];

/// 路径是否位于 macOS TCC 保护目录下（home/Documents|Desktop|Downloads）。
pub fn is_tcc_protected_path(path: &Path) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    TCC_PROTECTED_DIRS
        .iter()
        .any(|dir| path.starts_with(home.join(dir)))
}

const DEFAULT_SCHEMA_VERSION: u32 = 1;
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_MLX_PORT: u16 = 8092;
const DEFAULT_GATEWAY_PORT: u16 = 8091;
const DEFAULT_MAX_TOKENS: u32 = 4096;
const DEFAULT_PREFILL_STEP_SIZE: u32 = 2048;
const DEFAULT_PROMPT_CONCURRENCY: u32 = 1;
const DEFAULT_DECODE_CONCURRENCY: u32 = 1;
const DEFAULT_UPSTREAM_TIMEOUT_SECS: u64 = 172_800; // 48 小时，长任务不被切断

fn default_schema_version() -> u32 {
    DEFAULT_SCHEMA_VERSION
}
fn default_host() -> String {
    DEFAULT_HOST.to_string()
}
fn default_mlx_port() -> u16 {
    DEFAULT_MLX_PORT
}
fn default_gateway_port() -> u16 {
    DEFAULT_GATEWAY_PORT
}
fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}
fn default_prefill_step_size() -> u32 {
    DEFAULT_PREFILL_STEP_SIZE
}
fn default_prompt_cache_size() -> u32 {
    recommended_prompt_cache_size()
}
fn default_prompt_concurrency() -> u32 {
    DEFAULT_PROMPT_CONCURRENCY
}
fn default_decode_concurrency() -> u32 {
    DEFAULT_DECODE_CONCURRENCY
}
fn default_upstream_timeout_secs() -> u64 {
    DEFAULT_UPSTREAM_TIMEOUT_SECS
}

/// 单模型运行覆盖。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelOverride {
    /// Agent 工作上下文（None = 使用模型声明值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<u32>,
    /// 输出 fallback（request 未提供时生效）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// 本地服务设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSettings {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// 受管 Python 解释器路径（空 = 自动探测）。
    #[serde(default)]
    pub python_path: String,
    /// anthropic-proxy-rs 二进制路径（空 = 自动探测 bundled / ~/.local/bin）。
    #[serde(default)]
    pub proxy_path: String,
    /// 上次使用的模型目录（登录 "启动上次服务" 用）。
    #[serde(default)]
    pub selected_model_path: Option<String>,
    /// 自定义模型扫描根目录。
    #[serde(default)]
    pub custom_model_roots: Vec<String>,
    #[serde(default = "default_host")]
    pub host: String,
    /// 内部 MLX 端口（仅网关访问）。
    #[serde(default = "default_mlx_port")]
    pub mlx_port: u16,
    /// 统一网关端口（agent 可见）。
    #[serde(default = "default_gateway_port")]
    pub gateway_port: u16,
    /// 输出 fallback。
    #[serde(default = "default_max_tokens")]
    pub max_tokens_fallback: u32,
    #[serde(default = "default_prefill_step_size")]
    pub prefill_step_size: u32,
    /// true = 按物理内存自动计算 prompt cache 条目数。
    #[serde(default = "default_true")]
    pub automatic_prompt_cache: bool,
    /// 手动 prompt cache 条目数（automatic_prompt_cache=false 时生效）。
    #[serde(default = "default_prompt_cache_size")]
    pub prompt_cache_size: u32,
    #[serde(default = "default_prompt_concurrency")]
    pub prompt_concurrency: u32,
    #[serde(default = "default_decode_concurrency")]
    pub decode_concurrency: u32,
    #[serde(default = "default_upstream_timeout_secs")]
    pub upstream_timeout_secs: u64,
    /// 推理后端："mlx"（受管 mlx_lm.server）| "ollama"（复用 ollama serve）。
    /// ollama 模式与 ollama 共用模型文件，零重复下载。
    #[serde(default = "default_backend", alias = "backend")]
    pub backend: String,
    /// ollama Base URL（backend=ollama 时生效）。
    #[serde(default = "default_ollama_base_url", alias = "ollamaBaseUrl")]
    pub ollama_base_url: String,
    /// ollama 模型 tag（backend=ollama 时生效）。
    #[serde(default = "default_ollama_model", alias = "ollamaModel")]
    pub ollama_model: String,
    /// 按模型路径覆盖运行参数。
    #[serde(default)]
    pub model_overrides: HashMap<String, ModelOverride>,
    /// 应用启动时是否自动拉起上次服务（默认关）。
    #[serde(default)]
    pub auto_start_with_app: bool,
    /// Local 卡片按 app 隐藏（device 级偏好，不入库）。
    #[serde(default)]
    pub hidden_cards: HashMap<String, bool>,
}

fn default_true() -> bool {
    true
}
fn default_backend() -> String {
    "mlx".to_string()
}
fn default_ollama_base_url() -> String {
    "http://127.0.0.1:11434".to_string()
}
fn default_ollama_model() -> String {
    "qwen3.8:27b-mlx".to_string()
}

impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            schema_version: DEFAULT_SCHEMA_VERSION,
            python_path: detect_python_path(),
            proxy_path: detect_proxy_path(),
            selected_model_path: None,
            custom_model_roots: Vec::new(),
            host: DEFAULT_HOST.to_string(),
            mlx_port: DEFAULT_MLX_PORT,
            gateway_port: DEFAULT_GATEWAY_PORT,
            max_tokens_fallback: DEFAULT_MAX_TOKENS,
            prefill_step_size: DEFAULT_PREFILL_STEP_SIZE,
            automatic_prompt_cache: true,
            prompt_cache_size: recommended_prompt_cache_size(),
            prompt_concurrency: DEFAULT_PROMPT_CONCURRENCY,
            decode_concurrency: DEFAULT_DECODE_CONCURRENCY,
            upstream_timeout_secs: DEFAULT_UPSTREAM_TIMEOUT_SECS,
            backend: "mlx".to_string(),
            ollama_base_url: "http://127.0.0.1:11434".to_string(),
            ollama_model: "qwen3.8:27b-mlx".to_string(),
            model_overrides: HashMap::new(),
            auto_start_with_app: false,
            hidden_cards: HashMap::new(),
        }
    }
}

impl LocalSettings {
    /// 解析后的 prompt cache 条目数。
    pub fn resolved_prompt_cache_size(&self) -> u32 {
        if self.automatic_prompt_cache {
            recommended_prompt_cache_size()
        } else {
            self.prompt_cache_size
        }
    }

    /// 按模型路径解析运行覆盖。
    pub fn effective_overrides(&self, model_path: &str) -> ModelOverride {
        self.model_overrides
            .get(model_path)
            .cloned()
            .unwrap_or_default()
    }

    /// 网关 Base URL。
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.gateway_port)
    }

    /// 内部 MLX Base URL。
    pub fn mlx_base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.mlx_port)
    }
}

/// 按物理内存推荐 prompt cache 条目数（与 mlx-gateway-studio 一致）。
pub fn recommended_prompt_cache_size() -> u32 {
    let gib = physical_memory_gib();
    match gib {
        x if x >= 96.0 => 12,
        x if x >= 48.0 => 8,
        x if x >= 24.0 => 6,
        _ => 4,
    }
}

/// 按模型工作集估算 `--prompt-cache-bytes` 安全上限（半 GiB 对齐）。
pub fn recommended_prompt_cache_bytes(model_size_bytes: i64) -> i64 {
    let gib: i64 = 1_073_741_824;
    let physical = physical_memory_bytes();
    let estimated_model_working_set = (model_size_bytes as f64 * 1.20) as i64 + 2 * gib;
    let available_headroom = (physical - estimated_model_working_set).max(0);
    let half_gib: i64 = 512 * 1_024 * 1_024;
    let candidate = (8 * gib).min(gib.max(available_headroom / 8));
    (candidate / half_gib * half_gib).max(half_gib)
}

pub fn physical_memory_bytes() -> i64 {
    // macOS / Linux: sysconf；失败时保守返回 24 GiB。
    #[cfg(unix)]
    {
        unsafe {
            let bytes = libc::sysconf(libc::_SC_PHYS_PAGES) * libc::sysconf(libc::_SC_PAGESIZE);
            if bytes > 0 {
                return bytes;
            }
        }
    }
    // Windows: GlobalMemoryStatusEx 不在本模块依赖中，走保守值。
    24 * 1_073_741_824
}

fn physical_memory_gib() -> f64 {
    physical_memory_bytes() as f64 / 1_073_741_824.0
}

/// 自动探测受管 Python 解释器。
///
/// 顺序：env `MLX_GATEWAY_PYTHON` → 开发态父目录 `.venv/bin/python`
/// （`cc-switch` 旁的 `mlx_lm/.venv`）→ `~/.local/share/cc-switch/runtime/**`
/// （未来受管 runtime）→ `~/miniconda3` / `~/anaconda3` / `~/miniconda`。
/// 只返回真实存在且可执行的路径；找不到返回空串（UI 提示配置）。
pub fn detect_python_path() -> String {
    if let Ok(override_path) = std::env::var("MLX_GATEWAY_PYTHON") {
        if !override_path.is_empty() && Path::new(&override_path).is_file() {
            return override_path;
        }
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut candidates: Vec<PathBuf> = Vec::new();

    // 首选：TCC 安全位置（~ 下非 Documents/Desktop/Downloads）。
    // 注意：~/.mlx-lm-venv 与 ~/.local/share/cc-switch/runtime 不在 macOS TCC
    // 保护目录内，app 派生进程读取无需授权；Documents 下的 venv 会在
    // spawn 时挂起（python 启动读 pyvenv.cfg → open() 阻塞等待授权）。
    candidates.push(home.join(".mlx-lm-venv/bin/python"));

    // 开发态：cwd 的父目录下的 .venv（如 ~/Documents/PythonPrj/2026/mlx_lm/.venv）
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(parent) = cwd.parent() {
            candidates.push(parent.join(".venv/bin/python"));
        }
        if let Some(parent) = cwd.parent().and_then(|p| p.parent()) {
            candidates.push(parent.join(".venv/bin/python"));
        }
    }

    // 未来受管 runtime 位置
    candidates.push(home.join(".local/share/cc-switch/runtime/current/bin/python"));

    // 常见 conda
    for base in ["miniconda3", "anaconda3", "miniconda"] {
        candidates.push(home.join(base).join("bin/python"));
    }

    candidates
        .into_iter()
        .find(|p| p.is_file() && is_executable(p))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 自动探测 anthropic-proxy-rs 二进制。
///
/// 顺序：env `MLX_GATEWAY_PROXY` → 随包资源 `resources/anthropic-proxy`
/// （由 build script 放置）→ `~/.local/bin/anthropic-proxy`。
pub fn detect_proxy_path() -> String {
    if let Ok(override_path) = std::env::var("MLX_GATEWAY_PROXY") {
        if !override_path.is_empty() && Path::new(&override_path).is_file() {
            return override_path;
        }
    }

    #[cfg(target_os = "macos")]
    {
        let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/anthropic-proxy");
        if bundled.is_file() && is_executable(&bundled) {
            return bundled.to_string_lossy().to_string();
        }
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let local = home.join(".local/bin/anthropic-proxy");
    if local.is_file() && is_executable(&local) {
        return local.to_string_lossy().to_string();
    }
    String::new()
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub fn local_settings_path() -> PathBuf {
    get_app_config_dir().join(LOCAL_SETTINGS_FILE)
}

/// 读取设置；文件不存在或损坏时返回默认值（不覆盖用户文件）。
pub fn load_local_settings() -> LocalSettings {
    read_json_file::<LocalSettings>(&local_settings_path()).unwrap_or_default()
}

/// 保存设置（原子写 + 目录创建）。
pub fn save_local_settings(settings: &LocalSettings) -> Result<(), AppError> {
    write_json_file(&local_settings_path(), settings)
}
