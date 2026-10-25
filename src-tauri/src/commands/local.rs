//! 本地模型 Tauri 命令
//!
//! 重要：所有涉及磁盘扫描/数据库的命令必须为 async（在后台线程执行）。
//! Tauri 的同步命令在主线程执行，全量扫描会把 UI 冻结成"卡死"。

use crate::app_config::AppType;
use crate::error::AppError;
use crate::local::coordinator::{LocalManager, LocalServiceStatus, RuntimeState};
use crate::local::discovery::{discover_models, parse_model_dir, scan_root};
use crate::local::model::{LocalModelRecord, LocalModelSource};
use crate::local::providers;
use crate::local::settings::{load_local_settings, save_local_settings, LocalSettings};
use crate::local::LOCAL_SETTINGS_FILE;
use crate::services::{ProviderService, SwitchResult};
use crate::store::AppState;
use std::path::Path;
use tauri::{Manager, State};

/// 列出本地模型（来自数据库；未扫描过的目录可先 rescan）。
#[tauri::command]
pub async fn local_models_list(
    state: State<'_, AppState>,
) -> Result<Vec<LocalModelRecord>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let rows = db.list_local_models().map_err(|e| e.to_string())?;
        log::info!("[local] list_local_models -> {} rows", rows.len());
        Ok(rows)
    })
    .await
    .map_err(|e| format!("任务执行失败: {e}"))?
}

/// 扫描全部根目录并合并进数据库（保留上次使用时间与验证等级）。
/// 扫描结果 = 权威集合：合并新增/更新，并移除不再存在的记录（只删引用）。
#[tauri::command]
pub async fn local_models_rescan(
    state: State<'_, AppState>,
) -> Result<Vec<LocalModelRecord>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let settings = load_local_settings();
        let discovered = discover_models(&settings);
        log::info!(
            "[local] rescan: discovered {} model(s); roots={}",
            discovered.len(),
            settings.custom_model_roots.len()
        );
        let keep_paths: Vec<String> = discovered.iter().map(|m| m.path.clone()).collect();
        db.prune_local_models_not_in(&keep_paths)
            .map_err(|e| e.to_string())?;
        db.upsert_local_models(&discovered)
            .map_err(|e| e.to_string())?;
        db.list_local_models().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务执行失败: {e}"))?
}

/// 添加自定义模型根目录（立即扫描合并）。
#[tauri::command]
pub async fn local_models_add_root(
    state: State<'_, AppState>,
    path: String,
) -> Result<usize, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let dir = Path::new(&path);
        if !dir.is_dir() {
            return Err(format!("目录不存在：{path}"));
        }
        let canonical = dir
            .canonicalize()
            .map_err(|e| format!("无法解析目录：{e}"))?;
        let canonical_str = canonical.to_string_lossy().to_string();

        let mut settings = load_local_settings();
        if !settings
            .custom_model_roots
            .iter()
            .any(|r| r == &canonical_str)
        {
            settings.custom_model_roots.push(canonical_str.clone());
            save_local_settings(&settings).map_err(|e| e.to_string())?;
        }

        let records = scan_root(&canonical, LocalModelSource::Custom);
        db.upsert_local_models(&records)
            .map_err(|e| e.to_string())?;
        Ok(records.len())
    })
    .await
    .map_err(|e| format!("任务执行失败: {e}"))?
}

/// 移除自定义模型根目录。
#[tauri::command]
pub async fn local_models_remove_root(path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut settings = load_local_settings();
        settings.custom_model_roots.retain(|root| root != &path);
        save_local_settings(&settings).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务执行失败: {e}"))?
}

/// 移除本地模型引用（不删除磁盘文件）。
#[tauri::command]
pub async fn local_models_remove_ref(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        db.remove_local_model(&path).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务执行失败: {e}"))?
}

/// 启动本地服务（异步编排；就绪经状态轮询/事件观察）。
#[tauri::command]
pub async fn local_service_start(
    app_handle: tauri::AppHandle,
    model_path: String,
) -> Result<LocalServiceStatus, String> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    log::info!("[local] start requested: model_path={model_path}");
    let model = resolve_model(&state.db, &model_path)?;
    let mut settings = load_local_settings();
    settings.selected_model_path = Some(model.path.clone());
    save_local_settings(&settings).map_err(|e| e.to_string())?;

    // 被选中即元数据完整 → 验证等级至少 1；后台合并扫描结果
    let _ = state.db.set_local_model_verify_level(&model.path, 1);

    state
        .local
        .start(model, settings)
        .await
        .map_err(|e| e.to_string())?;
    Ok(state.local.status().await)
}

/// 停止本地服务（串行）。
#[tauri::command]
pub async fn local_service_stop(
    app_handle: tauri::AppHandle,
) -> Result<LocalServiceStatus, String> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    state.local.stop().await;
    Ok(state.local.status().await)
}

/// 当前服务状态。
#[tauri::command]
pub async fn local_service_status(
    app_handle: tauri::AppHandle,
) -> Result<LocalServiceStatus, String> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    Ok(state.local.status().await)
}

/// 增量日志（sinceId=0 全量，最多环形缓冲容量）。环形缓冲访问极轻，async 即可。
#[tauri::command]
pub async fn local_service_logs(
    app_handle: tauri::AppHandle,
    #[allow(non_snake_case)] sinceId: u64,
) -> Result<Vec<crate::local::LocalLogEntry>, String> {
    let manager: LocalManager = {
        let state = app_handle
            .try_state::<AppState>()
            .ok_or_else(|| "应用状态不可用".to_string())?;
        state.local.clone()
    };
    Ok(manager.logs_since(sinceId))
}

/// 清空日志。
#[tauri::command]
pub async fn local_service_clear_logs(app_handle: tauri::AppHandle) -> Result<(), String> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    state.local.clear_logs();
    Ok(())
}

/// 最小真实推理测试（走统一网关，需服务 ready）。
#[tauri::command]
pub async fn local_service_test(app_handle: tauri::AppHandle) -> Result<String, String> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    let status = state.local.status().await;
    if status.state != RuntimeState::Ready {
        return Err(format!("本地服务未就绪（当前状态：{}）", status.state));
    }
    let settings = load_local_settings();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(120))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .post(format!("{}/v1/chat/completions", settings.base_url()))
        .json(&serde_json::json!({
            "model": crate::local::GATEWAY_MODEL_ALIAS,
            "messages": [{ "role": "user", "content": "Reply with OK." }],
            "max_tokens": 4,
            "temperature": 0,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|e| format!("测试请求失败：{e}"))?;
    let http_status = response.status().as_u16();
    if (200..300).contains(&http_status) {
        Ok(format!("连接成功（HTTP {http_status}）"))
    } else {
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "测试请求失败（HTTP {http_status}）：{}",
            crate::local::coordinator::truncate_pub(&body, 500)
        ))
    }
}

/// 读取本地服务设置。
#[tauri::command]
pub fn local_settings_get() -> Result<LocalSettings, String> {
    Ok(load_local_settings())
}

/// 保存本地服务设置（刷新状态形状 + Local 卡片配置）。
#[tauri::command]
pub async fn local_settings_set(
    state: State<'_, AppState>,
    settings: LocalSettings,
) -> Result<(), String> {
    if settings.mlx_port == settings.gateway_port
        || !(1..=65_535).contains(&settings.mlx_port)
        || !(1..=65_535).contains(&settings.gateway_port)
    {
        return Err("端口配置无效：MLX 与网关端口必须不同且在 1-65535 之间".to_string());
    }
    let manager_for_shape = state.local.clone();
    let db = state.db.clone();
    let settings_for_rows = settings.clone();
    tauri::async_runtime::spawn_blocking(move || {
        save_local_settings(&settings_for_rows).map_err(|e| e.to_string())?;
        if let Err(e) = providers::refresh_local_provider_rows(&db, &settings_for_rows) {
            log::warn!("Failed to refresh Local provider rows: {e}");
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(|e| format!("任务执行失败: {e}"))??;
    manager_for_shape.refresh_status_shape(&settings);
    Ok(())
}

/// 本地服务设置文件路径（前端"打开配置文件夹"用）。
#[tauri::command]
pub fn get_local_settings_path() -> Result<String, String> {
    Ok(crate::config::get_app_config_dir()
        .join(LOCAL_SETTINGS_FILE)
        .to_string_lossy()
        .to_string())
}

// ===== 内部实现 =====

/// 解析模型记录：优先数据库，否则直接解析目录（支持"拖入目录即启动"）。
pub(crate) fn resolve_model(
    db: &crate::database::Database,
    model_path: &str,
) -> Result<LocalModelRecord, String> {
    if let Some(model) = db.get_local_model(model_path).map_err(|e| e.to_string())? {
        return Ok(model);
    }
    let dir = Path::new(model_path);
    if !dir.join("config.json").is_file() {
        return Err(format!("模型目录不可用：{model_path}"));
    }
    parse_model_dir(dir, LocalModelSource::Custom)
        .map(|mut record| {
            let _ = db.upsert_local_model(&record);
            record.verify_level = 1;
            record
        })
        .ok_or_else(|| format!("无法解析模型目录：{model_path}"))
}

/// 切换到 Local 卡片（agent 应用路由）。
///
/// 服务已就绪 → 立即写 live；未就绪 → 用上次模型后台启动，就绪后自动完成切换，
/// 并推送 `local-model://auto-switched` 事件；返回可读警告（前端 toast）。
pub async fn switch_to_local_provider(
    app_handle: tauri::AppHandle,
    app_type: AppType,
) -> Result<SwitchResult, AppError> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| AppError::Message("应用状态不可用".to_string()))?;
    let status = state.local.status().await;

    match status.state {
        RuntimeState::Ready | RuntimeState::Degraded => {
            let state_for_blocking = state.inner().clone();
            tauri::async_runtime::spawn_blocking(move || {
                ProviderService::switch_local(&state_for_blocking, app_type)
            })
            .await
            .map_err(|e| AppError::Message(format!("供应商切换任务执行失败: {e}")))?
        }
        RuntimeState::Stopping => Err(AppError::Message("本地服务正在停止，请稍候".to_string())),
        state if state.is_starting() => {
            schedule_auto_switch(app_handle, app_type);
            Ok(switch_result_warning("local.starting_auto_switch"))
        }
        _ => {
            // 停止 / 失败：用上次模型后台启动
            let settings = load_local_settings();
            let Some(model_path) = settings.selected_model_path.clone() else {
                return Err(AppError::localized(
                    "local.no_model_selected",
                    "本地模型尚未选择。请先在「本地模型」页选择模型并启动 Local API。",
                    "No local model selected yet. Pick a model in the Local Model page first.",
                ));
            };
            let model = resolve_model(&state.db, &model_path).map_err(AppError::Message)?;
            state
                .local
                .start(model, settings)
                .await
                .map_err(AppError::Message)?;
            schedule_auto_switch(app_handle, app_type);
            Ok(switch_result_warning("local.starting_auto_switch"))
        }
    }
}

fn switch_result_warning(code: &str) -> SwitchResult {
    SwitchResult {
        warnings: vec![code.to_string()],
        route: None,
    }
}

/// 后台等待就绪（上限 30 分钟）→ 切换 → 推送事件。
fn schedule_auto_switch(app_handle: tauri::AppHandle, app_type: AppType) {
    let mut state_for_blocking: Option<AppState> = None;
    let manager: Option<LocalManager> = match app_handle.try_state::<AppState>() {
        Some(state) => {
            state_for_blocking = Some(state.inner().clone());
            Some(state.local.clone())
        }
        None => None,
    };
    let (Some(manager), Some(state_for_blocking)) = (manager, state_for_blocking) else {
        return;
    };
    let app_type_str = app_type.as_str().to_string();

    tauri::async_runtime::spawn(async move {
        let mut attempts = 0u32;
        loop {
            if attempts > 1800 {
                log::warn!("Auto-switch to local provider timed out waiting for service");
                return;
            }
            let status = manager.status().await;
            match status.state {
                RuntimeState::Ready | RuntimeState::Degraded => break,
                RuntimeState::Failed | RuntimeState::Stopped => {
                    log::warn!("Local service stopped/failed before auto-switch could complete");
                    return;
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    attempts += 1;
                }
            }
        }

        let result = tauri::async_runtime::spawn_blocking(move || {
            ProviderService::switch_local(&state_for_blocking, app_type)
        })
        .await;
        match result {
            Ok(Ok(_)) => manager.emit_auto_switch(&app_type_str),
            Ok(Err(e)) => log::warn!("Auto-switch to local provider failed: {e}"),
            Err(e) => log::warn!("Auto-switch task panicked: {e}"),
        }
    });
}

// ===== 诊断面板 =====

/// 网关活跃请求快照（解析自 /studio/metrics）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveRequestInfo {
    pub id: u64,
    pub protocol: String,
    pub streaming: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_bytes: Option<u64>,
}

/// 网关指标快照。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayMetrics {
    pub uptime_seconds: f64,
    pub active_requests: u64,
    pub total_requests: u64,
    pub completed_requests: u64,
    pub failed_requests: u64,
    pub request_bytes_received: u64,
    pub response_bytes_sent: u64,
    pub active: Vec<ActiveRequestInfo>,
}

/// 诊断聚合：服务状态 + 网关指标 + 最近日志（调试面板轮询用）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalDiagnostics {
    pub status: LocalServiceStatus,
    pub metrics: Option<GatewayMetrics>,
    pub recent_logs: Vec<crate::local::LocalLogEntry>,
    /// 网关健康（/health 是否 200）
    pub gateway_healthy: bool,
    /// MLX 健康（/health 是否 200）
    pub mlx_healthy: bool,
}

#[tauri::command]
pub async fn local_service_diagnostics(
    app_handle: tauri::AppHandle,
) -> Result<LocalDiagnostics, String> {
    let state = app_handle
        .try_state::<AppState>()
        .ok_or_else(|| "应用状态不可用".to_string())?;
    let status = state.local.status().await;
    let settings = load_local_settings();

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(1))
        .timeout(std::time::Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default();

    let gateway_healthy = client
        .get(format!(
            "http://{}:{}/health",
            settings.host, settings.gateway_port
        ))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let mlx_healthy = client
        .get(format!(
            "http://{}:{}/health",
            settings.host, settings.mlx_port
        ))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);

    let metrics = if gateway_healthy {
        let metrics_value = match client
            .get(format!(
                "http://{}:{}/studio/metrics",
                settings.host, settings.gateway_port
            ))
            .send()
            .await
        {
            Ok(resp) => resp.json::<serde_json::Value>().await.ok(),
            Err(_) => None,
        };
        metrics_value.map(|v| GatewayMetrics {
            uptime_seconds: v
                .get("uptime_seconds")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0),
            active_requests: v
                .get("active_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            total_requests: v
                .get("total_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            completed_requests: v
                .get("completed_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            failed_requests: v
                .get("failed_requests")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            request_bytes_received: v
                .get("request_bytes_received")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            response_bytes_sent: v
                .get("response_bytes_sent")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            active: v
                .get("active")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| {
                            Some(ActiveRequestInfo {
                                id: item.get("id")?.as_u64()?,
                                protocol: item
                                    .get("protocol")
                                    .and_then(|p| p.as_str())
                                    .unwrap_or("?")
                                    .to_string(),
                                streaming: item
                                    .get("streaming")
                                    .and_then(|s| s.as_bool())
                                    .unwrap_or(false),
                                elapsed_seconds: item
                                    .get("elapsed_seconds")
                                    .and_then(|e| e.as_f64()),
                                request_bytes: item.get("request_bytes").and_then(|b| b.as_u64()),
                                response_bytes: item.get("response_bytes").and_then(|b| b.as_u64()),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
    } else {
        None
    };

    let recent_logs = state.local.logs_since(0);
    let tail = recent_logs
        .iter()
        .rev()
        .take(40)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    Ok(LocalDiagnostics {
        status,
        metrics,
        recent_logs: tail,
        gateway_healthy,
        mlx_healthy,
    })
}
