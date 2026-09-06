//! Local MLX 供应商卡片（合成 + 种子管理）
//!
//! ADR-001：Local 卡片以数据库**种子行**形态存在（id 固定 `local-mlx`，
//! `meta.provider_type = "local_mlx"`），因此：
//! - 所有读取路径（current 校验、托盘菜单、故障转移、导入导出）天然可见；
//! - 删除被后端拦截（用户视角不可删除）；
//! - 云同步/导入导出按 provider_type 豁免（M1 落地，M0 先拦截删除）。
//!
//! 配置模板：M0 实现 Claude Code + Claude Desktop（本轮用户接入 Desktop）；
//! 其余 app M1 补齐并实测。

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::local::settings::LocalSettings;
use crate::local::{GATEWAY_MODEL_ALIAS, LOCAL_AUTH_TOKEN, LOCAL_PROVIDER_ID, LOCAL_PROVIDER_NAME};
use crate::provider::{
    ClaudeDesktopMode, ClaudeDesktopModelRoute, Provider, ProviderMeta,
};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Claude Desktop 模型菜单中展示的 Claude-safe 路由 id（与前端预设一致）。
/// 网关侧由 `COMPLETION_MODEL/REASONING_MODEL=default_model` 统一映射，
/// 因此路由 id 只需满足 Desktop 的名白名单。
const DESKTOP_ROUTE_IDS: [&str; 3] = ["claude-sonnet-5", "claude-opus-5", "claude-haiku-4-5"];

/// 判断一个供应商是否为 Local MLX 卡片。
pub fn is_local_provider(provider: &Provider) -> bool {
    provider.id == LOCAL_PROVIDER_ID
        || provider
            .meta
            .as_ref()
            .and_then(|m| m.provider_type.as_deref())
            == Some("local_mlx")
}

/// 按 app 构建 Local 供应商（settings_config 使用当前运行时设置）。
///
/// M0 实现 Claude Code + Claude Desktop；其他 app 返回 Err（M1 补齐）。
pub fn build_local_provider(
    app_type: &AppType,
    settings: &LocalSettings,
) -> Result<Provider, AppError> {
    let (settings_config, desktop_mode, desktop_routes): (
        Value,
        Option<ClaudeDesktopMode>,
        Option<HashMap<String, ClaudeDesktopModelRoute>>,
    ) = match app_type {
        AppType::Claude => (
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": settings.base_url(),
                    "ANTHROPIC_AUTH_TOKEN": LOCAL_AUTH_TOKEN,
                    "ANTHROPIC_MODEL": GATEWAY_MODEL_ALIAS,
                    "ANTHROPIC_DEFAULT_HAIKU_MODEL": GATEWAY_MODEL_ALIAS,
                    "ANTHROPIC_DEFAULT_SONNET_MODEL": GATEWAY_MODEL_ALIAS,
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": GATEWAY_MODEL_ALIAS,
                    // 钉住上下文窗口：mlx_lm.server 对超大请求体（~400KB+，Claude Code
                    // 首请求即超）会卡死整个服务；64-256KB 实测安全。压缩窗口越小，
                    // 首请求 prefill 越快（8k tokens ≈ 55s）。
                    "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "32768",
                    "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "32768",
                }
            }),
            None,
            None,
        ),
        AppType::ClaudeDesktop => (
            // Direct 模式：Desktop 直连统一网关（8091），请求经网关映射 default_model。
            // api_key 用明确标注的占位 token（本地服务无鉴权）。
            json!({
                "env": {
                    "ANTHROPIC_BASE_URL": settings.base_url(),
                    "ANTHROPIC_AUTH_TOKEN": LOCAL_AUTH_TOKEN,
                }
            }),
            Some(ClaudeDesktopMode::Direct),
            Some(
                DESKTOP_ROUTE_IDS
                    .iter()
                    .map(|route_id| {
                        (
                            route_id.to_string(),
                            ClaudeDesktopModelRoute {
                                // Desktop 直连模式要求 route.model == route_id；
                                // 真正的模型替换发生在网关（COMPLETION_MODEL=default_model）。
                                model: route_id.to_string(),
                                label_override: Some("Local MLX".to_string()),
                                supports_1m: None,
                            },
                        )
                    })
                    .collect(),
            ),
        ),
        other => {
            return Err(AppError::localized(
                "local_provider.not_supported_yet",
                format!("{} 的 Local 卡片将在下一里程碑提供", other.as_str()),
                format!("Local provider for {} lands in the next milestone", other.as_str()),
            ));
        }
    };

    let mut meta = ProviderMeta::default();
    meta.provider_type = Some("local_mlx".to_string());
    // 本地供应商不参与故障转移队列
    meta.common_config_enabled = Some(false);
    meta.claude_desktop_mode = desktop_mode;
    meta.claude_desktop_model_routes = desktop_routes.unwrap_or_default();

    Ok(Provider {
        id: LOCAL_PROVIDER_ID.to_string(),
        name: LOCAL_PROVIDER_NAME.to_string(),
        settings_config,
        website_url: None,
        category: Some("local".to_string()),
        created_at: Some(chrono::Utc::now().timestamp()),
        sort_index: Some(0),
        notes: None,
        meta: Some(meta),
        icon: Some("cpu".to_string()),
        icon_color: Some("#34D399".to_string()),
        in_failover_queue: false,
    })
}

/// 确保某 app 存在 Local 卡片种子行，并用当前设置刷新其 settings_config。
/// 幂等；仅 macOS 生效（其他平台不产出卡片）。
pub fn ensure_local_provider_row(
    db: &Database,
    app_type: &AppType,
    settings: &LocalSettings,
) -> Result<(), AppError> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let app_supported = matches!(app_type, AppType::Claude | AppType::ClaudeDesktop);
    if !app_supported {
        return Ok(());
    }
    let provider = build_local_provider(app_type, settings)?;
    db.save_provider(app_type.as_str(), &provider)?;
    Ok(())
}

/// 刷新所有已支持 app 的 Local 卡片（设置变更时调用）。
pub fn refresh_local_provider_rows(
    db: &Database,
    settings: &LocalSettings,
) -> Result<(), AppError> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    for app_type in [AppType::Claude, AppType::ClaudeDesktop] {
        ensure_local_provider_row(db, &app_type, settings)?;
    }
    Ok(())
}

/// 是否允许删除该供应商（Local 卡片不可删除）。
pub fn is_deletable_provider(provider: &Provider) -> bool {
    !is_local_provider(provider)
}

// 兼容旧序列化字段名
impl ProviderMeta {
    pub fn is_local_mlx(&self) -> bool {
        self.provider_type.as_deref() == Some("local_mlx")
    }
}
