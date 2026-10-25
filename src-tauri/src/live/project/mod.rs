//! 纯函数投影器：供应商行 → 客户端文件里关键字段和独有字段的目标值。
//!
//! 投影器不读盘也不写盘，只给出「这个供应商要求哪些键是什么值」，再转成对应格式的
//! 补丁交给引擎，以现有文件为底改写。

pub mod claude;
pub mod codex;
pub mod gemini;
pub mod grok;

use serde_json::Value;

/// 复合供应商快照里的一个成员。
///
/// 复合卡没有自己的上游：它的模型来自成员。快照是行里的 `settings_config.composite`，
/// 成员按优先级排序，第一个是兜底目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeMember {
    /// 成员供应商的 id：路由用它回到 DB 里取成员真正的地址和密钥。
    pub provider_id: String,
    /// 成员供应商的名字：只用于显示。
    pub provider_name: String,
    /// 成员发布的模型（按目录顺序）。
    pub models: Vec<CompositeModel>,
}

/// 复合供应商快照里的一个模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeModel {
    /// 上游真实模型名：普通名，没有 `ccs/` 前缀。
    pub model: String,
    /// 显示名（没写时用模型名）。
    pub display_name: String,
}

/// 读出行里的复合供应商快照。
///
/// 判据是行里有 `composite.members`：复合卡创建时同时写下这个快照和
/// `meta.providerType = "composite"`（见 `ProviderMeta::provider_type`），两者一起出现。
/// 快照在，投影器才知道「这一家没有单一上游，模型来自成员」。
pub fn composite_members(settings: &Value) -> Option<Vec<CompositeMember>> {
    let members = settings.get("composite")?.get("members")?.as_array()?;
    Some(members.iter().filter_map(composite_member).collect())
}

fn composite_member(member: &Value) -> Option<CompositeMember> {
    let provider_id = member.get("providerId")?.as_str()?.trim();
    if provider_id.is_empty() {
        return None;
    }
    let provider_name = member
        .get("providerName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(provider_id);
    let models = member
        .get("models")
        .and_then(Value::as_array)
        .map(|models| models.iter().filter_map(composite_model).collect())
        .unwrap_or_default();
    Some(CompositeMember {
        provider_id: provider_id.to_string(),
        provider_name: provider_name.to_string(),
        models,
    })
}

fn composite_model(entry: &Value) -> Option<CompositeModel> {
    let model = entry.get("model")?.as_str()?.trim();
    if model.is_empty() {
        return None;
    }
    let display_name = entry
        .get("displayName")
        .or_else(|| entry.get("display_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(model);
    Some(CompositeModel {
        model: model.to_string(),
        display_name: display_name.to_string(),
    })
}

/// 复合供应商的默认模型：快照里第一个成员的第一个模型（普通名，没有 `ccs/` 前缀）。
///
/// 代理按「没有前缀的普通模型 → 第一个成员」的兜底路由转发它，所以这个模型名原样
/// 写进客户端就够，客户端不需要认识路由 id。
pub fn composite_default_model(settings: &Value) -> Option<String> {
    composite_members(settings)?
        .first()?
        .models
        .first()
        .map(|model| model.model.clone())
}

/// 复合供应商的路由 id 列表（`ccs/<成员 id>/<模型>`），顺序和快照一致。
pub fn composite_route_ids(settings: &Value) -> Vec<String> {
    composite_members(settings)
        .map(|members| {
            members
                .iter()
                .flat_map(|member| {
                    member.models.iter().map(|model| {
                        crate::proxy::model_routes::route_id(&member.provider_id, &model.model)
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> Value {
        json!({
            "composite": {
                "members": [
                    {"providerId": "a", "providerName": "来源A", "models": [
                        {"model": "claude-sonnet-4", "displayName": "Sonnet 4"}
                    ]},
                    {"providerId": "b", "providerName": "来源B", "models": [
                        {"model": "claude-opus-4", "displayName": "Opus 4"}
                    ]}
                ]
            }
        })
    }

    #[test]
    fn members_come_from_the_snapshot_in_order() {
        let members = composite_members(&snapshot()).expect("members");
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].provider_id, "a");
        assert_eq!(members[0].provider_name, "来源A");
        assert_eq!(members[0].models[0].model, "claude-sonnet-4");
        assert_eq!(members[0].models[0].display_name, "Sonnet 4");
        assert_eq!(members[1].provider_id, "b");
        assert_eq!(members[1].models[0].model, "claude-opus-4");
    }

    #[test]
    fn the_default_model_is_the_first_members_first_model() {
        assert_eq!(
            composite_default_model(&snapshot()).as_deref(),
            Some("claude-sonnet-4")
        );
    }

    #[test]
    fn route_ids_follow_the_snapshot() {
        assert_eq!(
            composite_route_ids(&snapshot()),
            vec![
                "ccs/a/claude-sonnet-4".to_string(),
                "ccs/b/claude-opus-4".to_string()
            ]
        );
    }

    #[test]
    fn a_row_without_a_snapshot_has_no_members() {
        assert!(composite_members(&json!({"env": {}})).is_none());
        assert!(composite_default_model(&json!({"env": {}})).is_none());
        assert!(composite_route_ids(&json!({"env": {}})).is_empty());
    }
}
