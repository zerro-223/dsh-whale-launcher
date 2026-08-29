fn main() {
    // 版本一致性守卫：Cargo.toml 与 tauri.conf.json 的版本号两处手动维护，
    // 构建期校验防止发布时漂移（不一致直接 panic，阻断构建）。
    let conf =
        std::fs::read_to_string("tauri.conf.json").expect("build.rs：无法读取 tauri.conf.json");
    let conf_ver = extract_conf_version(&conf)
        .unwrap_or_else(|| panic!("build.rs：tauri.conf.json 中未找到顶层 \"version\" 字段"));
    let cargo_ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    if conf_ver != cargo_ver {
        panic!(
            "版本号不一致：Cargo.toml = {}，tauri.conf.json = {}（请同步后重新构建）",
            cargo_ver, conf_ver
        );
    }
    tauri_build::build()
}

/// 从 tauri.conf.json 文本中提取顶层 "version" 字段值。
/// build-dependencies 未引入 serde_json，这里做最小化的文本扫描：
/// 取第一个 `"version"` 键之后冒号跟随的字符串字面量（该文件由 Tauri
/// 模板维护，结构固定，version 字段唯一出现）。
fn extract_conf_version(text: &str) -> Option<String> {
    let key_pos = text.find("\"version\"")?;
    let after_key = &text[key_pos + "\"version\"".len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    let quoted = after_colon.strip_prefix('"')?;
    let end = quoted.find('"')?;
    Some(quoted[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::extract_conf_version;

    #[test]
    fn extracts_top_level_version() {
        let conf = r#"{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "DSH-Launcher",
  "version": "1.3.0",
  "identifier": "com.dsh.launcher"
}"#;
        assert_eq!(extract_conf_version(conf).as_deref(), Some("1.3.0"));
        assert_eq!(extract_conf_version("{ }"), None);
        assert_eq!(extract_conf_version(r#"{"version": 3}"#), None);
    }
}
