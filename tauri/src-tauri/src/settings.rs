//! settings.json 读写与开机自启（注册表 Run 键）。
//!
//! 配置从 exe 旁的 settings.json 读取（避免源码硬编码用户环境路径，
//! 保证开源仓库不泄露个人信息）。字段均为可选：
//!   - webPort 默认 3080；junctionPath / dDrivePath 仅在显式配置时作为
//!     DSH 安装位置的候选之一，未配置时自动识别（npm 全局 / npx 缓存）
//!   - pluginProfile 插件管理目标 profile，默认 web（可在设置页修改）
//!   - registry npm registry 镜像地址，空 = 官方源（可在设置页修改）
//!   - closeAction 点击关闭按钮的行为：tray（隐藏到托盘）/ quit（直接退出）

use serde::{Deserialize, Serialize};

use crate::util::{beside_exe, lock_ok, write_atomic};

#[derive(Deserialize, Serialize, Clone)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Settings {
    pub(crate) junction_path: String,
    pub(crate) d_drive_path: String,
    pub(crate) web_port: u16,
    pub(crate) plugin_profile: String,
    pub(crate) registry: String,
    pub(crate) close_action: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            junction_path: String::new(),
            d_drive_path: String::new(),
            web_port: 3080,
            plugin_profile: "web".into(),
            registry: String::new(),
            close_action: "tray".into(),
        }
    }
}

// 运行时可变（设置页保存后更新），不再用 OnceLock
static SETTINGS: std::sync::Mutex<Option<Settings>> = std::sync::Mutex::new(None);

/// settings.json 加载失败的原因（文件存在但格式无效时非空）。
/// 静默回退默认配置会让用户毫无感知，这里记录下来，由自检面板展示。
static SETTINGS_LOAD_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn settings() -> Settings {
    let mut g = lock_ok(&SETTINGS);
    if g.is_none() {
        *g = Some(load_settings());
    }
    g.clone().unwrap()
}

/// settings.json 存在但解析失败的原因（自检面板展示用）
pub(crate) fn settings_load_error() -> Option<String> {
    lock_ok(&SETTINGS_LOAD_ERROR).clone()
}

fn load_settings() -> Settings {
    // 查找顺序：exe 旁 -> 当前工作目录
    let mut candidates = vec![beside_exe("settings.json")];
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("settings.json"));
    }
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<Settings>(&content) {
                Ok(mut s) => {
                    if !valid_profile_name(&s.plugin_profile) {
                        *lock_ok(&SETTINGS_LOAD_ERROR) = Some(format!(
                            "{} 中的 pluginProfile 无效，已回退为 web",
                            path.display()
                        ));
                        s.plugin_profile = Settings::default().plugin_profile;
                    } else {
                        *lock_ok(&SETTINGS_LOAD_ERROR) = None;
                    }
                    return s;
                }
                Err(e) => {
                    // 文件存在但解析失败：继续尝试下一个候选，但必须把原因
                    // 记下来供自检面板展示（曾静默回退默认值，用户配置的
                    // webPort 等悄悄失效且无从排查）
                    *lock_ok(&SETTINGS_LOAD_ERROR) =
                        Some(format!("{} 解析失败：{}", path.display(), e));
                }
            }
        }
    }
    Settings::default()
}

fn valid_profile_name(name: &str) -> bool {
    let t = name.trim();
    !t.is_empty()
        && !t.starts_with('.')
        && !t.contains('/')
        && !t.contains('\\')
        && !t.eq_ignore_ascii_case("node_modules")
        && !t.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// 设置页的部分更新补丁
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsPatch {
    plugin_profile: Option<String>,
    registry: Option<String>,
    close_action: Option<String>,
}

#[tauri::command]
pub(crate) fn get_settings() -> Settings {
    settings()
}

/// 保存设置页改动到 settings.json（保留 junctionPath / dDrivePath / webPort 等
/// 原有字段），并立即更新内存配置。
#[tauri::command]
pub(crate) fn save_settings(patch: SettingsPatch) -> Result<Settings, String> {
    let mut s = settings();
    if let Some(v) = patch.plugin_profile {
        let t = v.trim().to_string();
        // 除路径分隔符外，还必须拒绝 . / .. / 点开头名称：
        // profile_dir() = profiles\<name>，"." / ".." 会逃逸到 profiles 之外
        if !valid_profile_name(&t) {
            return Err("无效的 profile 名称".into());
        }
        s.plugin_profile = t;
    }
    if let Some(v) = patch.registry {
        let t = v.trim().to_string();
        if !t.is_empty() && !t.starts_with("http://") && !t.starts_with("https://") {
            return Err("registry 地址需以 http:// 或 https:// 开头".into());
        }
        s.registry = t;
    }
    if let Some(v) = patch.close_action {
        if v != "tray" && v != "quit" {
            return Err("closeAction 只能是 tray 或 quit".into());
        }
        s.close_action = v;
    }
    // 合并写回 settings.json（保留文件中未管理的字段）
    let path = beside_exe("settings.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    // 文件存在但解析失败时必须拒绝保存：以空对象兜底会静默丢掉
    // junctionPath / dDrivePath / webPort 等本命令不管理的字段（不可逆）
    let json: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return Err(format!(
                    "settings.json 格式无效（{}），请先手工修复该文件再保存设置",
                    e
                ))
            }
        }
    };
    let mut json = json;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| "settings.json 格式无效：根节点必须是 JSON 对象".to_string())?;
    obj.insert("pluginProfile".into(), serde_json::json!(s.plugin_profile));
    obj.insert("registry".into(), serde_json::json!(s.registry));
    obj.insert("closeAction".into(), serde_json::json!(s.close_action));
    let serialized = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("序列化 settings.json 失败：{}", e))?;
    write_atomic(&path, &serialized, "settings")?;
    *lock_ok(&SETTINGS) = Some(s.clone());
    Ok(s)
}

// ---------------------------------------------------------------------------
// 开机自启（注册表 Run 键）
// ---------------------------------------------------------------------------
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DSHLauncher";

#[tauri::command]
pub(crate) fn get_autostart() -> bool {
    use windows_registry::*;
    match CURRENT_USER.open(RUN_KEY) {
        Ok(key) => key
            .get_string(RUN_VALUE)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
        Err(_) => false,
    }
}

#[tauri::command]
pub(crate) fn set_autostart(enabled: bool) -> Result<bool, String> {
    use windows_registry::*;
    let exe = std::env::current_exe().map_err(|e| format!("获取 exe 路径失败：{}", e))?;
    let path = exe.to_string_lossy().to_string();
    let key = CURRENT_USER
        .create(RUN_KEY)
        .map_err(|e| format!("打开注册表失败：{}", e))?;
    if enabled {
        // Run 键值必须带引号：含空格的未引号路径会被逐段试探解析
        // （C:\Program Files\... 可能被拆成 C:\Program.exe），导致自启失效
        let quoted = format!("\"{}\"", path);
        key.set_string(RUN_VALUE, &quoted)
            .map_err(|e| format!("写入注册表失败：{}", e))?;
    } else {
        let _ = key.remove_value(RUN_VALUE);
    }
    Ok(enabled)
}

#[cfg(test)]
mod tests {
    use super::valid_profile_name;

    #[test]
    fn profile_name_rejects_traversal_and_separators() {
        assert!(valid_profile_name("web"));
        assert!(valid_profile_name("dsh-tui"));
        assert!(!valid_profile_name("."));
        assert!(!valid_profile_name(".."));
        assert!(!valid_profile_name(".hidden"));
        assert!(!valid_profile_name("a/b"));
        assert!(!valid_profile_name("a\\b"));
        assert!(!valid_profile_name("node_modules"));
        assert!(!valid_profile_name("NODE_MODULES"));
        assert!(!valid_profile_name(""));
        assert!(!valid_profile_name("a b"));
    }
}
