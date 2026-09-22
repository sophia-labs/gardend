pub(crate) fn json_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

pub(crate) fn json_number(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::Number(value) => value.as_f64(),
        serde_json::Value::String(value) => value.parse::<f64>().ok(),
        _ => None,
    }
}

pub(crate) fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    match value? {
        serde_json::Value::Number(value) => value.as_u64(),
        serde_json::Value::String(value) => value.parse::<u64>().ok(),
        _ => None,
    }
}

pub(crate) fn json_bool(value: Option<&serde_json::Value>) -> Option<bool> {
    match value? {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(value) => value.parse::<bool>().ok(),
        _ => None,
    }
}

pub(crate) fn json_scalar_lexical(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(value) if !value.is_empty() => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_coerces_non_empty_scalar_values() {
        assert_eq!(
            json_string(Some(&serde_json::json!("alpha"))),
            Some("alpha".to_string())
        );
        assert_eq!(
            json_string(Some(&serde_json::json!(42))),
            Some("42".to_string())
        );
        assert_eq!(
            json_string(Some(&serde_json::json!(true))),
            Some("true".to_string())
        );
        assert_eq!(json_string(Some(&serde_json::json!(""))), None);
    }

    #[test]
    fn json_number_and_bool_parse_string_scalars() {
        assert_eq!(json_number(Some(&serde_json::json!("3.5"))), Some(3.5));
        assert_eq!(json_u64(Some(&serde_json::json!("42"))), Some(42));
        assert_eq!(json_bool(Some(&serde_json::json!("true"))), Some(true));
    }
}
