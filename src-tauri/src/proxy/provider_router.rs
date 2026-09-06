//! 供应商路由器模块
//!
//! 负责选择和管理代理目标供应商，实现智能故障转移

use crate::app_config::AppType;
use crate::database::Database;
use crate::error::AppError;
use crate::provider::Provider;
use crate::proxy::circuit_breaker::{AllowResult, CircuitBreaker, CircuitBreakerConfig};
use crate::proxy::model_routes::{self, ModelRoute};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

/// 供应商路由器
pub struct ProviderRouter {
    /// 数据库连接
    db: Arc<Database>,
    /// 熔断器管理器 - key 格式: "app_type:provider_id"
    circuit_breakers: Arc<RwLock<HashMap<String, Arc<CircuitBreaker>>>>,
}

impl ProviderRouter {
    /// 创建新的供应商路由器
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            circuit_breakers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 选择可用的供应商（支持故障转移）
    ///
    /// 返回按优先级排序的可用供应商列表：
    /// - 故障转移关闭时：仅返回当前供应商
    /// - 故障转移开启时：仅使用故障转移队列，按队列顺序依次尝试（P1 → P2 → ...）
    pub async fn select_providers(&self, app_type: &str) -> Result<Vec<Provider>, AppError> {
        let mut result = Vec::new();
        let mut total_providers = 0usize;
        let mut circuit_open_count = 0usize;

        // 检查该应用的自动故障转移开关是否开启（从 proxy_config 表读取）
        let auto_failover_enabled = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(config) => config.auto_failover_enabled,
            Err(e) => {
                log::error!("[{app_type}] 读取 proxy_config 失败: {e}，默认禁用故障转移");
                false
            }
        };

        if auto_failover_enabled {
            // 故障转移开启：仅按队列顺序依次尝试（P1 → P2 → ...）
            let all_providers = self.db.get_all_providers(app_type)?;

            // 使用 DAO 返回的排序结果，确保和前端展示一致
            let ordered_ids: Vec<String> = self
                .db
                .get_failover_queue(app_type)?
                .into_iter()
                .map(|item| item.provider_id)
                .collect();

            total_providers = ordered_ids.len();

            for provider_id in ordered_ids {
                let Some(provider) = all_providers.get(&provider_id).cloned() else {
                    continue;
                };

                let circuit_key = format!("{app_type}:{}", provider.id);
                let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

                if breaker.is_available().await {
                    result.push(provider);
                } else {
                    circuit_open_count += 1;
                }
            }
        } else {
            // 故障转移关闭：仅使用当前供应商，跳过熔断器检查
            let current_id = AppType::from_str(app_type)
                .ok()
                .and_then(|app_enum| {
                    crate::settings::get_effective_current_provider(&self.db, &app_enum)
                        .ok()
                        .flatten()
                })
                .or_else(|| self.db.get_current_provider(app_type).ok().flatten());

            if let Some(current_id) = current_id {
                if let Some(current) = self.db.get_provider_by_id(&current_id, app_type)? {
                    total_providers = 1;
                    result.push(current);
                }
            }
        }

        if result.is_empty() {
            if total_providers > 0 && circuit_open_count == total_providers {
                log::warn!("[{app_type}] [FO-004] 所有供应商均已熔断");
                return Err(AppError::AllProvidersCircuitOpen);
            } else {
                log::warn!("[{app_type}] [FO-005] 未配置供应商");
                return Err(AppError::NoProvidersConfigured);
            }
        }

        Ok(result)
    }

    /// 按请求中的模型别名选择供应商。
    ///
    /// 普通模型保持既有的“当前供应商 / 故障转移队列”语义。Codex 聚合
    /// catalog 产生的 `ccs/...` 模型则绕过全局 current provider，直接绑定
    /// 到别名所属的供应商；路由请求只返回这一家，避免一次失败无提示地消耗
    /// 另一套餐额度。熔断器放行仍由 forwarder 在真正发请求前统一处理。
    ///
    /// 当前供应商是复合供应商时：
    /// - `ccs/...` 模型按成员 ID 从 DB 实时解析成员供应商（URL/密钥取最新）；
    /// - 无前缀普通模型路由到第一个成员，并合成 `route_id == upstream_model`
    ///   的兜底路由（forwarder 改写为 no-op，且不触发全局切换）。
    pub async fn select_providers_for_model(
        &self,
        app_type: &str,
        request_model: &str,
    ) -> Result<(Vec<Provider>, Option<ModelRoute>), AppError> {
        let providers = match self.select_providers(app_type).await {
            Ok(providers) => {
                if providers.first().map(model_routes::is_composite) == Some(true) {
                    return self.select_for_composite(app_type, &providers, request_model);
                }
                providers
            }
            Err(err) => {
                // Codex 的聚合 `ccs/` 解析不依赖 current provider（目录覆盖
                // 所有供应商），没有 current 时也必须能按别名路由。
                if app_type != "codex" || !model_routes::is_route_model(request_model) {
                    return Err(err);
                }
                Vec::new()
            }
        };

        if app_type != "codex" || !model_routes::is_route_model(request_model) {
            return Ok((providers, None));
        }

        let all_providers = self.db.get_all_providers(app_type)?;
        let route = model_routes::resolve_route(all_providers.values(), request_model).ok_or_else(
            || {
                AppError::InvalidInput(format!(
                    "未知的模型路由: {request_model}。请刷新 Codex 模型目录后重试"
                ))
            },
        )?;
        let provider = all_providers.get(&route.provider_id).cloned().ok_or_else(|| {
            AppError::InvalidInput(format!(
                "模型路由 {} 指向的供应商不存在: {}",
                route.route_id, route.provider_id
            ))
        })?;
        Ok((vec![provider], Some(route)))
    }

    /// 复合供应商的路由选择。
    ///
    /// 成员快照只决定目录展示；这里按 `providerId` 从 DB 重新加载成员，
    /// 保证转发永远使用成员的最新 URL / 密钥。快照中已不存在的成员被跳过。
    fn select_for_composite(
        &self,
        app_type: &str,
        selected: &[Provider],
        request_model: &str,
    ) -> Result<(Vec<Provider>, Option<ModelRoute>), AppError> {
        let composite = selected
            .iter()
            .find(|provider| model_routes::is_composite(provider))
            .ok_or_else(|| AppError::NoProvidersConfigured)?;
        let member_ids = model_routes::composite_member_ids(composite);

        let all_providers = self.db.get_all_providers(app_type)?;
        let members: Vec<Provider> = member_ids
            .iter()
            .filter_map(|id| all_providers.get(id).cloned())
            .collect();

        if members.is_empty() {
            return Err(AppError::InvalidInput(format!(
                "复合供应商 {} 的成员均不存在，请重新创建复合供应商",
                composite.name
            )));
        }

        if model_routes::is_route_model(request_model) {
            // 路由 id 来自复合快照（成员自身的 modelCatalog 不一定存在，
            // Claude 成员尤其如此），成员 provider 只负责提供 URL / 密钥。
            let route = model_routes::composite_routes(composite)
                .into_iter()
                .find(|route| route.route_id == request_model)
                .ok_or_else(|| {
                    AppError::InvalidInput(format!(
                        "未知的模型路由: {request_model}。请刷新模型目录后重试"
                    ))
                })?;
            let member = members
                .iter()
                .find(|provider| provider.id == route.provider_id)
                .ok_or_else(|| {
                    AppError::InvalidInput(format!(
                        "模型路由 {} 指向的供应商不存在: {}",
                        route.route_id, route.provider_id
                    ))
                })?;
            return Ok((vec![member.clone()], Some(route)));
        }

        // 无前缀普通模型：兜底路由到第一个成员，按原名转发。
        let first = members.first().expect("checked non-empty above");
        let route = model_routes::composite_fallback_route(first, request_model);
        Ok((vec![first.clone()], Some(route)))
    }

    /// 请求执行前获取熔断器“放行许可”
    ///
    /// - Closed：直接放行
    /// - Open：超时到达后切到 HalfOpen 并放行一次探测
    /// - HalfOpen：按限流规则放行探测
    ///
    /// 注意：调用方必须在请求结束后通过 `record_result()` 释放 HalfOpen 名额，
    /// 否则会导致该 Provider 长时间无法进入探测状态。
    pub async fn allow_provider_request(&self, provider_id: &str, app_type: &str) -> AllowResult {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.allow_request().await
    }

    /// 记录供应商请求结果
    pub async fn record_result(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
        success: bool,
        error_msg: Option<String>,
    ) -> Result<(), AppError> {
        // 1. 按应用独立获取熔断器配置
        let failure_threshold = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => app_config.circuit_failure_threshold,
            Err(_) => 5, // 默认值
        };

        // 2. 更新熔断器状态
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;

        if success {
            breaker.record_success(used_half_open_permit).await;
        } else {
            breaker.record_failure(used_half_open_permit).await;
        }

        // 3. 更新数据库健康状态（使用配置的阈值）
        self.db
            .update_provider_health_with_threshold(
                provider_id,
                app_type,
                success,
                error_msg.clone(),
                failure_threshold,
            )
            .await?;

        Ok(())
    }

    /// 重置熔断器（手动恢复）
    pub async fn reset_circuit_breaker(&self, circuit_key: &str) {
        let breakers = self.circuit_breakers.read().await;
        if let Some(breaker) = breakers.get(circuit_key) {
            breaker.reset().await;
        }
    }

    /// 重置指定供应商的熔断器
    pub async fn reset_provider_breaker(&self, provider_id: &str, app_type: &str) {
        let circuit_key = format!("{app_type}:{provider_id}");
        self.reset_circuit_breaker(&circuit_key).await;
    }

    /// 仅释放 HalfOpen permit，不影响健康统计（neutral 接口）
    ///
    /// 用于整流器等场景：请求结果不应计入 Provider 健康度，
    /// 但仍需释放占用的探测名额，避免 HalfOpen 状态卡死
    pub async fn release_permit_neutral(
        &self,
        provider_id: &str,
        app_type: &str,
        used_half_open_permit: bool,
    ) {
        if !used_half_open_permit {
            return;
        }
        let circuit_key = format!("{app_type}:{provider_id}");
        let breaker = self.get_or_create_circuit_breaker(&circuit_key).await;
        breaker.release_half_open_permit();
    }

    /// 更新所有熔断器的配置（热更新）
    pub async fn update_all_configs(&self, config: CircuitBreakerConfig) {
        let breakers = self.circuit_breakers.read().await;
        for breaker in breakers.values() {
            breaker.update_config(config.clone()).await;
        }
    }

    /// 更新指定应用已创建熔断器的配置（热更新）
    pub async fn update_app_configs(&self, app_type: &str, config: CircuitBreakerConfig) {
        let prefix = format!("{app_type}:");
        let breakers = self.circuit_breakers.read().await;
        for (key, breaker) in breakers.iter() {
            if key.starts_with(&prefix) {
                breaker.update_config(config.clone()).await;
            }
        }
    }

    /// 获取熔断器状态
    #[allow(dead_code)]
    pub async fn get_circuit_breaker_stats(
        &self,
        provider_id: &str,
        app_type: &str,
    ) -> Option<crate::proxy::circuit_breaker::CircuitBreakerStats> {
        let circuit_key = format!("{app_type}:{provider_id}");
        let breakers = self.circuit_breakers.read().await;

        if let Some(breaker) = breakers.get(&circuit_key) {
            Some(breaker.get_stats().await)
        } else {
            None
        }
    }

    /// 获取或创建熔断器
    async fn get_or_create_circuit_breaker(&self, key: &str) -> Arc<CircuitBreaker> {
        // 先尝试读锁获取
        {
            let breakers = self.circuit_breakers.read().await;
            if let Some(breaker) = breakers.get(key) {
                return breaker.clone();
            }
        }

        // 如果不存在，获取写锁创建
        let mut breakers = self.circuit_breakers.write().await;

        // 双重检查，防止竞争条件
        if let Some(breaker) = breakers.get(key) {
            return breaker.clone();
        }

        // 从 key 中提取 app_type (格式: "app_type:provider_id")
        let app_type = key.split(':').next().unwrap_or("claude");

        // 按应用独立读取熔断器配置
        let config = match self.db.get_proxy_config_for_app(app_type).await {
            Ok(app_config) => crate::proxy::circuit_breaker::CircuitBreakerConfig {
                failure_threshold: app_config.circuit_failure_threshold,
                success_threshold: app_config.circuit_success_threshold,
                timeout_seconds: app_config.circuit_timeout_seconds as u64,
                error_rate_threshold: app_config.circuit_error_rate_threshold,
                min_requests: app_config.circuit_min_requests,
            },
            Err(_) => crate::proxy::circuit_breaker::CircuitBreakerConfig::default(),
        };

        let breaker = Arc::new(CircuitBreaker::new(config));
        breakers.insert(key.to_string(), breaker.clone());

        breaker
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use tempfile::TempDir;

    struct TempHome {
        #[allow(dead_code)]
        dir: TempDir,
        original_home: Option<String>,
        original_userprofile: Option<String>,
        original_test_home: Option<String>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = TempDir::new().expect("failed to create temp home");
            let original_home = env::var("HOME").ok();
            let original_userprofile = env::var("USERPROFILE").ok();
            let original_test_home = env::var("CC_SWITCH_TEST_HOME").ok();

            env::set_var("HOME", dir.path());
            env::set_var("USERPROFILE", dir.path());
            env::set_var("CC_SWITCH_TEST_HOME", dir.path());
            crate::settings::reload_settings().expect("reload settings");

            Self {
                dir,
                original_home,
                original_userprofile,
                original_test_home,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.original_home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }

            match &self.original_userprofile {
                Some(value) => env::set_var("USERPROFILE", value),
                None => env::remove_var("USERPROFILE"),
            }

            match &self.original_test_home {
                Some(value) => env::set_var("CC_SWITCH_TEST_HOME", value),
                None => env::remove_var("CC_SWITCH_TEST_HOME"),
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_router_creation() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let router = ProviderRouter::new(db);

        let breaker = router.get_or_create_circuit_breaker("claude:test").await;
        assert!(breaker.allow_request().await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_disabled_uses_current_provider() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_composite_route_resolves_member_from_snapshot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let member_a =
            Provider::with_id("a".to_string(), "来源A".to_string(), json!({}), None);
        let member_b =
            Provider::with_id("b".to_string(), "来源B".to_string(), json!({}), None);
        let mut composite = Provider::with_id(
            "comp".to_string(),
            "复合".to_string(),
            json!({"composite": {"members": [
                {"providerId": "a", "providerName": "来源A", "models": [
                    {"model": "claude-sonnet-4", "displayName": "Sonnet 4"}]},
                {"providerId": "b", "providerName": "来源B", "models": [
                    {"model": "claude-opus-4"}]}
            ]}}),
            None,
        );
        composite.meta = Some(crate::provider::ProviderMeta {
            provider_type: Some("composite".to_string()),
            ..Default::default()
        });

        db.save_provider("claude", &member_a).unwrap();
        db.save_provider("claude", &member_b).unwrap();
        db.save_provider("claude", &composite).unwrap();
        db.set_current_provider("claude", "comp").unwrap();

        let router = ProviderRouter::new(db);
        let (providers, route) = router
            .select_providers_for_model("claude", "ccs/b/claude-opus-4")
            .await
            .unwrap();
        let route = route.expect("composite route");

        assert_eq!(route.upstream_model, "claude-opus-4");
        assert_eq!(route.provider_id, "b");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[0].name, "来源B");
    }

    #[tokio::test]
    #[serial]
    async fn test_composite_plain_model_falls_back_to_first_member() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let member_a =
            Provider::with_id("a".to_string(), "来源A".to_string(), json!({}), None);
        let member_b =
            Provider::with_id("b".to_string(), "来源B".to_string(), json!({}), None);
        let mut composite = Provider::with_id(
            "comp".to_string(),
            "复合".to_string(),
            json!({"composite": {"members": [
                {"providerId": "a", "providerName": "来源A", "models": [
                    {"model": "claude-sonnet-4"}]},
                {"providerId": "b", "providerName": "来源B", "models": [
                    {"model": "claude-opus-4"}]}
            ]}}),
            None,
        );
        composite.meta = Some(crate::provider::ProviderMeta {
            provider_type: Some("composite".to_string()),
            ..Default::default()
        });

        db.save_provider("claude", &member_a).unwrap();
        db.save_provider("claude", &member_b).unwrap();
        db.save_provider("claude", &composite).unwrap();
        db.set_current_provider("claude", "comp").unwrap();

        let router = ProviderRouter::new(db);
        let (providers, route) = router
            .select_providers_for_model("claude", "claude-sonnet-4")
            .await
            .unwrap();
        let route = route.expect("composite fallback route");

        assert_eq!(route.route_id, "claude-sonnet-4");
        assert_eq!(route.upstream_model, "claude-sonnet-4");
        assert_eq!(route.provider_id, "a");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_model_route_selects_provider_without_using_current_or_failover_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a = Provider::with_id(
            "a".to_string(),
            "Provider A".to_string(),
            json!({
                "modelCatalog": {"models": [{"model": "gpt-5.6"}]}
            }),
            None,
        );
        let provider_b = Provider::with_id(
            "b".to_string(),
            "Provider B".to_string(),
            json!({
                "modelCatalog": {"models": [{"model": "gpt-5.6"}]}
            }),
            None,
        );

        db.save_provider("codex", &provider_a).unwrap();
        db.save_provider("codex", &provider_b).unwrap();
        db.set_current_provider("codex", "a").unwrap();

        // b is deliberately not in the failover queue. An explicit route still
        // selects it, because model routing is independent from global failover.
        let router = ProviderRouter::new(db);
        let (providers, route) = router
            .select_providers_for_model("codex", "ccs/b/gpt-5.6")
            .await
            .unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
        let route = route.expect("model route");
        assert_eq!(route.upstream_model, "gpt-5.6");
    }

    #[tokio::test]
    #[serial]
    async fn test_unknown_model_route_is_a_client_input_error() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());
        let provider = Provider::with_id(
            "a".to_string(),
            "Provider A".to_string(),
            json!({"modelCatalog": {"models": [{"model": "gpt-5.6"}]}}),
            None,
        );
        db.save_provider("codex", &provider).unwrap();

        let router = ProviderRouter::new(db);
        let error = router
            .select_providers_for_model("codex", "ccs/a/missing")
            .await
            .expect_err("unknown route must fail");
        assert!(matches!(error, AppError::InvalidInput(_)));
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_order_ignoring_current() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 设置 sort_index 来控制顺序：b=1, a=2
        let mut provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        provider_a.sort_index = Some(2);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        db.add_to_failover_queue("claude", "b").unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 2);
        // 故障转移开启时：仅按队列顺序选择（忽略当前供应商）
        assert_eq!(providers[0].id, "b");
        assert_eq!(providers[1].id, "a");
    }

    #[tokio::test]
    #[serial]
    async fn test_failover_enabled_uses_queue_only_even_if_current_not_in_queue() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let mut provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);
        provider_b.sort_index = Some(1);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();
        db.set_current_provider("claude", "a").unwrap();

        // 只把 b 加入故障转移队列（模拟“当前供应商不在队列里”的常见配置）
        db.add_to_failover_queue("claude", "b").unwrap();

        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());
        let providers = router.select_providers("claude").await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "b");
    }

    #[tokio::test]
    #[serial]
    async fn test_select_providers_does_not_consume_half_open_permit() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        let provider_b =
            Provider::with_id("b".to_string(), "Provider B".to_string(), json!({}), None);

        db.save_provider("claude", &provider_a).unwrap();
        db.save_provider("claude", &provider_b).unwrap();

        db.add_to_failover_queue("claude", "a").unwrap();
        db.add_to_failover_queue("claude", "b").unwrap();

        // 启用自动故障转移（使用新的 proxy_config API）
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        router
            .record_result("b", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        let providers = router.select_providers("claude").await.unwrap();
        assert_eq!(providers.len(), 2);

        assert!(router.allow_provider_request("b", "claude").await.allowed);
    }

    #[tokio::test]
    #[serial]
    async fn test_release_permit_neutral_frees_half_open_slot() {
        let _home = TempHome::new();
        let db = Arc::new(Database::memory().unwrap());

        // 配置熔断器：1 次失败即熔断，0 秒超时立即进入 HalfOpen
        db.update_circuit_breaker_config(&CircuitBreakerConfig {
            failure_threshold: 1,
            timeout_seconds: 0,
            ..Default::default()
        })
        .await
        .unwrap();

        let provider_a =
            Provider::with_id("a".to_string(), "Provider A".to_string(), json!({}), None);
        db.save_provider("claude", &provider_a).unwrap();
        db.add_to_failover_queue("claude", "a").unwrap();

        // 启用自动故障转移
        let mut config = db.get_proxy_config_for_app("claude").await.unwrap();
        config.auto_failover_enabled = true;
        db.update_proxy_config_for_app(config).await.unwrap();

        let router = ProviderRouter::new(db.clone());

        // 触发熔断：1 次失败
        router
            .record_result("a", "claude", false, false, Some("fail".to_string()))
            .await
            .unwrap();

        // 第一次请求：获取 HalfOpen 探测名额
        let first = router.allow_provider_request("a", "claude").await;
        assert!(first.allowed);
        assert!(first.used_half_open_permit);

        // 第二次请求应被拒绝（名额已被占用）
        let second = router.allow_provider_request("a", "claude").await;
        assert!(!second.allowed);

        // 使用 release_permit_neutral 释放名额（不影响健康统计）
        router
            .release_permit_neutral("a", "claude", first.used_half_open_permit)
            .await;

        // 第三次请求应被允许（名额已释放）
        let third = router.allow_provider_request("a", "claude").await;
        assert!(third.allowed);
        assert!(third.used_half_open_permit);
    }
}
