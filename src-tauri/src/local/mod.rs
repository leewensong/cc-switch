//! 本地 MLX 模型服务（Local LLM）
//!
//! CC Switch v4.0 新特性：在应用内直接启动一个本地 MLX 模型
//! （`mlx_lm.server` 8090 内部 + `anthropic-proxy-rs` 8091 统一网关），
//! 并在各 agent 应用的供应商路由里缺省提供 "Local MLX" 卡片。
//!
//! 行为语义移植自 `mlx-gateway-studio`（RuntimeCoordinator / ModelDiscovery），
//! 实现见项目 spec：`docs/local-model-kiro-spec-zh.md`。

pub mod coordinator;
pub mod discovery;
pub mod model;
pub mod providers;
pub mod settings;

/// Local 卡片的固定 provider id（各 app 共用）。
pub const LOCAL_PROVIDER_ID: &str = "local-mlx";
/// Local 卡片显示名。
pub const LOCAL_PROVIDER_NAME: &str = "Local MLX";
/// 对外稳定模型别名：切换真实模型后客户端模型名不变。
pub const GATEWAY_MODEL_ALIAS: &str = "default_model";
/// 本地网关不需要真实鉴权的占位 token（明确标注为占位，不是安全认证）。
pub const LOCAL_AUTH_TOKEN: &str = "local-not-required";

/// 状态变更事件名（全量状态推送）。
pub const LOCAL_STATUS_EVENT: &str = "local-model://status";
/// 后台"启动完成后自动切换"完成事件。
pub const LOCAL_AUTO_SWITCH_EVENT: &str = "local-model://auto-switched";

pub use coordinator::{LocalLogEntry, LocalManager, LocalServiceStatus, RuntimeState};
pub use discovery::{discover_models, scan_root};
pub use model::{LocalModelRecord, LocalModelSource, MemoryFit};
pub use settings::{
    detect_proxy_path, detect_python_path, load_local_settings, save_local_settings, LocalSettings,
    LOCAL_SETTINGS_FILE,
};
