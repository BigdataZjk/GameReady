//! Merge preference templates without replacing account/session data.
#[derive(Clone, Debug, PartialEq)]
enum Value { Text(String), Object(Vec<(String, Value)>) }

fn parse(text: &str) -> Result<Vec<(String, Value)>, String> {
    let bytes = text.trim_start_matches('\u{feff}').as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b if b.is_ascii_whitespace() => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
            },
            b'{' | b'}' => { tokens.push((bytes[i] as char).to_string()); i += 1; },
            b'"' => {
                i += 1; let start = i;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' { i += 1; }
                    i += 1;
                }
                if i >= bytes.len() { return Err("VDF 字符串未闭合，原文件已保留".into()); }
                tokens.push(format!("\"{}", std::str::from_utf8(&bytes[start..i]).map_err(|_| "VDF 编码错误")?));
                i += 1;
            },
            _ => return Err("VDF 格式无法识别，原文件已保留".into()),
        }
    }
    fn object(tokens: &[String], i: &mut usize, nested: bool, depth: usize) -> Result<Vec<(String, Value)>, String> {
        if depth > 64 { return Err("VDF 嵌套层数过多".into()); }
        let mut pairs = Vec::new();
        while let Some(token) = tokens.get(*i) {
            *i += 1;
            if token == "}" {
                if nested { return Ok(pairs); }
                return Err("VDF 大括号不匹配".into());
            }
            let key = token.strip_prefix('"').ok_or("VDF 缺少键名")?.to_string();
            let token = tokens.get(*i).ok_or("VDF 缺少字段值")?;
            *i += 1;
            let value = if token == "{" { Value::Object(object(tokens, i, true, depth + 1)?) }
                else { Value::Text(token.strip_prefix('"').ok_or("VDF 字段值无效")?.into()) };
            pairs.push((key, value));
        }
        if nested { return Err("VDF 大括号未闭合".into()); }
        Ok(pairs)
    }
    object(&tokens, &mut 0, false, 0)
}

fn merge_into(target: &mut Vec<(String, Value)>, patch: &[(String, Value)]) {
    for (key, value) in patch {
        if let Some((_, current)) = target.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            match (current, value) {
                (Value::Object(current), Value::Object(patch)) => merge_into(current, patch),
                (current, value) => *current = value.clone(),
            }
        } else { target.push((key.clone(), value.clone())); }
    }
}

fn render(pairs: &[(String, Value)], depth: usize, output: &mut String) {
    let indent = "\t".repeat(depth);
    for (key, value) in pairs {
        match value {
            Value::Text(value) => output.push_str(&format!("{indent}\"{key}\"\t\t\"{value}\"\n")),
            Value::Object(children) => {
                output.push_str(&format!("{indent}\"{key}\"\n{indent}{{\n"));
                render(children, depth + 1, output);
                output.push_str(&format!("{indent}}}\n"));
            },
        }
    }
}

pub fn merge(existing: &[u8], template: &[u8]) -> Result<Vec<u8>, String> {
    let mut target = parse(std::str::from_utf8(existing).map_err(|_| "现有 Steam 配置不是 UTF-8，原文件已保留")?)?;
    let patch = parse(std::str::from_utf8(template).map_err(|_| "内置 Steam 配置编码错误")?)?;
    if patch.len() != 1 { return Err("内置 Steam 配置根节点无效".into()); }
    if !target.is_empty() {
        if target.len() != 1 { return Err("现有 Steam 配置根节点无效，原文件已保留".into()); }
        if patch[0].0 == "UserLocalConfigStore" && target[0].0 == "InstallConfigStore" {
            // 修复旧版账号模板使用全局根名的问题，同时保留已有内容。
            target[0].0 = patch[0].0.clone();
        }
        if !target[0].0.eq_ignore_ascii_case(&patch[0].0) { return Err("现有 Steam 配置类型不匹配，原文件已保留".into()); }
    }
    merge_into(&mut target, &patch);
    let mut output = String::new(); render(&target, 0, &mut output);
    Ok(output.into_bytes())
}

pub fn validate_users(text: &str) -> Result<(), String> {
    let tree = parse(text)?;
    if tree.len() != 1 || !tree[0].0.eq_ignore_ascii_case("users") { return Err("loginusers.vdf 根节点无效，原文件已保留".into()); }
    let Value::Object(users) = &tree[0].1 else { return Err("loginusers.vdf 账号列表格式无效".into()); };
    for (id, value) in users {
        let Value::Object(fields) = value else { return Err("loginusers.vdf 账号块格式无效".into()); };
        if id.parse::<u64>().is_err() || fields.iter().any(|(_, v)| !matches!(v, Value::Text(_))) {
            return Err("loginusers.vdf 含无法识别的账号字段，原文件已保留".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_merge_without_losing_other_fields() {
        let original = br#""UserLocalConfigStore" { "system" { "InGameOverlayShortcutKey" "OLD" "unknown" "keep" } "session" { "fixture" "unchanged" } }"#;
        let patch = br#""UserLocalConfigStore" { "system" { "InGameOverlayShortcutKey" "KEY_NONE" } }"#;
        let result = merge(original, patch).unwrap();
        let text = String::from_utf8(result.clone()).unwrap();
        assert!(text.contains("unchanged")); assert!(text.contains("keep")); assert!(!text.contains("OLD"));
        assert_eq!(merge(&result, patch).unwrap(), result);
    }
    #[test]
    fn malformed_text_is_rejected_and_escaped_text_round_trips() {
        assert!(merge(b"broken", br#""InstallConfigStore" {}"#).is_err());
        assert!(merge(br#""InstallConfigStore" { "x" "unfinished""#, br#""InstallConfigStore" {}"#).is_err());
        let original = "\u{feff}\"InstallConfigStore\" { // comment\n\"name\" \"escaped \\\"quote\\\" 中文\" }";
        let result = merge(original.as_bytes(), br#""InstallConfigStore" {}"#).unwrap();
        assert_eq!(parse(original).unwrap(), parse(std::str::from_utf8(&result).unwrap()).unwrap());
    }
}
