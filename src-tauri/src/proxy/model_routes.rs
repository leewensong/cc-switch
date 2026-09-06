//! 请求级模型路由。
//!
//! Codex 连接到 cc-switch 代理时，客户端看到的是一个聚合后的模型目录。
//! 目录中的 slug 使用稳定的 `ccs/<provider-id>/<model>` 形式；显示名称
//! 可以随着供应商名称变化，但路由本身只依赖 provider id 和真实模型名。
//!
//! 这里故意不使用显示名称解析路由。供应商改名、同名供应商或模型名中含有
//! `/` 都不会改变已保存会话使用的 route id。

use crate::provider::Provider;
use serde_json::Value;
use std::collections::HashSet;

/// cc-switch 聚合模型的稳定前缀。
pub const CODEX_MODEL_ROUTE_PREFIX: &str = "ccs/";

/// 一个客户端可见模型对应的实际上游路由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    /// 发给客户端的稳定模型别名。
    pub route_id: String,
    /// 实际使用的供应商 ID。
    pub provider_id: String,
    /// 供应商显示名称（仅用于 UI / 日志）。
    pub provider_name: String,
    /// 供应商真实模型名。
    pub upstream_model: String,
    /// 客户端模型目录中的显示名称。
    pub display_name: String,
}

/// 返回供应商模型目录中的模型条目。
///
/// `modelCatalog` 是 Codex provider 的 DB SSOT。没有填写目录的旧供应商仍
/// 可以通过顶层 `model`（或 config.toml 中的 model）暴露一个默认路由，方便
/// 老配置逐步迁移到聚合目录。
pub fn provider_model_routes(provider: &Provider) -> Vec<ModelRoute> {
    let mut models = Vec::new();

    if let Some(entries) = provider
        .settings_config
        .get("modelCatalog")
        .and_then(|catalog| catalog.get("models"))
        .and_then(Value::as_array)
    {
        for entry in entries {
            let Some(model) = entry
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
            else {
                continue;
            };

            let display_model = entry
                .get("displayName")
                .or_else(|| entry.get("display_name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(model);

            models.push(route_for_model(provider, model, display_model));
        }
    }

    // 兼容只配置了顶层 model 的旧 Codex provider。显式 modelCatalog 优先，
    // 避免同一个 provider 的默认模型被重复加入目录。
    if models.is_empty() {
        if let Some(model) = crate::proxy::providers::codex_provider_upstream_model(provider) {
            models.push(route_for_model(provider, &model, &model));
        }
    }

    // DB 导入或手工编辑可能产生重复 model；保持首个条目，保证 route id
    // 唯一且 catalog / router 的顺序一致。
    let mut seen = HashSet::new();
    models.retain(|route| seen.insert(route.route_id.clone()));
    models
}

fn route_for_model(provider: &Provider, model: &str, display_model: &str) -> ModelRoute {
    ModelRoute {
        route_id: route_id(&provider.id, model),
        provider_id: provider.id.clone(),
        provider_name: provider.name.clone(),
        upstream_model: model.to_string(),
        display_name: format!("{} · {}", provider.name, display_model),
    }
}

/// 判断供应商是否为复合供应商。
pub fn is_composite(provider: &Provider) -> bool {
    provider
        .meta
        .as_ref()
        .and_then(|meta| meta.provider_type.as_deref())
        == Some("composite")
}

/// 复合供应商快照中的成员 ID（按优先级排序，第一个是兜底目标）。
pub fn composite_member_ids(composite: &Provider) -> Vec<String> {
    composite
        .settings_config
        .get("composite")
        .and_then(|composite| composite.get("members"))
        .and_then(Value::as_array)
        .map(|members| {
            members
                .iter()
                .filter_map(|member| {
                    member
                        .get("providerId")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 从复合供应商的成员快照构建请求级路由。
///
/// 快照只决定客户端目录展示（route id / 显示名）；转发时的 URL 与密钥
/// 由路由器按 `provider_id` 从 DB 实时解析成员供应商获得。
pub fn composite_routes(composite: &Provider) -> Vec<ModelRoute> {
    let Some(members) = composite
        .settings_config
        .get("composite")
        .and_then(|composite| composite.get("members"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };

    let mut models = Vec::new();
    let mut seen = HashSet::new();

    for member in members {
        let Some(provider_id) = member
            .get("providerId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let provider_name = member
            .get("providerName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(provider_id);

        let Some(entries) = member.get("models").and_then(Value::as_array) else {
            continue;
        };

        for entry in entries {
            let Some(model) = entry
                .get("model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
            else {
                continue;
            };

            let display_model = entry
                .get("displayName")
                .or_else(|| entry.get("display_name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(model);

            let route = ModelRoute {
                route_id: route_id(provider_id, model),
                provider_id: provider_id.to_string(),
                provider_name: provider_name.to_string(),
                upstream_model: model.to_string(),
                display_name: format!("{provider_name} · {display_model}"),
            };
            if seen.insert(route.route_id.clone()) {
                models.push(route);
            }
        }
    }

    models
}

/// 从复合供应商快照中移除指定成员，返回更新后的 settings_config。
///
/// 返回 `None` 表示该供应商不是复合、快照缺失或不含此成员。成员清空时
/// 仍返回 `Some`（`members` 为空数组），由调用方决定是否删除整条记录。
pub fn composite_without_member(provider: &Provider, removed_id: &str) -> Option<Value> {
    if !is_composite(provider) {
        return None;
    }
    let members = provider
        .settings_config
        .get("composite")?
        .get("members")?
        .as_array()?;

    if !members
        .iter()
        .any(|member| member.get("providerId").and_then(Value::as_str) == Some(removed_id))
    {
        return None;
    }

    let remaining: Vec<Value> = members
        .iter()
        .filter(|member| member.get("providerId").and_then(Value::as_str) != Some(removed_id))
        .cloned()
        .collect();

    let mut settings = provider.settings_config.clone();
    if let Some(composite) = settings.get_mut("composite").and_then(Value::as_object_mut) {
        composite.insert("members".to_string(), Value::Array(remaining));
    }
    Some(settings)
}

/// 为复合供应商的“无前缀普通模型”合成一条兜底路由。
///
/// `route_id == upstream_model` 使 forwarder 的模型改写成为 no-op，
/// 同时保持 `routed_request` 语义：不触发全局 current provider 切换。
pub fn composite_fallback_route(member: &Provider, model: &str) -> ModelRoute {
    ModelRoute {
        route_id: model.to_string(),
        provider_id: member.id.clone(),
        provider_name: member.name.clone(),
        upstream_model: model.to_string(),
        display_name: model.to_string(),
    }
}

/// 生成稳定 route id。
pub fn route_id(provider_id: &str, model: &str) -> String {
    format!("{CODEX_MODEL_ROUTE_PREFIX}{provider_id}/{model}")
}

/// 判断请求是否尝试使用 cc-switch 模型路由。
pub fn is_route_model(model: &str) -> bool {
    model.starts_with(CODEX_MODEL_ROUTE_PREFIX)
}

/// 在供应商集合中解析一个客户端模型别名。
pub fn resolve_route<'a>(
    providers: impl IntoIterator<Item = &'a Provider>,
    model: &str,
) -> Option<ModelRoute> {
    if !is_route_model(model) {
        return None;
    }

    providers
        .into_iter()
        .flat_map(provider_model_routes)
        .find(|route| route.route_id == model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(id: &str, name: &str, settings: Value) -> Provider {
        Provider::with_id(id.to_string(), name.to_string(), settings, None)
    }

    #[test]
    fn route_ids_are_stable_and_allow_slashes_in_model_names() {
        assert_eq!(
            route_id("provider-a", "vendor/gpt-5.6"),
            "ccs/provider-a/vendor/gpt-5.6"
        );
    }

    #[test]
    fn catalog_models_use_provider_name_only_for_display() {
        let a = provider(
            "a",
            "套餐 A",
            json!({
                "modelCatalog": {"models": [
                    {"model": "gpt-5.6", "displayName": "GPT-5.6"}
                ]}
            }),
        );
        let routes = provider_model_routes(&a);
        assert_eq!(routes[0].route_id, "ccs/a/gpt-5.6");
        assert_eq!(routes[0].upstream_model, "gpt-5.6");
        assert_eq!(routes[0].display_name, "套餐 A · GPT-5.6");
    }

    #[test]
    fn legacy_top_level_model_gets_a_default_route() {
        let a = provider("a", "套餐 A", json!({"model": "gpt-5.6"}));
        let routes = provider_model_routes(&a);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].route_id, "ccs/a/gpt-5.6");
    }

    #[test]
    fn resolver_matches_exact_route_without_parsing_slashes() {
        let a = provider(
            "a",
            "套餐 A",
            json!({"modelCatalog": {"models": [{"model": "vendor/gpt-5.6"}]}}),
        );
        let route = resolve_route([&a], "ccs/a/vendor/gpt-5.6").expect("route");
        assert_eq!(route.provider_id, "a");
        assert_eq!(route.upstream_model, "vendor/gpt-5.6");
        assert!(resolve_route([&a], "ccs/a/unknown").is_none());
    }

    fn composite_provider(id: &str, name: &str) -> Provider {
        let mut composite = provider(
            id,
            name,
            json!({
                "composite": {
                    "members": [
                        {"providerId": "a", "providerName": "来源A", "models": [
                            {"model": "claude-sonnet-4", "displayName": "Sonnet 4"}
                        ]},
                        {"providerId": "b", "providerName": "来源B", "models": [
                            {"model": "gpt-5.6"}
                        ]}
                    ]
                }
            }),
        );
        composite.meta = Some(crate::provider::ProviderMeta {
            provider_type: Some("composite".to_string()),
            ..Default::default()
        });
        composite
    }

    #[test]
    fn composite_routes_use_member_ids_and_prefixed_display_names() {
        let composite = composite_provider("comp", "复合");
        assert!(is_composite(&composite));

        let routes = composite_routes(&composite);
        assert_eq!(routes.len(), 2);

        assert_eq!(routes[0].route_id, "ccs/a/claude-sonnet-4");
        assert_eq!(routes[0].provider_id, "a");
        assert_eq!(routes[0].provider_name, "来源A");
        assert_eq!(routes[0].upstream_model, "claude-sonnet-4");
        assert_eq!(routes[0].display_name, "来源A · Sonnet 4");

        assert_eq!(routes[1].route_id, "ccs/b/gpt-5.6");
        assert_eq!(routes[1].display_name, "来源B · gpt-5.6");

        assert_eq!(composite_member_ids(&composite), vec!["a", "b"]);
    }

    #[test]
    fn non_composite_providers_have_no_composite_routes() {
        let plain = provider("a", "来源A", json!({"model": "claude-sonnet-4"}));
        assert!(!is_composite(&plain));
        assert!(composite_routes(&plain).is_empty());
        assert!(composite_member_ids(&plain).is_empty());
    }

    #[test]
    fn composite_without_member_removes_only_matching_member() {
        let composite = composite_provider("comp", "复合");

        let updated =
            composite_without_member(&composite, "a").expect("member a should be removed");
        let members = updated["composite"]["members"].as_array().unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0]["providerId"], "b");

        assert!(composite_without_member(&composite, "c").is_none());
        let plain = provider("a", "来源A", json!({"model": "claude-sonnet-4"}));
        assert!(composite_without_member(&plain, "a").is_none());
    }

    #[test]
    fn composite_fallback_route_keeps_model_name_as_is() {
        let member = provider("a", "来源A", json!({}));
        let route = composite_fallback_route(&member, "claude-sonnet-4");
        assert_eq!(route.route_id, "claude-sonnet-4");
        assert_eq!(route.upstream_model, "claude-sonnet-4");
        assert_eq!(route.provider_id, "a");
        assert!(!is_route_model(&route.route_id));
    }
}
