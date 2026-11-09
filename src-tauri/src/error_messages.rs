pub const VALIDATION_NAME_REQUIRED: &str = "periodic.validation.nameRequired";
pub const VALIDATION_SOURCE_REQUIRED: &str = "periodic.validation.sourceRequired";
pub const VALIDATION_SOURCE_INVALID: &str = "periodic.validation.sourceInvalid";
pub const VALIDATION_SOURCE_MISMATCH: &str = "periodic.validation.sourceMismatch";
pub const VALIDATION_SOURCE_DUPLICATE: &str = "periodic.validation.sourceDuplicate";
pub const CANCEL_NOT_MANAGED: &str = "periodic.cancel.notManaged";
pub const CANCEL_STOP_FAILED: &str = "periodic.cancel.stopFailed";

#[allow(dead_code)]
pub const ALL_UI_MESSAGE_KEYS: &[&str] = &[
    VALIDATION_NAME_REQUIRED,
    VALIDATION_SOURCE_REQUIRED,
    VALIDATION_SOURCE_INVALID,
    VALIDATION_SOURCE_MISMATCH,
    VALIDATION_SOURCE_DUPLICATE,
    CANCEL_NOT_MANAGED,
    CANCEL_STOP_FAILED,
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    fn flatten_json_keys(value: &Value, prefix: &str, keys: &mut HashSet<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let full_key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    flatten_json_keys(value, &full_key, keys);
                }
            }
            _ => {
                keys.insert(prefix.to_string());
            }
        }
    }

    fn locale_keys(locale: &str) -> HashSet<String> {
        let value: Value = serde_json::from_str(locale).expect("locale must be valid JSON");
        let mut keys = HashSet::new();
        flatten_json_keys(&value, "", &mut keys);
        keys
    }

    #[test]
    fn all_ui_message_keys_exist_in_each_locale() {
        for (locale_name, locale) in [
            ("ja", include_str!("../../src/i18n/locales/ja.json")),
            ("en", include_str!("../../src/i18n/locales/en.json")),
        ] {
            let keys = locale_keys(locale);
            for key in super::ALL_UI_MESSAGE_KEYS {
                assert!(
                    keys.contains(*key),
                    "{key} is missing from the {locale_name} locale"
                );
            }
        }
    }
}
