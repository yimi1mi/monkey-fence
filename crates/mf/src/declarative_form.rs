//! 声明式表单模型(UI 计划 Task 1):插件 UI Schema 的宿主侧渲染基础。
//!
//! 插件只提供字段 Schema(id/label/类型/必填/占位/选项),
//! 值与校验由宿主统一持有;Secret 字段只保存引用与掩码,
//! 明文永不进入表单状态。

use std::collections::HashMap;

/// 单个表单字段 Schema(来自插件声明式贡献)。
#[derive(Debug, Clone, PartialEq)]
pub struct FormField {
    pub id: String,
    pub label: String,
    /// text | secret | number | select | boolean
    pub kind: String,
    pub required: bool,
    pub placeholder: String,
    /// select 的可选值。
    pub options: Vec<String>,
}

/// 字段值(Secret 字段只存引用 id,不存明文)。
#[derive(Debug, Clone, PartialEq)]
pub enum FormValue {
    Text(String),
    SecretRef(String),
    Number(f64),
    Boolean(bool),
}

impl FormValue {
    pub fn as_text(&self) -> String {
        match self {
            FormValue::Text(s) => s.clone(),
            FormValue::SecretRef(id) => id.clone(),
            FormValue::Number(n) => n.to_string(),
            FormValue::Boolean(b) => b.to_string(),
        }
    }
}

/// 声明式表单状态:字段 + 值 + 校验,独立于 GPUI 渲染。
/// 字段来自插件 manifest 的 agent_types[].config_schema 文件
/// (JSON `{"fields":[...]}`)或内置类型的代码内声明。
#[derive(Debug, Clone, Default)]
pub struct DeclarativeForm {
    fields: Vec<FormField>,
    values: HashMap<String, FormValue>,
}

impl DeclarativeForm {
    pub fn new(fields: Vec<FormField>) -> DeclarativeForm {
        DeclarativeForm {
            fields,
            values: HashMap::new(),
        }
    }

    /// 从插件 Schema JSON 解析:`{"fields":[{id,label,kind,required,
    /// placeholder,options}]}`。非法字段安全跳过。
    pub fn from_json(schema: &serde_json::Value) -> DeclarativeForm {
        let mut fields = Vec::new();
        if let Some(list) = schema.get("fields").and_then(|v| v.as_array()) {
            for item in list {
                let str_of = |key: &str| {
                    item.get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string()
                };
                let id = str_of("id");
                if id.is_empty() {
                    continue;
                }
                fields.push(FormField {
                    id,
                    label: str_of("label"),
                    kind: {
                        let kind = str_of("kind");
                        if kind.is_empty() {
                            "text".to_string()
                        } else {
                            kind
                        }
                    },
                    required: item
                        .get("required")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    placeholder: str_of("placeholder"),
                    options: item
                        .get("options")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|o| o.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        }
        DeclarativeForm::new(fields)
    }

    pub fn fields(&self) -> &[FormField] {
        &self.fields
    }

    /// 设置字段值(未知字段安全忽略);Secret 字段一律视为引用。
    pub fn set_value(&mut self, id: &str, raw: &str) {
        let Some(field) = self.fields.iter().find(|f| f.id == id) else {
            return;
        };
        let value: Option<FormValue> = match field.kind.as_str() {
            "secret" => Some(FormValue::SecretRef(raw.to_string())),
            "number" => raw.parse::<f64>().ok().map(FormValue::Number),
            "boolean" => Some(FormValue::Boolean(matches!(
                raw.to_lowercase().as_str(),
                "true" | "1" | "yes" | "是"
            ))),
            _ => Some(FormValue::Text(raw.to_string())),
        };
        if let Some(value) = value {
            self.values.insert(id.to_string(), value);
        }
    }

    pub fn get(&self, id: &str) -> Option<&FormValue> {
        self.values.get(id)
    }

    /// 清除字段值(select 字段的"清空"入口);未知字段安全忽略。
    pub fn clear_value(&mut self, id: &str) {
        self.values.remove(id);
    }

    /// 展示值:Secret 字段一律掩码(UI 默认遮罩,设计 §8)。
    pub fn masked_value(&self, id: &str) -> String {
        match self.values.get(id) {
            Some(FormValue::SecretRef(_)) => "••••".into(),
            Some(v) => v.as_text(),
            None => String::new(),
        }
    }

    /// 校验:必填项缺失时返回人类可读错误(含字段 label)。
    pub fn validation(&self) -> Vec<String> {
        self.fields
            .iter()
            .filter(|f| f.required)
            .filter(|f| {
                self.values
                    .get(&f.id)
                    .map(|v| v.as_text().trim().is_empty())
                    .unwrap_or(true)
            })
            .map(|f| format!("「{}」({})为必填项", f.label, f.id))
            .collect()
    }

    /// 导出为 JSON 对象(存入实例 config)。
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for field in &self.fields {
            if let Some(value) = self.values.get(&field.id) {
                map.insert(field.id.clone(), serde_json::Value::String(value.as_text()));
            }
        }
        serde_json::Value::Object(map)
    }
}
