//! DSH 插件管理（默认 web profile）。
//!
//! 插件机制：插件 = profile 目录（$DSH_HOME/profiles/<pluginProfile>，默认 web）
//! 的 pnpm 依赖；package.json 声明了 "dsh": { "bundle": { "patch": ... } } 的包是
//! "bundle"（profile 层），`dsh.profile.bundles` 按序叠加生效。
//! 安装 / 卸载 / 更新全部走官方 `dsh plugin --profile <name> <add|remove|update>`
//! （内部转发 pnpm 并对账 bundles 列表，见 @deepseek-ai/dsh 的 plugin 子命令），
//! 不直接改 package.json，保证与 CLI / Web UI 行为一致。
//! 启用/禁用通过 profile 的 cordis.patch.yml 的 disabled 条目实现（本启动器
//! 写入的块带标记注释，enable 时只移除自己的块，保留用户手工配置）。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use tauri::ipc::Channel;

use crate::locate::{dsh_home_dir, find_bin, find_dsh, DSH_PKG_NAME};
use crate::npm::{apply_registry, npm_view_publish_time, npm_view_version, run_cmd_streaming};
use crate::proxy::npm_proxy_env;
use crate::settings::settings;
use crate::util::{app_log_line, write_atomic, CREATE_NO_WINDOW};

fn profile_dir() -> PathBuf {
    dsh_home_dir()
        .join("profiles")
        .join(settings().plugin_profile)
}

/// 去掉 spec 中的版本号得到裸包名：`@scope/name@1.0.0` → `@scope/name`；
/// git / file: 等含 `/` 的 spec 原样返回。
fn bare_pkg_name(spec: &str) -> &str {
    let s = spec.trim();
    if let Some(i) = s.rfind('@') {
        if i > 0 && !s[i + 1..].contains('/') {
            return &s[..i];
        }
    }
    s
}

fn validate_plugin_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("请输入要安装的插件包名".into());
    }
    if n.len() > 200 {
        return Err("包名过长（最多 200 字符）".into());
    }
    if n.starts_with('-') {
        return Err("包名不能以 - 开头".into());
    }
    if n.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("包名包含非法字符（空格 / 控制符）".into());
    }
    // `dsh plugin` currently forwards to pnpm through Windows shell=true;
    // reject cmd.exe metacharacters until that upstream path is shell-free.
    if n.chars().any(|c| {
        matches!(
            c,
            '&' | '|' | '<' | '>' | '^' | '(' | ')' | '%' | '!' | ';' | '"' | '\'' | '`'
        )
    }) {
        return Err("包名包含不安全的 shell 字符".into());
    }
    Ok(())
}

/// 已安装插件名的路径安全校验（node_modules/<name> 拼接前必须通过）：
/// 拒绝 `..`、反斜杠与盘符冒号，防路径穿越读任意目录的 package.json。
/// @scope/name 的 `/` 是 node_modules 的合法层级，不拒绝。
fn validate_installed_name(name: &str) -> Result<(), String> {
    validate_plugin_name(name)?;
    let n = name.trim();
    if n.contains("..") || n.contains('\\') || n.contains(':') {
        return Err(format!("无效的插件名：{}", n));
    }
    Ok(())
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginInfo {
    name: String,
    spec: String,    // dependencies 中的 spec（如 ^0.12.2 / file:...），内置组件为空
    version: String, // 已安装版本（读 node_modules 下 package.json），读不到为空
    description: String,
    is_bundle: bool,        // 是否声明了 dsh.bundle.patch（profile 层）
    is_builtin: bool,       // 在 bundles 中但不在 dependencies（随 DSH 安装的内置层）
    enabled: Option<bool>,  // bundle：是否在 bundles 列表；普通依赖：None
    entry_ids: Vec<String>, // bundle patch 中 insert 的入口 id（启用/禁用定位用）
    patch_disabled: bool,   // profile cordis.patch.yml 中是否存在该插件的 disabled 条目
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginListResult {
    profile_dir: String,
    initialized: bool,
    plugins: Vec<PluginInfo>,
}

/// 解析 YAML 列表项行 `- id: <value>`，返回去引号后的 id 值。
/// 容忍冒号后的多余空白与单/双引号包裹（用户手工编辑的常见格式变体）。
fn entry_id_of(trimmed_line: &str) -> Option<String> {
    let rest = trimmed_line.strip_prefix("- id:")?;
    let id = strip_yaml_comment(rest)
        .trim()
        .trim_matches(|c| c == '\'' || c == '"');
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Remove an unquoted YAML comment while preserving `#` inside quoted values.
fn strip_yaml_comment(value: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in value.char_indices() {
        if quote == Some('"') {
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
                continue;
            }
            if c == '"' {
                quote = None;
            }
            continue;
        }
        if c == '\'' {
            quote = if quote == Some('\'') {
                None
            } else {
                Some('\'')
            };
        } else if c == '"' {
            quote = Some('"');
        } else if c == '#' && (i == 0 || value[..i].chars().last().is_some_and(char::is_whitespace))
        {
            return &value[..i];
        }
    }
    value
}

/// `disabled: <value>` 行是否为真值（true/yes/on，大小写不敏感，
/// 容忍空格与引号——YAML 布尔写法不止 `true` 一种）
fn disabled_flag(trimmed_line: &str) -> bool {
    let Some(rest) = trimmed_line.strip_prefix("disabled:") else {
        return false;
    };
    matches!(
        strip_yaml_comment(rest)
            .trim()
            .trim_matches(|c| c == '\'' || c == '"')
            .to_ascii_lowercase()
            .as_str(),
        "true" | "yes" | "on"
    )
}

/// 轻量解析 bundle 的 cordis.patch.yml，提取 insert 块中的条目 id。
/// 只解析本应用需要的结构（顶层数组项的 `- insert:` 下缩进的 `- id: xxx`），
/// 不引入完整 YAML 依赖。
fn extract_insert_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut in_insert = false;
    let mut insert_indent = 0usize;
    for line in content.lines() {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !in_insert {
            if t.starts_with("- insert:") || t.starts_with("insert:") {
                in_insert = true;
                insert_indent = indent;
            }
            continue;
        }
        if t.starts_with("- id:") && indent > insert_indent {
            if let Some(id) = entry_id_of(t) {
                ids.push(id);
            }
            continue;
        }
        if indent <= insert_indent {
            // The current line may itself begin the next top-level insert.
            // Re-open it here instead of dropping that line.
            in_insert = false;
            if t.starts_with("- insert:") || t.starts_with("insert:") {
                in_insert = true;
                insert_indent = indent;
            }
        }
    }
    ids
}

/// 轻量解析 profile 的 cordis.patch.yml，提取所有 `- id: xxx` + `disabled: true`
/// 组合的条目 id（含用户手动配置的禁用项）。
fn read_disabled_ids(content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut states: Vec<(String, bool)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(id) = entry_id_of(line.trim()) else {
            continue;
        };
        let base_indent = line.len() - line.trim_start().len();
        let mut disabled = false;
        for next in lines.iter().skip(i + 1) {
            let next_indent = next.len() - next.trim_start().len();
            let trimmed = next.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if trimmed.starts_with('-') && next_indent <= base_indent {
                break;
            }
            if disabled_flag(trimmed) {
                disabled = true;
                break;
            }
        }
        if let Some((_, state)) = states.iter_mut().find(|(known, _)| known == &id) {
            // Later patch entries override earlier entries with the same id.
            *state = disabled;
        } else {
            states.push((id, disabled));
        }
    }
    states
        .into_iter()
        .filter_map(|(id, disabled)| disabled.then_some(id))
        .collect()
}

/// 读取已安装包的 (version, description, 是否声明 dsh.bundle.patch, 入口 ids)
fn installed_pkg_info(profile: &Path, name: &str) -> Option<(String, String, bool, Vec<String>)> {
    if validate_installed_name(name).is_err() {
        return None;
    }
    let p = profile.join("node_modules").join(name).join("package.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    let version = v
        .get("version")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let is_bundle = v
        .get("dsh")
        .and_then(|d| d.get("bundle"))
        .and_then(|b| b.get("patch"))
        .is_some();
    let entry_ids = if is_bundle {
        v.get("dsh")
            .and_then(|d| d.get("bundle"))
            .and_then(|b| b.get("patch"))
            .and_then(|p| p.as_str())
            .and_then(|rel| {
                std::fs::read_to_string(profile.join("node_modules").join(name).join(rel)).ok()
            })
            .map(|c| extract_insert_ids(&c))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some((version, description, is_bundle, entry_ids))
}

/// 内置 bundle（不在 profile dependencies 中）的版本与描述：
/// 版本优先取 dsh 自身 manifest 的 dependencies 声明，描述从实际安装目录读取。
fn builtin_pkg_info(name: &str) -> (String, String) {
    let mut version = String::new();
    let mut description = String::new();
    if let Some(inst) = find_dsh() {
        let root = PathBuf::from(&inst.pkg_root);
        let dsh_pkg = root
            .join("node_modules")
            .join(DSH_PKG_NAME)
            .join("package.json");
        if let Ok(content) = std::fs::read_to_string(&dsh_pkg) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(spec) = v
                    .get("dependencies")
                    .and_then(|d| d.get(name))
                    .and_then(|x| x.as_str())
                {
                    version = spec
                        .trim_start_matches('^')
                        .trim_start_matches('~')
                        .to_string();
                }
            }
        }
        // 全局安装时 dsh 的依赖嵌套在 dsh/node_modules 下，也兼容 hoisted 布局
        for cand in [
            root.join("node_modules").join(name).join("package.json"),
            root.join("node_modules")
                .join(DSH_PKG_NAME)
                .join("node_modules")
                .join(name)
                .join("package.json"),
        ] {
            if let Ok(content) = std::fs::read_to_string(&cand) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    description = v
                        .get("description")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if version.is_empty() {
                        version = v
                            .get("version")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    break;
                }
            }
        }
    }
    (version, description)
}

fn run_plugin_list() -> PluginListResult {
    let profile = profile_dir();
    let content = match std::fs::read_to_string(profile.join("package.json")) {
        Ok(c) => c,
        Err(_) => {
            return PluginListResult {
                profile_dir: profile.to_string_lossy().into_owned(),
                initialized: false,
                plugins: Vec::new(),
            }
        }
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            return PluginListResult {
                profile_dir: profile.to_string_lossy().into_owned(),
                initialized: false,
                plugins: Vec::new(),
            }
        }
    };
    let deps: Vec<(String, String)> = manifest
        .get("dependencies")
        .and_then(|d| d.as_object())
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    let bundles: Vec<String> = manifest
        .get("dsh")
        .and_then(|d| d.get("profile"))
        .and_then(|p| p.get("bundles"))
        .and_then(|b| b.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut plugins = Vec::new();
    // profile 层禁用状态（cordis.patch.yml 中的 disabled 条目 id）
    let patch_content =
        std::fs::read_to_string(profile.join("cordis.patch.yml")).unwrap_or_default();
    let disabled_ids = read_disabled_ids(&patch_content);

    // 用户插件（profile dependencies），保持 manifest 声明顺序
    for (name, spec) in &deps {
        let (version, description, is_bundle, entry_ids) =
            installed_pkg_info(&profile, name).unwrap_or_default();
        let enabled = if bundles.iter().any(|b| b == name) {
            Some(true)
        } else if is_bundle {
            Some(false)
        } else {
            None
        };
        let patch_disabled = entry_ids.iter().any(|id| disabled_ids.contains(id));
        plugins.push(PluginInfo {
            name: name.clone(),
            spec: spec.clone(),
            version,
            description,
            is_bundle,
            is_builtin: false,
            enabled,
            entry_ids,
            patch_disabled,
        });
    }
    // 内置 bundle（在 bundles 中但不在 dependencies），只读展示
    for b in &bundles {
        if deps.iter().any(|(n, _)| n == b) {
            continue;
        }
        let (version, description) = builtin_pkg_info(b);
        plugins.push(PluginInfo {
            name: b.clone(),
            spec: String::new(),
            version,
            description,
            is_bundle: true,
            is_builtin: true,
            enabled: Some(true),
            entry_ids: Vec::new(),
            patch_disabled: false,
        });
    }
    PluginListResult {
        profile_dir: profile.to_string_lossy().into_owned(),
        initialized: true,
        plugins,
    }
}

/// 执行 `dsh plugin --profile <profile> <args...>`（官方通道，转发 pnpm），
/// stdout 流式推送进度；stderr 末尾在失败时回读。
pub(crate) fn run_profile_cmd(
    profile: &str,
    args: &[&str],
    proxy_on: bool,
    proxy_addr: &str,
    progress: &Channel<String>,
) -> Result<(), String> {
    let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
    let mut cmd = Command::new("node");
    cmd.arg(&bin)
        .arg("plugin")
        .arg("--profile")
        .arg(profile)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW);
    for (k, v) in npm_proxy_env(proxy_on, proxy_addr) {
        cmd.env(k, v);
    }
    apply_registry(&mut cmd);
    // 实际执行的是 node bin.js plugin ...（内部转发 pnpm），错误前缀
    // 必须指向 dsh plugin 而非 pnpm，否则误导排障方向
    run_cmd_streaming(&mut cmd, progress, "dsh plugin")
}

/// 插件管理命令：目标 profile 取设置里的 pluginProfile（默认 web）
fn run_plugin_cmd(
    args: &[&str],
    proxy_on: bool,
    proxy_addr: &str,
    progress: &Channel<String>,
) -> Result<(), String> {
    run_profile_cmd(
        &settings().plugin_profile,
        args,
        proxy_on,
        proxy_addr,
        progress,
    )
}

#[tauri::command]
pub(crate) async fn plugin_list() -> PluginListResult {
    tauri::async_runtime::spawn_blocking(run_plugin_list)
        .await
        .unwrap_or_else(|_| PluginListResult {
            profile_dir: String::new(),
            initialized: false,
            plugins: Vec::new(),
        })
}

#[tauri::command]
pub(crate) async fn plugin_install(
    name: String,
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    let name = name.trim().to_string();
    validate_plugin_name(&name)?;
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        // 已安装则给出明确提示（避免 pnpm add 静默重装）
        if let Ok(content) = std::fs::read_to_string(profile.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) {
                    let bare = bare_pkg_name(&name).to_string();
                    if deps.contains_key(&bare) {
                        return Err(format!("插件 {} 已安装，如需升级请用「更新」", bare));
                    }
                }
            }
        }
        run_plugin_cmd(&["add", &name], proxy_on, &proxy_addr, &progress)?;
        // 安装后读回实际版本（pnpm add 可能解析出具体版本）
        let bare = bare_pkg_name(&name).to_string();
        Ok(installed_pkg_info(&profile, &bare)
            .map(|(v, _, _, _)| v)
            .unwrap_or_default())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn plugin_remove(
    name: String,
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    let name = name.trim().to_string();
    validate_installed_name(&name)?;
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        let profile_name = settings().plugin_profile;
        let content = std::fs::read_to_string(profile.join("package.json"))
            .map_err(|_| format!("{} profile 尚未初始化", profile_name))?;
        let manifest: serde_json::Value = serde_json::from_str(&content)
            .map_err(|_| format!("{} profile 配置解析失败", profile_name))?;
        let deps = manifest
            .get("dependencies")
            .and_then(|d| d.as_object())
            .cloned()
            .unwrap_or_default();
        let bundles: Vec<String> = manifest
            .get("dsh")
            .and_then(|d| d.get("profile"))
            .and_then(|p| p.get("bundles"))
            .and_then(|b| b.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !deps.contains_key(&name) {
            if bundles.iter().any(|b| b == &name) {
                return Err(format!("{} 是 DSH 内置组件，不能卸载", name));
            }
            return Err(format!("插件 {} 不在已安装列表", name));
        }
        let entry_ids = installed_pkg_info(&profile, &name)
            .map(|(_, _, _, ids)| ids)
            .unwrap_or_default();
        run_plugin_cmd(&["remove", &name], proxy_on, &proxy_addr, &progress)?;
        if !entry_ids.is_empty() {
            let patch_path = profile.join("cordis.patch.yml");
            if let Err(e) = manage_disabled_blocks(&patch_path, &name, &entry_ids, true) {
                return Err(format!("插件 {} 已卸载，但清理禁用配置失败：{}", name, e));
            }
        }
        Ok("removed".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 是否是"新版本尚未同步完成"导致的失败。
///
/// 现象：检查更新能查到新版本（`npm view` 已经看得到），但一点更新就报错。
/// 成因有两类，都会命中下面这些特征串：
/// - pnpm 自己的 registry 元数据缓存还没刷新 → `ERR_PNPM_NO_MATCHING_VERSION` / `ETARGET`
/// - packument 已更新、tarball 还没传播到 registry 的 CDN 边缘 → `ERR_PNPM_FETCH_404` / `E404`
///
/// 检查更新用的是 npm、实际安装用的是 pnpm，两者各自维护缓存，所以"看得见装不上"
/// 是必然会出现的时间窗（本机实测：某插件发布后 17 秒就被查出来并点了更新）。
fn is_fresh_package_error(msg: &str) -> bool {
    const NEEDLES: [&str; 6] = [
        "ERR_PNPM_NO_MATCHING_VERSION",
        "No matching version found",
        "ERR_PNPM_FETCH_404",
        "ETARGET",
        "E404",
        "404 Not Found",
    ];
    NEEDLES.iter().any(|n| msg.contains(n))
}

/// 执行插件更新命令；遇到"新版本还没同步好"的错误时短暂等待后重试一次。
/// 重试仍失败则补一段可行动的说明（等几分钟，而不是反复点点点）。
fn run_plugin_update_with_retry(
    args: &[&str],
    proxy_on: bool,
    proxy_addr: &str,
    progress: &Channel<String>,
    label: &str,
) -> Result<(), String> {
    let first = run_plugin_cmd(args, proxy_on, proxy_addr, progress);
    let Err(e) = first else {
        return Ok(());
    };
    if !is_fresh_package_error(&e) {
        return Err(e);
    }
    let _ = progress.send(
        "检测到新版本尚未同步完成（registry 元数据/包文件传播中），3 秒后重试一次…".to_string(),
    );
    app_log_line(&format!(
        "{} 首次失败，疑似发布同步延迟，重试一次：{}",
        label,
        e.replace('\n', " ")
    ));
    std::thread::sleep(Duration::from_secs(3));
    match run_plugin_cmd(args, proxy_on, proxy_addr, progress) {
        Ok(()) => Ok(()),
        Err(e2) => Err(format!(
            "{}\n\n该版本可能刚发布不久：registry 元数据或包文件尚未在你本地与 CDN 同步完成。\
             建议等 5–30 分钟后重试；完整输出已写入 exe 旁 launcher.log",
            e2
        )),
    }
}

#[tauri::command]
pub(crate) async fn plugin_update(
    all: bool,
    name: Option<String>,
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    if !all {
        let candidate = name.as_deref().ok_or_else(|| "缺少插件名".to_string())?;
        validate_installed_name(candidate)?;
    }
    tauri::async_runtime::spawn_blocking(move || {
        if all {
            run_plugin_update_with_retry(
                &["update", "--latest"],
                proxy_on,
                &proxy_addr,
                &progress,
                "全部插件更新",
            )?;
            return Ok("all".to_string());
        }
        let name = name
            .ok_or_else(|| "缺少插件名".to_string())?
            .trim()
            .to_string();
        // 内置组件不是 profile 依赖，先给出明确提示
        let profile = profile_dir();
        if let Ok(content) = std::fs::read_to_string(profile.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let in_deps = v
                    .get("dependencies")
                    .and_then(|d| d.as_object())
                    .map(|d| d.contains_key(&name))
                    .unwrap_or(false);
                if !in_deps {
                    return Err(format!(
                        "{} 不在已安装插件列表（内置组件随 DSH 本体更新）",
                        name
                    ));
                }
            }
        }
        run_plugin_update_with_retry(
            &["update", "--latest", &name],
            proxy_on,
            &proxy_addr,
            &progress,
            &format!("插件 {} 更新", name),
        )?;
        Ok(name)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginUpdateInfo {
    name: String,
    installed: String,
    latest: String,        // registry 最新版本；空表示未检查（本地依赖）或检查失败
    error: Option<String>, // npm view 失败原因（网络 / 包不存在等）
    /// latest 的发布时间（RFC3339 UTC）；未知为 None
    published_at: Option<String>,
    /// latest 距今多少分钟（由 published_at 换算）；未知为 None。
    /// 界面据此对"刚发布"的版本降级提示：从发布到可稳定安装之间有传播延迟。
    age_minutes: Option<i64>,
}

impl PluginUpdateInfo {
    /// 无需查询的条目（本地依赖 / 已是最新 / 名字非法）
    fn bare(name: &str, installed: &str) -> Self {
        PluginUpdateInfo {
            name: name.to_string(),
            installed: installed.to_string(),
            latest: String::new(),
            error: None,
            published_at: None,
            age_minutes: None,
        }
    }

    fn with_error(name: &str, installed: &str, err: impl Into<String>) -> Self {
        PluginUpdateInfo {
            error: Some(err.into()),
            ..Self::bare(name, installed)
        }
    }
}

/// 该 spec 是否走 npm registry（file: / link: / git: / 路径等本地或 VCS
/// 依赖无法用 npm view 查版本，跳过检查）。
fn is_registry_spec(spec: &str) -> bool {
    let s = spec.trim();
    !(s.starts_with("file:")
        || s.starts_with("link:")
        || s.starts_with("git")
        || s.starts_with("github:")
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with('.')
        || s.contains('\\'))
}

/// 检查所有用户插件的 registry 最新版本（并行执行 npm view，各自 30 秒超时）。
/// 每个插件独立失败，互不影响。
#[tauri::command]
pub(crate) async fn plugin_check_updates(
    proxy_on: bool,
    proxy_addr: String,
) -> Result<Vec<PluginUpdateInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        let profile_name = settings().plugin_profile;
        let content = match std::fs::read_to_string(profile.join("package.json")) {
            Ok(c) => c,
            Err(_) => return Err(format!("{} profile 尚未初始化", profile_name)),
        };
        let manifest: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return Err(format!("{} profile 配置解析失败", profile_name)),
        };
        let deps: Vec<(String, String)> = manifest
            .get("dependencies")
            .and_then(|d| d.as_object())
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let env = npm_proxy_env(proxy_on, &proxy_addr);
        // 先按依赖顺序整理：无需查询的直接出结果，需要 npm view 的进入待查队列
        let mut slots: Vec<Option<PluginUpdateInfo>> = Vec::with_capacity(deps.len());
        let mut pending: Vec<(usize, String, String)> = Vec::new(); // (slot, name, installed)
        for (name, spec) in &deps {
            if validate_installed_name(name).is_err() {
                slots.push(Some(PluginUpdateInfo::with_error(
                    name,
                    "",
                    "清单中的插件名无效，已跳过检查",
                )));
                continue;
            }
            let installed = installed_pkg_info(&profile, name)
                .map(|(v, _, _, _)| v)
                .unwrap_or_default();
            if installed.is_empty() {
                slots.push(Some(PluginUpdateInfo::with_error(
                    name,
                    "",
                    "未找到已安装的 package.json",
                )));
            } else if !is_registry_spec(spec) {
                slots.push(Some(PluginUpdateInfo::bare(name, &installed)));
            } else {
                slots.push(None);
                pending.push((slots.len() - 1, name.clone(), installed));
            }
        }
        // 并发上限：npm view 是网络子进程，插件很多时避免瞬时拉起大量进程
        const NPM_VIEW_CONCURRENCY: usize = 4;
        std::thread::scope(|s| {
            for chunk in pending.chunks(NPM_VIEW_CONCURRENCY) {
                let mut handles = Vec::new();
                for &(slot, ref name, ref installed) in chunk {
                    let env = env.clone();
                    let name = name.clone();
                    let installed = installed.clone();
                    handles.push(s.spawn(move || {
                        let info = match npm_view_version(&name, &env) {
                            Ok(latest) => {
                                // 只在"确实存在可更新版本"时才补查一次发布时间，
                                // 让常规的"已是最新"检查保持零额外开销
                                let (published_at, age_minutes) =
                                    if crate::util::cmp_ver(&latest, &installed)
                                        == std::cmp::Ordering::Greater
                                    {
                                        match npm_view_publish_time(&name, &latest, &env) {
                                            Some(t) => {
                                                let age = crate::util::parse_rfc3339_utc_secs(&t)
                                                    .map(|secs| {
                                                        (crate::util::now_epoch_secs() - secs) / 60
                                                    });
                                                (Some(t), age)
                                            }
                                            None => (None, None),
                                        }
                                    } else {
                                        (None, None)
                                    };
                                PluginUpdateInfo {
                                    name,
                                    installed,
                                    latest,
                                    error: None,
                                    published_at,
                                    age_minutes,
                                }
                            }
                            Err(e) => PluginUpdateInfo::with_error(&name, &installed, e),
                        };
                        (slot, info)
                    }));
                }
                for h in handles {
                    let (slot, info) = h.join().unwrap_or_else(|_| {
                        (
                            usize::MAX,
                            PluginUpdateInfo::with_error("", "", "检查线程异常"),
                        )
                    });
                    if slot != usize::MAX {
                        slots[slot] = Some(info);
                    }
                }
            }
        });
        let results: Vec<PluginUpdateInfo> = slots
            .into_iter()
            .map(|s| s.unwrap_or_else(|| PluginUpdateInfo::with_error("", "", "检查线程异常")))
            .collect();
        Ok(results)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 在 profile 的 cordis.patch.yml 中追加/移除本启动器管理的禁用块
/// （`- id: <entryId>` + `disabled: true`，带标记注释便于识别与移除）。
/// enable 只移除本启动器写入的块，并保留用户手动配置的 disabled 条目；
/// disable 时跳过已有禁用条目的 id。返回是否发生变更。
fn manage_disabled_blocks(
    path: &Path,
    name: &str,
    ids: &[String],
    enable: bool,
) -> Result<bool, String> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let marker = format!("# --- 由 DSH 启动器管理：禁用 {} ---", name);
    let mut changed = false;

    // 1) 只移除本启动器写入的完整三行块。不要按 "- id:" 扫描后续内容，
    //    否则会吞掉用户条目或留下孤立的 YAML 字段。
    let mut kept: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == marker {
            changed = true;
            if i + 2 < lines.len()
                && entry_id_of(lines[i + 1].trim()).is_some()
                && disabled_flag(lines[i + 2].trim())
            {
                i += 3;
            } else {
                // A stale marker is still ours, but preserve all following user text.
                i += 1;
            }
            continue;
        }
        kept.push(lines[i].clone());
        i += 1;
    }
    lines = kept;

    // 2) disable：为尚未有 disabled 条目的 id 追加管理块。
    //    注意：原文件可能是 `[]`（内联空数组）——在其后追加条目会生成非法
    //    YAML（"end of the stream or a document separator is expected"），
    //    导致整个 profile 解析失败、DSH 无法启动。此时必须整文件重写。
    if !enable {
        let meaningful: Vec<&str> = lines
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let empty_file =
            meaningful.is_empty() || meaningful.iter().all(|l| *l == "[]" || *l == "---");
        if empty_file {
            // 保留注释行、去掉空行与 []，直接接管理块（重写为合法条目列表）
            lines.retain(|l| l.trim().starts_with('#'));
            while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.pop();
            }
        }
        let mut to_add: Vec<String> = Vec::new();
        'outer: for id in ids {
            if read_disabled_ids(&lines.join("\n")).iter().any(|x| x == id) {
                continue 'outer; // 已有禁用条目，不重复添加
            }
            to_add.push(id.clone());
        }
        if !to_add.is_empty() {
            if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            for id in to_add {
                lines.push(marker.clone());
                lines.push(format!("- id: {}", id));
                lines.push("  disabled: true".into());
            }
            changed = true;
        }
    }

    if changed {
        // 启用后若文件只剩注释（无任何条目），恢复 `[]` 占位保证 YAML 合法
        if enable {
            let meaningful: Vec<&str> = lines
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();
            if meaningful.is_empty() {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push("[]".into());
            }
        }
        let mut text = lines.join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        write_atomic(path, &text, "cordis-patch")
            .map_err(|e| format!("写入 cordis.patch.yml 失败：{}", e))?;
    }
    Ok(changed)
}

/// 启用/禁用插件：通过 profile 的 cordis.patch.yml 的 disabled 条目实现
/// （保留安装，仅停用；下次启动 DSH 生效）。
#[tauri::command]
pub(crate) async fn plugin_toggle(name: String, enable: bool) -> Result<String, String> {
    let name = name.trim().to_string();
    tauri::async_runtime::spawn_blocking(move || {
        validate_installed_name(&name)?;
        let profile = profile_dir();
        let (_, _, is_bundle, entry_ids) =
            installed_pkg_info(&profile, &name).ok_or_else(|| format!("插件 {} 未安装", name))?;
        if !is_bundle {
            return Err(format!(
                "{} 不是 profile 层插件（普通依赖库），不支持启用/禁用",
                name
            ));
        }
        if entry_ids.is_empty() {
            return Err(format!(
                "{} 的 bundle 未声明可管理的入口（insert id），无法启用/禁用",
                name
            ));
        }
        let patch_path = profile.join("cordis.patch.yml");
        let changed = manage_disabled_blocks(&patch_path, &name, &entry_ids, enable)?;
        if !changed && !enable {
            return Err(format!("{} 已处于禁用状态", name));
        }
        if !changed && enable {
            let patch_content = std::fs::read_to_string(&patch_path).unwrap_or_default();
            if entry_ids
                .iter()
                .any(|id| read_disabled_ids(&patch_content).iter().any(|x| x == id))
            {
                return Err(format!(
                    "{} 的禁用项来自用户配置，未自动修改；请手工移除对应 disabled 条目",
                    name
                ));
            }
            return Err(format!("{} 未处于禁用状态", name));
        }
        Ok(if enable { "enabled" } else { "disabled" }.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginDetail {
    name: String,
    version: String,
    spec: String,
    description: String,
    license: String,
    homepage: String,
    repository: String,
    is_bundle: bool,
    entry_ids: Vec<String>,
    readme: String,
}

/// 插件详情：元信息 + README 摘要（供展开面板展示）
#[tauri::command]
pub(crate) async fn plugin_detail(name: String) -> Result<PluginDetail, String> {
    let name = name.trim().to_string();
    tauri::async_runtime::spawn_blocking(move || {
        validate_installed_name(&name)?;
        let profile = profile_dir();
        let pkg_path = profile
            .join("node_modules")
            .join(&name)
            .join("package.json");
        let content =
            std::fs::read_to_string(&pkg_path).map_err(|_| format!("插件 {} 未安装", name))?;
        let v: serde_json::Value =
            serde_json::from_str(&content).map_err(|_| "插件配置解析失败".to_string())?;
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        // repository 可能是对象 {type, url}
        let repository = v
            .get("repository")
            .and_then(|r| {
                r.as_str()
                    .map(String::from)
                    .or_else(|| r.get("url").and_then(|u| u.as_str()).map(String::from))
            })
            .unwrap_or_default();
        // spec：profile manifest 中声明的依赖范围
        let spec = std::fs::read_to_string(profile.join("package.json"))
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|m| {
                m.get("dependencies")
                    .and_then(|d| d.get(&name))
                    .and_then(|x| x.as_str())
                    .map(String::from)
            })
            .unwrap_or_default();
        let (_, _, is_bundle, entry_ids) = installed_pkg_info(&profile, &name).unwrap_or_default();
        // README 摘要（前 2000 字符，前端会清洗 markdown 后展示简介）
        let mut readme = String::new();
        let base = profile.join("node_modules").join(&name);
        for f in ["README.md", "readme.md", "README.MD", "README"] {
            if let Ok(c) = std::fs::read_to_string(base.join(f)) {
                readme = c.chars().take(2000).collect();
                if c.chars().count() > 2000 {
                    readme.push('…');
                }
                break;
            }
        }
        Ok(PluginDetail {
            name: name.clone(),
            version: get("version"),
            spec,
            description: get("description"),
            license: get("license"),
            homepage: get("homepage"),
            repository,
            is_bundle,
            entry_ids,
            readme,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_extraction() {
        assert_eq!(bare_pkg_name("dsh-better-sidebar"), "dsh-better-sidebar");
        assert_eq!(
            bare_pkg_name("dsh-better-sidebar@0.12.2"),
            "dsh-better-sidebar"
        );
        assert_eq!(
            bare_pkg_name("@zerro223/dsh-token-usage"),
            "@zerro223/dsh-token-usage"
        );
        assert_eq!(
            bare_pkg_name("@zerro223/dsh-token-usage@1.0.0"),
            "@zerro223/dsh-token-usage"
        );
        // 含斜杠的 spec（git / file: 等）保持原样，不做版本剥离
        assert_eq!(bare_pkg_name("file:../plugin"), "file:../plugin");
        assert_eq!(
            bare_pkg_name("git+ssh://git@github.com/x/y.git"),
            "git+ssh://git@github.com/x/y.git"
        );
    }

    #[test]
    fn name_validation() {
        assert!(validate_plugin_name("dsh-better-sidebar").is_ok());
        assert!(validate_plugin_name("@scope/name@1.2.3").is_ok());
        assert!(validate_plugin_name("").is_err());
        assert!(validate_plugin_name("   ").is_err());
        assert!(validate_plugin_name("-x").is_err());
        assert!(validate_plugin_name("a b").is_err());
    }

    #[test]
    fn registry_spec_detection() {
        // registry 版本范围：可查 npm view
        assert!(is_registry_spec("^0.12.2"));
        assert!(is_registry_spec("0.1.0-rc.6"));
        assert!(is_registry_spec("~1.0.0"));
        // 本地 / VCS / URL 依赖：跳过检查
        assert!(!is_registry_spec("file:E:/work/dsh-token-usage"));
        assert!(!is_registry_spec("file:../plugin.tgz"));
        assert!(!is_registry_spec("link:../plugin"));
        assert!(!is_registry_spec("git+https://github.com/x/y.git"));
        assert!(!is_registry_spec("github:user/repo"));
        assert!(!is_registry_spec("https://example.com/pkg.tgz"));
    }

    #[test]
    fn insert_ids_include_multiple_top_level_blocks() {
        let content = "- insert:\n    - id: first\n- insert:\n    - id: second\n";
        assert_eq!(
            extract_insert_ids(content),
            vec!["first".to_string(), "second".to_string()]
        );
    }

    #[test]
    fn disabled_parser_handles_fields_comments_and_overrides() {
        let content = "- id: first\n  name: plugin\n  disabled: true # user reason\n- id: second\n  disabled: true\n- id: second\n  disabled: false\n";
        assert_eq!(read_disabled_ids(content), vec!["first".to_string()]);
    }

    /// 验证 cordis.patch.yml 的禁用/启用写入始终生成合法 YAML 结构
    /// （回归：曾在 `[]` 空数组后追加条目导致 DSH 解析失败、Web UI 无法启动）
    #[test]
    fn disabled_blocks_always_valid() {
        let dir = std::env::temp_dir().join(format!("dsh-launcher-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cordis.patch.yml");
        let ids = vec!["ui-skin-maid-atelier".to_string()];

        // 场景 1：空数组文件（[]）→ 禁用 → 必须整文件重写为条目列表
        std::fs::write(&path, "# 注释\n[]\n").unwrap();
        manage_disabled_blocks(&path, "maid-atelier", &ids, false).unwrap();
        let c1 = std::fs::read_to_string(&path).unwrap();
        assert!(
            c1.contains("- id: ui-skin-maid-atelier"),
            "禁用后应有条目: {}",
            c1
        );
        assert!(!c1.contains("[]"), "禁用后不应残留空数组: {}", c1);

        // 场景 2：启用 → 删除条目后恢复 [] 占位（保证 YAML 合法）
        manage_disabled_blocks(&path, "maid-atelier", &ids, true).unwrap();
        let c2 = std::fs::read_to_string(&path).unwrap();
        let stripped2: String = c2
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(stripped2, "[]", "启用后应恢复空数组: {}", c2);

        // 场景 3：已有条目列表的文件 → 追加新条目（保持合法）
        std::fs::write(&path, "- id: other\n  disabled: true\n").unwrap();
        manage_disabled_blocks(&path, "maid-atelier", &ids, false).unwrap();
        let c3 = std::fs::read_to_string(&path).unwrap();
        assert!(c3.contains("- id: ui-skin-maid-atelier"));
        assert!(c3.contains("- id: other"));

        // 场景 4：追加模式下禁用后再启用，保留其他条目且无重复
        manage_disabled_blocks(&path, "maid-atelier", &ids, true).unwrap();
        let c4 = std::fs::read_to_string(&path).unwrap();
        assert!(!c4.contains("ui-skin-maid-atelier"));
        assert!(c4.contains("- id: other"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enabling_preserves_user_fields_and_manual_disabled_entries() {
        let dir =
            std::env::temp_dir().join(format!("dsh-launcher-plugin-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cordis.patch.yml");
        std::fs::write(
            &path,
            "# --- 由 DSH 启动器管理：禁用 demo ---\n- id: managed\n  disabled: true\n- id: manual\n  disabled: true # keep me\n  config:\n    value: 1\n",
        )
        .unwrap();
        let ids = vec!["managed".to_string(), "manual".to_string()];
        assert!(manage_disabled_blocks(&path, "demo", &ids, true).unwrap());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("managed"));
        assert!(content.contains("manual"));
        assert!(content.contains("value: 1"));
        assert!(content.contains("disabled: true"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// YAML 轻量解析的格式容忍：多余空格、引号、布尔变体（True/yes）
    #[test]
    fn yaml_parsing_tolerance() {
        // entry_id_of：冒号后多空格、引号包裹
        assert_eq!(
            entry_id_of("- id:   spaced-id").as_deref(),
            Some("spaced-id")
        );
        assert_eq!(entry_id_of("- id: 'quoted'").as_deref(), Some("quoted"));
        assert_eq!(entry_id_of("- id: \"dq\"").as_deref(), Some("dq"));
        assert_eq!(entry_id_of("- id:").as_deref(), None);
        assert_eq!(entry_id_of("- name: x").as_deref(), None);
        // disabled_flag：布尔变体
        assert!(disabled_flag("disabled: true"));
        assert!(disabled_flag("disabled:   True"));
        assert!(disabled_flag("disabled: yes"));
        assert!(disabled_flag("disabled: 'ON'"));
        assert!(!disabled_flag("disabled: false"));
        assert!(!disabled_flag("disabled:"));
        assert!(!disabled_flag("id: x"));
        // read_disabled_ids：变体组合整体识别
        let content = "- id: a\n  disabled: true\n- id: b\n  disabled: True\n- id: c\n  disabled: false\n- id: d\n";
        let ids = read_disabled_ids(content);
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    /// 已安装插件名的路径安全校验（防穿越读）
    #[test]
    fn installed_name_validation() {
        assert!(validate_installed_name("dsh-better-sidebar").is_ok());
        assert!(validate_installed_name("@scope/name").is_ok());
        assert!(validate_installed_name("../../etc").is_err());
        assert!(validate_installed_name("a\\b").is_err());
        assert!(validate_installed_name("C:/x").is_err());
        assert!(validate_installed_name("a b").is_err());
    }
}
