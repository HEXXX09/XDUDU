//! 面向日志、事件和持久化数据的敏感信息脱敏。

use serde_json::Value;

const REDACTED: &str = "[已脱敏]";

fn sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', ' '], "_");
    matches!(
        normalized.as_str(),
        "api_key"
            | "apikey"
            | "access_token"
            | "refresh_token"
            | "token"
            | "secret"
            | "password"
            | "passwd"
            | "authorization"
            | "cookie"
            | "set_cookie"
            | "private_key"
    ) || normalized.ends_with("_api_key")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_password")
}

fn token_end(value: &str, start: usize) -> usize {
    value[start..]
        .char_indices()
        .find_map(|(offset, character)| {
            (!character.is_ascii_alphanumeric()
                && !matches!(character, '-' | '_' | '.' | '/' | '+' | '='))
            .then_some(start + offset)
        })
        .unwrap_or(value.len())
}

fn redact_prefixed(mut value: String, prefix: &str) -> String {
    let mut cursor = 0;
    while let Some(relative) = value[cursor..].find(prefix) {
        let start = cursor + relative;
        let end = token_end(&value, start + prefix.len());
        if end.saturating_sub(start) < prefix.len() + 6 {
            cursor = start + prefix.len();
            continue;
        }
        value.replace_range(start..end, REDACTED);
        cursor = start + REDACTED.len();
    }
    value
}

fn redact_private_keys(mut value: String) -> String {
    while let Some(begin) = value.find("-----BEGIN ") {
        let Some(kind_end) = value[begin..].find("PRIVATE KEY-----") else {
            break;
        };
        let body_start = begin + kind_end + "PRIVATE KEY-----".len();
        let Some(end_start) = value[body_start..].find("-----END ") else {
            value.replace_range(begin.., REDACTED);
            break;
        };
        let end_start = body_start + end_start;
        let closing_body = end_start + "-----".len();
        let Some(end_line) = value[closing_body..].find("-----") else {
            value.replace_range(begin.., REDACTED);
            break;
        };
        let end = closing_body + end_line + "-----".len();
        value.replace_range(begin..end, REDACTED);
    }
    value
}

pub fn redact_text(value: &str) -> String {
    let mut redacted = redact_private_keys(value.to_owned());
    for prefix in [
        "sk-",
        "xai-",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "Bearer ",
        "bearer ",
    ] {
        redacted = redact_prefixed(redacted, prefix);
    }
    redacted
}

pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if sensitive_key(key) {
                        Value::String(REDACTED.into())
                    } else {
                        redact_value(value)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_value).collect()),
        Value::String(value) => Value::String(redact_text(value)),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn 脱敏常见令牌和结构化秘密字段() {
        let value = json!({
            "apiKey": "secret-value",
            "maxOutputTokens": 4096,
            "nested": {
                "message": "token sk-abcdefghijklmnopqrstuvwxyz and xai-1234567890"
            }
        });
        let redacted = redact_value(&value);
        assert_eq!(redacted["apiKey"], REDACTED);
        assert_eq!(redacted["maxOutputTokens"], 4096);
        assert_eq!(redacted["nested"]["message"], "token [已脱敏] and [已脱敏]");
    }

    #[test]
    fn 脱敏私钥块() {
        let text = "before\n-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\nafter";
        assert_eq!(redact_text(text), "before\n[已脱敏]\nafter");
    }
}
