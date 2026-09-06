//! 本地服务进程监督（双进程状态机）
//!
//! 移植自 `mlx-gateway-studio` 的 `RuntimeCoordinator` 语义：
//! 单一串行操作、health 就绪、预热、停机顺序（TERM → 限时等待 → KILL）、
//! 状态事件推送。M0 简化：不做 drain/ownership token 持久化（M2 收口），
//! 不做 /studio/metrics 采样（M2）。
//!
//! 关键约定：子进程一旦 spawn 立即写入 slot（mlx_child / gateway_child），
//! 所有退出路径统一走 `shutdown()`（TERM→等待→KILL），不存在孤儿进程。

use crate::local::model::LocalModelRecord;
use crate::local::settings::{load_local_settings, recommended_prompt_cache_bytes, LocalSettings};
use crate::local::{LOCAL_AUTO_SWITCH_EVENT, LOCAL_STATUS_EVENT};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use tauri::Emitter;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// 运行时状态（对外 camelCase）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeState {
    Stopped,
    Validating,
    StartingMlx,
    WarmingUp,
    StartingGateway,
    Ready,
    Degraded,
    Stopping,
    Failed,
}

impl std::fmt::Display for RuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            RuntimeState::Stopped => "stopped",
            RuntimeState::Validating => "validating",
            RuntimeState::StartingMlx => "startingMlx",
            RuntimeState::WarmingUp => "warmingUp",
            RuntimeState::StartingGateway => "startingGateway",
            RuntimeState::Ready => "ready",
            RuntimeState::Degraded => "degraded",
            RuntimeState::Stopping => "stopping",
            RuntimeState::Failed => "failed",
        };
        f.write_str(name)
    }
}

impl RuntimeState {
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            RuntimeState::Validating
                | RuntimeState::StartingMlx
                | RuntimeState::WarmingUp
                | RuntimeState::StartingGateway
                | RuntimeState::Ready
                | RuntimeState::Degraded
        )
    }

    pub fn is_starting(&self) -> bool {
        matches!(
            self,
            RuntimeState::Validating
                | RuntimeState::StartingMlx
                | RuntimeState::WarmingUp
                | RuntimeState::StartingGateway
        )
    }
}

/// 状态全量快照（前端轮询 + 事件推送共用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalServiceStatus {
    pub state: RuntimeState,
    pub phase_detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    pub base_url: String,
    pub mlx_port: u16,
    pub gateway_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_mlx: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_gateway: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// 网关当前活跃请求数（/studio/metrics，ready 时轮询；None=未知）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_requests: Option<u32>,
}

impl LocalServiceStatus {
    pub fn stopped(settings: &LocalSettings) -> Self {
        Self {
            state: RuntimeState::Stopped,
            phase_detail: String::new(),
            model_path: None,
            model_name: None,
            base_url: settings.base_url(),
            mlx_port: settings.mlx_port,
            gateway_port: settings.gateway_port,
            pid_mlx: None,
            pid_gateway: None,
            last_error: None,
            started_at: None,
            active_requests: None,
        }
    }
}

/// 日志条目（有界环形缓冲）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalLogEntry {
    pub id: u64,
    pub source: String,
    pub level: String,
    pub line: String,
    pub ts: i64,
}

const MAX_LOG_ENTRIES: usize = 2000;
const PROCESS_KILL_GRACE_SECS: u64 = 8;

struct RuntimeSlot {
    status: LocalServiceStatus,
    logs: VecDeque<LocalLogEntry>,
    log_seq: u64,
    /// 子进程常驻 slot：一旦 spawn 立即入槽，退出路径统一 shutdown。
    mlx_child: Option<Child>,
    gateway_child: Option<Child>,
    operation: Option<tauri::async_runtime::JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
    /// 看门狗自动重启计数
    watchdog_restarts: u32,
}

/// 本地服务管理器（AppState 持有，可 Clone 共享）。
#[derive(Clone)]
pub struct LocalManager {
    slot: Arc<Mutex<RuntimeSlot>>,
    app_handle: Arc<Mutex<Option<tauri::AppHandle>>>,
}

impl Default for LocalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalManager {
    pub fn new() -> Self {
        Self {
            slot: Arc::new(Mutex::new(RuntimeSlot {
                status: LocalServiceStatus::stopped(&LocalSettings::default()),
                logs: VecDeque::new(),
                log_seq: 0,
                mlx_child: None,
                gateway_child: None,
                operation: None,
                stop_flag: Arc::new(AtomicBool::new(false)),
                watchdog_restarts: 0,
            })),
            app_handle: Arc::new(Mutex::new(None)),
        }
    }

    /// 挂载 AppHandle（事件推送用；setup 时调用一次）。
    pub fn attach(&self, app_handle: tauri::AppHandle) {
        if let Ok(mut guard) = self.app_handle.lock() {
            *guard = Some(app_handle);
        }
        self.spawn_metrics_poller();
        self.spawn_watchdog();
    }

    /// 看门狗：服务就绪后每 20s 发最小探测请求；连续 3 次失败判定卡死 → 自动重启。
    fn spawn_watchdog(&self) {
        let manager = self.clone();
        tauri::async_runtime::spawn(async move {
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(2))
                .timeout(std::time::Duration::from_secs(25))
                .no_proxy()
                .build()
                .unwrap_or_default();
            let mut fails = 0u32;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let (ready, base_url) = {
                    let Ok(guard) = manager.slot.lock() else { continue };
                    (
                        guard.status.state == RuntimeState::Ready,
                        guard.status.base_url.clone(),
                    )
                };
                if !ready {
                    fails = 0;
                    continue;
                }
                let probe = client
                    .post(format!("{base_url}/v1/chat/completions"))
                    .json(&serde_json::json!({
                        "model": crate::local::GATEWAY_MODEL_ALIAS,
                        "messages": [{ "role": "user", "content": "ping" }],
                        "max_tokens": 1,
                        "stream": false,
                    }))
                    .send()
                    .await;
                match probe {
                    Ok(resp) if resp.status().is_success() => {
                        fails = 0;
                    }
                    _ => {
                        // 关键：区分"忙碌"与"卡死"。
                        // 服务正被长 prefill 占用时（并发=1），探测会排队超时——
                        // 这是正常状态，绝不能触发重启（会掐掉用户活跃请求 → ECONNRESET）。
                        let busy = manager.gateway_has_active_request().await;
                        if busy {
                            fails = 0;
                            log::debug!("[local] 看门狗探测失败但网关有活跃请求（prefill 忙碌），不判定卡死");
                            continue;
                        }
                        fails += 1;
                        log::warn!("[local] 看门狗探测失败（{fails}/3），空闲态无响应，判定卡死");
                        if fails >= 3 {
                            log::error!("[local] 看门狗判定服务卡死，自动重启");
                            fails = 0;
                            if let Ok(mut guard) = manager.slot.lock() {
                                guard.watchdog_restarts += 1;
                            }
                            manager.stop().await;
                            let settings = load_local_settings();
                            let Some(model_path) = settings.selected_model_path.clone() else {
                                log::warn!("[local] 看门狗无模型可重启");
                                continue;
                            };
                            let db = crate::database::Database::init().ok();
                            let model = db.and_then(|db| {
                                crate::commands::resolve_model(&db, &model_path).ok()
                            });
                            if let Some(model) = model {
                                if let Err(e) = manager.start(model, settings).await {
                                    log::error!("[local] 看门狗自动重启失败: {e}");
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    /// 网关活跃请求轮询：每 2s 读 /studio/metrics，更新状态并推送变化。
    /// 仅 ready 时有效；不阻塞状态机（锁外 emit）。
    fn spawn_metrics_poller(&self) {
        let manager = self.clone();
        tauri::async_runtime::spawn(async move {
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(1))
                .timeout(std::time::Duration::from_secs(2))
                .no_proxy()
                .build()
                .unwrap_or_default();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let (ready, base_url) = {
                    let Ok(guard) = manager.slot.lock() else { continue };
                    (
                        guard.status.state == RuntimeState::Ready,
                        guard.status.base_url.clone(),
                    )
                };
                if !ready {
                    // 翻回未知（服务未就绪时不算活跃）
                    let mut changed = false;
                    if let Ok(mut guard) = manager.slot.lock() {
                        if guard.status.active_requests.is_some() {
                            guard.status.active_requests = None;
                            changed = true;
                        }
                    }
                    if changed {
                        manager.emit_status();
                    }
                    continue;
                }
                let Ok(resp) = client.get(format!("{base_url}/studio/metrics")).send().await else {
                    continue;
                };
                let Ok(value) = resp.json::<serde_json::Value>().await else {
                    continue;
                };
                let active = value
                    .get("active_requests")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
                    .min(u32::MAX as u64) as u32;
                let mut changed = false;
                {
                    let Ok(mut guard) = manager.slot.lock() else { continue };
                    if guard.status.active_requests != Some(active) {
                        guard.status.active_requests = Some(active);
                        changed = true;
                    }
                }
                if changed {
                    manager.emit_status();
                }
            }
        });
    }

    /// 启动时刷新状态形状（base_url/port），不触碰子进程。
    pub fn refresh_status_shape(&self, settings: &LocalSettings) {
        if let Ok(mut guard) = self.slot.lock() {
            guard.status.base_url = settings.base_url();
            guard.status.mlx_port = settings.mlx_port;
            guard.status.gateway_port = settings.gateway_port;
        }
        self.emit_status();
    }

    pub async fn status(&self) -> LocalServiceStatus {
        self.slot
            .lock()
            .map(|guard| guard.status.clone())
            .unwrap_or_else(|_| LocalServiceStatus::stopped(&LocalSettings::default()))
    }

    pub fn logs_since(&self, since_id: u64) -> Vec<LocalLogEntry> {
        self.slot
            .lock()
            .map(|guard| {
                guard
                    .logs
                    .iter()
                    .filter(|entry| entry.id > since_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn clear_logs(&self) {
        if let Ok(mut guard) = self.slot.lock() {
            guard.logs.clear();
        }
    }

    fn push_log(&self, slot: &mut RuntimeSlot, source: &str, level: &str, line: impl Into<String>) {
        slot.log_seq += 1;
        slot.logs.push_back(LocalLogEntry {
            id: slot.log_seq,
            source: source.to_string(),
            level: level.to_string(),
            line: line.into(),
            ts: chrono::Utc::now().timestamp_millis(),
        });
        while slot.logs.len() > MAX_LOG_ENTRIES {
            slot.logs.pop_front();
        }
    }

    /// 只赋值状态；事件推送由调用方在锁外调用 `emit_status()`。
    /// 注意：绝不能在持锁路径内发事件（emit_status 会再锁 slot → 重入死锁）。
    fn set_status(&self, slot: &mut RuntimeSlot, status: LocalServiceStatus) {
        log::info!("[local] set_status -> {}", status.state);
        slot.status = status;
    }

    fn emit_status(&self) {
        // try_lock：防御性设计——如果调用方仍持有 slot 锁（重入路径），
        // 静默跳过本次推送（下一个锁外调用会补发最新状态）
        let Ok(guard) = self.slot.try_lock() else {
            return;
        };
        let status = guard.status.clone();
        drop(guard);
        let handle = self.app_handle.lock().ok().and_then(|g| g.clone());
        let Some(handle) = handle else { return };
        // 异步发送：绝不阻塞调用方（run() 状态机）——若 webview 繁忙，emit 是 fire-and-forget
        tauri::async_runtime::spawn(async move {
            if let Err(e) = handle.emit(LOCAL_STATUS_EVENT, &status) {
                log::warn!("[local] 状态事件发送失败: {e}");
            }
        });
    }

    pub(crate) fn emit_auto_switch(&self, app_type: &str) {
        let handle = self.app_handle.lock().ok().and_then(|g| g.clone());
        if let Some(handle) = handle {
            let app_type = app_type.to_string();
            tauri::async_runtime::spawn(async move {
                let _ = handle.emit(
                    LOCAL_AUTO_SWITCH_EVENT,
                    serde_json::json!({ "appType": app_type }),
                );
            });
        }
    }

    /// 启动本地服务（异步编排，立即返回；就绪前状态经事件/轮询可见）。
    ///
    /// - 同模型已在运行 → Ok（幂等）
    /// - 其他模型在运行 → 先停再启（G5：端点和别名不变）
    /// - 启动中/停止中 → 错误
    pub async fn start(
        &self,
        model: LocalModelRecord,
        settings: LocalSettings,
    ) -> Result<(), String> {
        if !cfg!(target_os = "macos") {
            return Err("本地模型服务仅支持 macOS（Apple Silicon）".to_string());
        }

        let (current_state, current_model) = {
            let guard = self.slot.lock().map_err(|_| "本地服务状态不可用".to_string())?;
            (guard.status.state, guard.status.model_path.clone())
        };

        match current_state {
            state if state.is_starting() => {
                return Err("本地服务正在启动中，请稍候".to_string());
            }
            RuntimeState::Stopping => {
                return Err("本地服务正在停止，请稍候".to_string());
            }
            RuntimeState::Ready | RuntimeState::Degraded => {
                if current_model.as_deref() == Some(model.path.as_str()) {
                    return Ok(());
                }
                self.stop().await;
            }
            _ => {}
        }

        let manager = self.clone();
        let stop_flag = Arc::new(AtomicBool::new(false));
        {
            let mut guard = self.slot.lock().map_err(|_| "本地服务状态不可用".to_string())?;
            guard.stop_flag = stop_flag.clone();
            guard.operation = None;
        }

        let handle = tauri::async_runtime::spawn(async move {
            manager.run(model, settings, stop_flag).await;
        });

        if let Ok(mut guard) = self.slot.lock() {
            guard.operation = Some(handle);
        }
        Ok(())
    }

    /// 停止本地服务：置位停止标记 → 等待操作任务退出（任务内统一 shutdown）→
    /// 兜底强杀残留子进程 → 置 Stopped。
    pub async fn stop(&self) {
        {
            if let Ok(guard) = self.slot.lock() {
                guard.stop_flag.store(true, Ordering::SeqCst);
            }
        }

        let operation = {
            let mut guard = match self.slot.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            guard.operation.take()
        };
        if let Some(handle) = operation {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(PROCESS_KILL_GRACE_SECS * 3),
                handle,
            )
            .await;
        }

        // 兜底：无论操作任务是否还活着，清理槽内子进程
        self.shutdown().await;
        self.finish_stopped();
    }

    /// 主操作流程（状态机）。
    async fn run(
        &self,
        model: LocalModelRecord,
        settings: LocalSettings,
        stop_flag: Arc<AtomicBool>,
    ) {
        log::info!(
            "[local] run() 开始: model={}, mlx_port={}, gateway_port={}",
            model.path, settings.mlx_port, settings.gateway_port
        );
        if stop_flag.load(Ordering::SeqCst) {
            log::info!("[local] run() stop_flag 已置位，直接停止");
            self.shutdown().await;
            self.finish_stopped();
            return;
        }

        // 阶段 1：校验
        if let Err(err) = self.validate(&settings, &model) {
            log::error!("[local] run() 校验失败: {err}");
            self.finish_failed(&stop_flag, err).await;
            return;
        }
        log::info!("[local] run() 校验通过");

        if settings.backend == "ollama" {
            // ===== ollama 后端：模型由 ollama serve 管理（与 ollama 共用模型文件） =====
            if !self.ollama_available(&settings).await {
                self.finish_failed(
                    &stop_flag,
                    format!("ollama 服务不可达：{}", settings.ollama_base_url),
                )
                .await;
                return;
            }
            self.set_phase(RuntimeState::StartingMlx, "使用 ollama 后端".to_string());
            log::info!(
                "[local] ollama 后端：model={}，执行预热",
                settings.ollama_model
            );
            if let Err(err) = self.warmup_ollama(&settings, &stop_flag).await {
                self.finish_failed(&stop_flag, err).await;
                return;
            }
            log::info!("[local] ollama 预热完成");
        } else {
            // ===== MLX 后端：受管 mlx_lm.server =====
            match self.launch_mlx(&model, &settings).await {
                Ok(child) => {
                    log::info!("[local] run(): 拿到了 Child，准备写入 slot");
                    if let Ok(mut guard) = self.slot.lock() {
                        guard.mlx_child = Some(child);
                        log::info!("[local] run(): Child 已写入 slot（锁释放）");
                    } else {
                        log::error!("[local] run(): slot lock 失败（Mutex poisoned?）");
                    }
                }
                Err(err) => {
                    self.finish_failed(&stop_flag, err).await;
                    return;
                }
            }
            log::info!("[local] run(): set_phase(StartingMlx) 之前");
            self.set_phase(RuntimeState::StartingMlx, "启动 MLX 服务".to_string());
            log::info!("[local] run(): set_phase 完成，进入 wait_for_health");

            if !self
                .wait_for_health(&settings, settings.mlx_port, 120, &stop_flag, "MLX")
                .await
            {
                self.finish_failed(&stop_flag, "等待 MLX 服务就绪超时或进程退出".to_string()).await;
                return;
            }
            log::info!("[local] MLX 健康检查通过，进入预热");

            // 阶段 3：预热（权重加载 + 最小推理）
            self.set_phase(RuntimeState::WarmingUp, "加载权重并执行最小预热请求".to_string());
            log::info!("[local] 发送预热请求 {}...", settings.mlx_base_url());
            if let Err(err) = self.warmup(&settings, &stop_flag).await {
                log::error!("[local] 预热失败: {err}");
                self.finish_failed(&stop_flag, err).await;
                return;
            }
            log::info!("[local] 预热完成");
        }

        // 阶段 4：启动网关
        match self.launch_gateway(&settings).await {
            Ok(child) => {
                if let Ok(mut guard) = self.slot.lock() {
                    guard.gateway_child = Some(child);
                }
            }
            Err(err) => {
                self.finish_failed(&stop_flag, err).await;
                return;
            }
        }
        self.set_phase(
            RuntimeState::StartingGateway,
            "启动 OpenAI / Anthropic 网关".to_string(),
        );

        if !self
            .wait_for_health(&settings, settings.gateway_port, 30, &stop_flag, "Gateway")
            .await
        {
            self.finish_failed(&stop_flag, "等待网关就绪超时".to_string()).await;
            return;
        }

        // 阶段 5：就绪
        let (pid_mlx, pid_gateway) = {
            if let Ok(guard) = self.slot.lock() {
                (
                    guard.mlx_child.as_ref().and_then(|c| c.id()),
                    guard.gateway_child.as_ref().and_then(|c| c.id()),
                )
            } else {
                (None, None)
            }
        };
        if let Ok(mut guard) = self.slot.lock() {
            let mut status = guard.status.clone();
            status.state = RuntimeState::Ready;
            status.phase_detail = "OpenAI 与 Anthropic 接口均已就绪".to_string();
            status.pid_mlx = pid_mlx;
            status.pid_gateway = pid_gateway;
            status.started_at = Some(chrono::Utc::now().timestamp());
            status.last_error = None;
            self.push_log(
                &mut guard,
                "app",
                "info",
                format!("Local API 已就绪：{}", settings.base_url()),
            );
            self.set_status(&mut guard, status);
            guard.operation = None;
        }
        self.emit_status();
    }

    async fn finish_failed(&self, stop_flag: &Arc<AtomicBool>, error: String) {
        log::error!("[local] finish_failed: {error}");
        if stop_flag.load(Ordering::SeqCst) {
            // 用户主动停止：不需要 failed 态
            self.shutdown().await;
            return self.finish_stopped();
        }
        // 统一清理子进程后标记失败
        self.shutdown().await;
        if let Ok(mut guard) = self.slot.lock() {
            self.push_log(&mut guard, "app", "error", error.clone());
            let mut status = guard.status.clone();
            status.state = RuntimeState::Failed;
            status.phase_detail = error.clone();
            status.last_error = Some(error);
            status.pid_mlx = None;
            status.pid_gateway = None;
            self.set_status(&mut guard, status);
            guard.operation = None;
        }
        self.emit_status();
    }

    fn finish_stopped(&self) {
        log::info!("[local] finish_stopped");
        if let Ok(mut guard) = self.slot.lock() {
            let mut status = guard.status.clone();
            status.state = RuntimeState::Stopped;
            status.phase_detail = "已停止".to_string();
            status.pid_mlx = None;
            status.pid_gateway = None;
            status.started_at = None;
            self.push_log(&mut guard, "app", "info", "本地服务已停止");
            self.set_status(&mut guard, status);
            guard.operation = None;
        }
        self.emit_status();
    }

    fn set_phase(&self, state: RuntimeState, phase: String) {
        if let Ok(mut guard) = self.slot.lock() {
            let mut status = guard.status.clone();
            status.state = state;
            status.phase_detail = phase.clone();
            self.push_log(&mut guard, "app", "info", phase);
            self.set_status(&mut guard, status);
        }
        // 锁外推送（持锁路径绝不发事件）
        self.emit_status();
    }

    /// 串行清理：先网关后 MLX，SIGTERM → 限时等待 → SIGKILL（M0 直接 KILL 兜底）。
    async fn shutdown(&self) {
        log::info!("[local] shutdown()");
        let mut mlx = None;
        let mut gateway = None;
        if let Ok(mut guard) = self.slot.lock() {
            mlx = guard.mlx_child.take();
            gateway = guard.gateway_child.take();
        }

        for mut child in [gateway, mlx].into_iter().flatten() {
            if let Ok(Some(_)) = child.try_wait() {
                continue; // 已退出
            }
            let _ = child.start_kill(); // SIGKILL（M0 兜底；M2 改为 TERM→等待）
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(PROCESS_KILL_GRACE_SECS),
                child.wait(),
            )
            .await;
        }
    }

    fn validate(&self, settings: &LocalSettings, model: &LocalModelRecord) -> Result<(), String> {
        if !std::path::Path::new(&model.path).join("config.json").is_file() {
            return Err(format!("模型目录不可用：{}", model.path));
        }
        if crate::local::settings::is_tcc_protected_path(std::path::Path::new(&model.path)) {
            return Err(
                "模型目录位于 macOS 受保护目录（Documents/Desktop/Downloads）下：\
                 未授权应用访问时系统会阻塞模型加载。\
                 请把模型移到 .cache/huggingface 等非保护位置或授予本应用「文件与文件夹」访问权限"
                    .to_string(),
            );
        }
        if settings.python_path.is_empty() {
            return Err("未找到 Python 解释器（需要 mlx_lm），请在设置中配置".to_string());
        }
        if !std::path::Path::new(&settings.python_path).is_file() {
            return Err(format!("找不到 Python 可执行文件：{}", settings.python_path));
        }
        // TCC：Documents/Desktop/Downloads 下的 venv 会让 app 派生进程阻塞在
        // open()（python 启动即读 pyvenv.cfg），直接给出可操作的错误。
        if crate::local::settings::is_tcc_protected_path(std::path::Path::new(&settings.python_path)) {
            return Err(
                "Python 解释器位于 macOS 受保护目录（Documents/Desktop/Downloads）下：\
                 未授权应用访问时系统会阻塞进程启动。\
                 请将 venv 移到非保护位置（如 ~/.mlx-lm-venv）或授予本应用「文件与文件夹 → Documents」访问权限"
                    .to_string(),
            );
        }
        if settings.proxy_path.is_empty() {
            return Err("未找到 anthropic-proxy 网关二进制，请在设置中配置".to_string());
        }
        if !std::path::Path::new(&settings.proxy_path).is_file() {
            return Err(format!("找不到网关可执行文件：{}", settings.proxy_path));
        }
        if settings.mlx_port == settings.gateway_port
            || !(1..=65_535).contains(&settings.mlx_port)
            || !(1..=65_535).contains(&settings.gateway_port)
        {
            return Err("端口配置无效：MLX 与网关端口必须不同且在 1-65535 之间".to_string());
        }
        Ok(())
    }

    async fn launch_mlx(
        &self,
        model: &LocalModelRecord,
        settings: &LocalSettings,
    ) -> Result<Child, String> {
        let overrides = settings.effective_overrides(&model.path);
        let max_tokens = overrides.max_tokens.unwrap_or(settings.max_tokens_fallback).max(1);
        // prompt cache 关闭（确定配置）：cache+stream 在 16K+ 上下文级仍会卡死
        // （8K 正常/16K 卡死，边界不可控）；无 cache 时 135KB 流式请求 239s 首字节可用。
        let _ = recommended_prompt_cache_bytes(model.size_bytes);

        let args = vec![
            "-m".to_string(),
            "mlx_lm.server".to_string(),
            "--model".to_string(),
            model.path.clone(),
            "--host".to_string(),
            settings.host.clone(),
            "--port".to_string(),
            settings.mlx_port.to_string(),
            "--max-tokens".to_string(),
            max_tokens.to_string(),
            "--prefill-step-size".to_string(),
            settings.prefill_step_size.to_string(),
            "--prompt-concurrency".to_string(),
            settings.prompt_concurrency.to_string(),
            "--decode-concurrency".to_string(),
            settings.decode_concurrency.to_string(),
            "--log-level".to_string(),
            "INFO".to_string(),
        ];

        self.push_owned_log(
            "app",
            "info",
            format!(
                "MLX 参数：max_tokens={max_tokens}, prefill={}, concurrency={}/{}（prompt cache 关闭：Qwen3.5 cache+stream 卡死）",
                settings.prefill_step_size,
                settings.prompt_concurrency,
                settings.decode_concurrency,
            ),
        );

        let mut command = Command::new(&settings.python_path);
        command
            .args(&args)
            .env("PYTHONUNBUFFERED", "1")
            .kill_on_drop(false)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        self.apply_common_env(&mut command);

        // 端口占用预检：以 lsof 实际监听为准（探活对 wedged 实例会漏判），
        // 先回收自家遗留实例，再以监听结果为准。
        if !self.port_listener_pids(settings.mlx_port).await.is_empty() {
            if self
                .reclaim_own_orphans(settings.mlx_port, "mlx_lm.server")
                .await
                && self.port_listener_pids(settings.mlx_port).await.is_empty()
            {
                log::info!("[local] 端口 {} 已回收，继续启动", settings.mlx_port);
            } else {
                log::error!("[local] 端口 {} 已被占用（非自有实例或回收失败）", settings.mlx_port);
                return Err(format!("端口 {} 已被占用", settings.mlx_port));
            }
        }

        log::info!("[local] spawn mlx_lm.server: {} {}", settings.python_path, args.join(" "));
        let mut child = command
            .spawn()
            .map_err(|e| format!("启动 mlx_lm.server 失败：{e}"))?;
        let pid = child.id().unwrap_or(0);
        log::info!("[local] mlx_lm.server spawn 成功 PID={pid}");
        self.pipe_output(&mut child, "mlx");
        self.push_owned_log("app", "info", format!("mlx_lm.server 已启动，PID {pid}"));
        Ok(child)
    }

    async fn launch_gateway(&self, settings: &LocalSettings) -> Result<Child, String> {
        let isolated_home = crate::config::get_app_config_dir();
        let port = settings.gateway_port.to_string();
        // ollama 后端：上游指向 ollama OpenAI API，模型名映射为 ollama tag
        let (upstream_base, model_alias) = if settings.backend == "ollama" {
            (format!("{}/v1", settings.ollama_base_url), settings.ollama_model.clone())
        } else {
            (format!("{}/v1", settings.mlx_base_url()), crate::local::GATEWAY_MODEL_ALIAS.to_string())
        };
        let mut command = Command::new(&settings.proxy_path);
        command
            .stdin(std::process::Stdio::null())
            .args(["--bind", &settings.host, "--port", &port])
            .env("UPSTREAM_BASE_URL", &upstream_base)
            .env(
                "UPSTREAM_TIMEOUT_SECS",
                settings.upstream_timeout_secs.to_string(),
            )
            .env("ANTHROPIC_PROXY_BIND", &settings.host)
            .env("COMPLETION_MODEL", &model_alias)
            .env("REASONING_MODEL", &model_alias)
            .env("HOME", &isolated_home)
            .kill_on_drop(false);
        self.apply_common_env(&mut command);

        if !self.port_listener_pids(settings.gateway_port).await.is_empty() {
            if self
                .reclaim_own_orphans(settings.gateway_port, "anthropic-proxy")
                .await
                && self.port_listener_pids(settings.gateway_port).await.is_empty()
            {
                log::info!("[local] 端口 {} 已回收，继续启动", settings.gateway_port);
            } else {
                log::error!("[local] 端口 {} 已被占用（非自有实例或回收失败）", settings.gateway_port);
                return Err(format!("端口 {} 已被占用", settings.gateway_port));
            }
        }

        let mut child = command
            .spawn()
            .map_err(|e| format!("启动网关失败：{e}"))?;
        let pid = child.id().unwrap_or(0);
        self.pipe_output(&mut child, "gateway");
        self.push_owned_log("app", "info", format!("网关已启动，PID {pid}"));
        Ok(child)
    }

    fn apply_common_env(&self, command: &mut Command) {
        command.env(
            "PATH",
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin",
        );
        if let Some(home) = dirs::home_dir() {
            command.env("HOME", home);
        }
        command.env("LANG", "en_US.UTF-8");
        for key in ["TMPDIR", "USER", "LOGNAME"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }
    }

    fn pipe_output(&self, child: &mut Child, source: &str) {
        let manager = self.clone();
        let source_stdout = source.to_string();
        if let Some(stdout) = child.stdout.take() {
            let manager = manager.clone();
            let source = source_stdout.clone();
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    manager.push_owned_log(&source, log_level_for_line(&line), line);
                }
            });
        }
        // stderr 与 stdout 合并
        if let Some(stderr) = child.stderr.take() {
            let manager = manager.clone();
            let source = format!("{source_stdout}-err");
            tauri::async_runtime::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    manager.push_owned_log(&source, log_level_for_line(&line), line);
                }
            });
        }
    }

    /// 查询网关活跃请求（/studio/metrics）——看门狗区分"忙碌"与"卡死"。
    async fn gateway_has_active_request(&self) -> bool {
        let (ready, base_url) = {
            let Ok(guard) = self.slot.lock() else { return false };
            (guard.status.state == RuntimeState::Ready, guard.status.base_url.clone())
        };
        if !ready {
            return false;
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap_or_default();
        match client.get(format!("{base_url}/studio/metrics")).send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) => v
                    .get("active_requests")
                    .and_then(|x| x.as_u64())
                    .map(|n| n > 0)
                    .unwrap_or(false),
                Err(_) => false,
            },
            Err(_) => false,
        }
    }

    fn push_owned_log(&self, source: &str, level: &str, line: String) {
        if let Ok(mut guard) = self.slot.lock() {
            self.push_log(&mut guard, source, level, line);
        }
    }

    /// 回收遗留的自有实例（mlx_lm.server / anthropic-proxy），
    /// 仅杀命令行特征匹配的进程，绝不误杀其它服务。
    async fn reclaim_own_orphans(&self, port: u16, marker: &str) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let Ok(output) = tokio::process::Command::new("lsof")
                .args(["-ti", &format!("tcp:{port}")])
                .output()
                .await
            else {
                return false;
            };
            let text = String::from_utf8_lossy(&output.stdout);
            let mut killed = false;
            for line in text.lines() {
                let pid = line.trim();
                if pid.is_empty() {
                    continue;
                }
                let Ok(cmd_output) = tokio::process::Command::new("ps")
                    .args(["-p", pid, "-o", "command="])
                    .output()
                    .await
                else {
                    continue;
                };
                let cmd = String::from_utf8_lossy(&cmd_output.stdout);
                if cmd.contains(marker) {
                    log::warn!(
                        "[local] 回收自家遗留实例（端口 {port}）：PID {pid} -> {}",
                        cmd.trim().chars().take(80).collect::<String>()
                    );
                    let _ = tokio::process::Command::new("kill")
                        .args(["-9", pid])
                        .status()
                        .await;
                    killed = true;
                }
            }
            if killed {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            return killed;
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (port, marker);
            false
        }
    }

    /// 端口实际监听者 PID 列表（lsof 为准）。
    /// 注意：不能用 HTTP 探活代替——wedged 实例（accept 后不响应，
    /// 如 stdout 管道断裂导致的请求级 BrokenPipeError）会给出 "Empty reply"，
    /// 此时必须仍按"端口被占"处理并回收，否则新进程无法 bind。
    async fn port_listener_pids(&self, port: u16) -> Vec<String> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let Ok(output) = tokio::process::Command::new("lsof")
                .args(["-ti", &format!("tcp:{port}"), "-sTCP:LISTEN"])
                .output()
                .await
            else {
                return Vec::new();
            };
            return String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = port;
            Vec::new()
        }
    }

    /// ollama 服务可用性（GET /api/version）。
    async fn ollama_available(&self, settings: &LocalSettings) -> bool {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(4))
            .no_proxy()
            .build()
            .unwrap_or_default();
        client
            .get(format!("{}/api/version", settings.ollama_base_url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// ollama 预热：用 ollama tag 发最小生成请求。
    async fn warmup_ollama(
        &self,
        settings: &LocalSettings,
        stop_flag: &AtomicBool,
    ) -> Result<(), String> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(600))
            .no_proxy()
            .build()
            .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
        let body = serde_json::json!({
            "model": settings.ollama_model,
            "messages": [{ "role": "user", "content": "Reply with OK." }],
            "max_tokens": 1,
            "stream": false,
        });
        let request = client
            .post(format!("{}/v1/chat/completions", settings.ollama_base_url))
            .json(&body)
            .send();
        let response = tokio::select! {
            response = request => response,
            _ = wait_for_stop(stop_flag) => {
                return Err("已取消".to_string());
            }
        };
        let response = response.map_err(|e| format!("ollama 预热请求失败：{e}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("ollama 预热失败（HTTP {status}）：{}", truncate_pub(&text, 300)));
        }
        Ok(())
    }

    async fn wait_for_health(
        &self,
        settings: &LocalSettings,
        port: u16,
        timeout_secs: u64,
        stop_flag: &AtomicBool,
        name: &str,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(1))
            .timeout(std::time::Duration::from_secs(2))
            .no_proxy()
            .build()
            .unwrap_or_default();
        let url = format!("http://{}:{port}/health", settings.host);

        let mut attempts: u32 = 0;
        loop {
            if stop_flag.load(Ordering::SeqCst) {
                log::info!("[local] {name} 健康检查被 stop_flag 中止");
                return false;
            }
            attempts += 1;
            match client.get(&url).send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        log::info!("[local] {name} 健康检查通过（第 {attempts} 次，HTTP {}）", resp.status());
                        self.push_owned_log("app", "info", format!("{name} 健康检查通过"));
                        return true;
                    }
                    log::warn!("[local] {name} 健康检查第 {attempts} 次: HTTP {}", resp.status());
                }
                Err(e) => {
                    if attempts <= 5 || attempts % 10 == 0 {
                        log::warn!("[local] {name} 健康检查第 {attempts} 次失败: {e}");
                    }
                }
            }
            // 子进程提前退出 → 直接失败
            let exited_status = if let Ok(mut guard) = self.slot.lock() {
                let child = if name == "MLX" {
                    guard.mlx_child.as_mut()
                } else {
                    guard.gateway_child.as_mut()
                };
                child.and_then(|c| c.try_wait().ok().flatten())
            } else {
                None
            };
            if let Some(status) = exited_status {
                log::error!(
                    "[local] {name} 子进程提前退出（exit={:?}, signal={:?}），健康检查失败",
                    status.code(),
                    status.signal()
                );
                // 转储最近环形日志（MLX/网关 stderr）到文件，便于定位崩溃原因
                let tail = self.logs_since(0);
                let dump: Vec<String> = tail
                    .iter()
                    .skip(tail.len().saturating_sub(10))
                    .map(|e| format!("[{}][{}] {}", e.source, e.level, e.line))
                    .collect();
                log::error!("[local] {name} 最近输出: {}", dump.join(" | "));
                return false;
            }
            if tokio::time::Instant::now() >= deadline {
                log::error!("[local] {name} 健康检查超时（{timeout_secs}s），共尝试 {attempts} 次");
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    async fn warmup(&self, settings: &LocalSettings, stop_flag: &AtomicBool) -> Result<(), String> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(1800))
            .no_proxy()
            .build()
            .map_err(|e| format!("HTTP 客户端创建失败：{e}"))?;
        let body = serde_json::json!({
            "model": crate::local::GATEWAY_MODEL_ALIAS,
            "messages": [{ "role": "user", "content": "Reply with OK." }],
            "max_tokens": 1,
            "temperature": 0,
            "stream": false,
        });

        let request = client
            .post(format!("{}/v1/chat/completions", settings.mlx_base_url()))
            .json(&body)
            .send();

        let response = tokio::select! {
            response = request => response,
            _ = wait_for_stop(stop_flag) => {
                return Err("已取消".to_string());
            }
        };
        let response = response.map_err(|e| format!("预热请求失败：{e}"))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(format!("模型预热失败（HTTP {status}）：{}", truncate_pub(&text, 500)));
        }
        self.push_owned_log("app", "info", "模型预热完成".to_string());
        Ok(())
    }
}

/// 等待 stop_flag 置位（用于 select!）。
async fn wait_for_stop(stop_flag: &AtomicBool) {
    while !stop_flag.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn log_level_for_line(line: &str) -> &'static str {
    if line.contains("ERROR") || line.contains("Traceback") || line.contains("panic") {
        "error"
    } else if line.contains("WARNING") {
        "warn"
    } else {
        "info"
    }
}

pub(crate) fn truncate_pub(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    }
}
