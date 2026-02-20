use rand::Rng;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::domain::context::ScenarioContext;
use crate::domain::error::PulseError;

pub fn render_json_templates(value: &Value, ctx: &ScenarioContext) -> Result<Value, PulseError> {
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), render_json_templates(v, ctx)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(render_json_templates(item, ctx)?);
            }
            Ok(Value::Array(out))
        }
        Value::String(s) => render_string_template(s, ctx).map(Value::String),
        Value::Number(n) => Ok(Value::Number(n.clone())),
        Value::Bool(b) => Ok(Value::Bool(*b)),
        Value::Null => Ok(Value::Null),
    }
}

pub fn render_string_template(input: &str, ctx: &ScenarioContext) -> Result<String, PulseError> {
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0_usize;
    while let Some(start_rel) = input[cursor..].find("${") {
        let start = cursor + start_rel;
        out.push_str(&input[cursor..start]);

        let expr_start = start + 2;
        let Some(end_rel) = input[expr_start..].find('}') else {
            return Err(PulseError::Client(format!(
                "unterminated template expression in '{input}'"
            )));
        };
        let end = expr_start + end_rel;
        let expr = input[expr_start..end].trim();
        let resolved = evaluate_template_expr(expr, ctx)?;
        out.push_str(&resolved);
        cursor = end + 1;
    }
    out.push_str(&input[cursor..]);
    Ok(out)
}

fn evaluate_template_expr(expr: &str, ctx: &ScenarioContext) -> Result<String, PulseError> {
    if let Some(key) = expr.strip_prefix("ctx.") {
        return ctx
            .get(key)
            .map(ToString::to_string)
            .ok_or_else(|| PulseError::MissingContextVar(key.to_string()));
    }

    if expr == "gen.uuid" {
        return Ok(Uuid::new_v4().to_string());
    }
    if expr == "gen.phone" {
        return Ok(generate_phone());
    }
    if let Some(args) = expr.strip_prefix("gen.int:") {
        let mut parts = args.split(':');
        let min = parts
            .next()
            .ok_or_else(|| PulseError::Client(format!("invalid generator expression: {expr}")))?
            .parse::<i64>()
            .map_err(|_| PulseError::Client(format!("invalid int min in expression: {expr}")))?;
        let max = parts
            .next()
            .ok_or_else(|| PulseError::Client(format!("invalid generator expression: {expr}")))?
            .parse::<i64>()
            .map_err(|_| PulseError::Client(format!("invalid int max in expression: {expr}")))?;
        if parts.next().is_some() {
            return Err(PulseError::Client(format!(
                "invalid generator expression: {expr}"
            )));
        }
        if min > max {
            return Err(PulseError::Client(format!(
                "invalid generator bounds in '{expr}': min > max"
            )));
        }
        let mut rng = rand::rng();
        let value: i64 = rng.random_range(min..=max);
        return Ok(value.to_string());
    }

    Err(PulseError::Client(format!(
        "unsupported template expression: {expr}"
    )))
}

fn generate_phone() -> String {
    let mut rng = rand::rng();
    let n1: u16 = rng.random_range(200..999);
    let n2: u16 = rng.random_range(100..999);
    let n3: u16 = rng.random_range(1000..9999);
    format!("+1{n1}{n2}{n3}")
}

pub fn extract_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    if path.trim().is_empty() {
        return Some(current);
    }

    for segment in path.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

pub fn value_to_context_string(value: &Value) -> Result<String, PulseError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Null => Err(PulseError::Client(
            "cannot store null response value in context".to_string(),
        )),
        Value::Object(_) | Value::Array(_) => serde_json::to_string(value)
            .map_err(|e| PulseError::Client(format!("failed to serialize extracted value: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_path, render_string_template};
    use crate::domain::context::ScenarioContext;
    use serde_json::json;

    #[test]
    fn renders_context_and_generator_tokens() {
        let mut ctx = ScenarioContext::default();
        ctx.set("user_id", "u-123");
        let rendered = render_string_template("id=${ctx.user_id};uuid=${gen.uuid}", &ctx)
            .expect("template should render");
        assert!(rendered.starts_with("id=u-123;uuid="));
    }

    #[test]
    fn extracts_nested_object_path() {
        let value = json!({"account": {"id": "abc"}});
        let found = extract_path(&value, "account.id").expect("path should exist");
        assert_eq!(found, "abc");
    }
}
