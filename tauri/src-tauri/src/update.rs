//! DSH 本体的版本检测、更新检查、更新与一键安装（npm 通道）。
//!
//! 更新/安装前必须先过 [`update_preflight`]：`npm install -g` 要写入 npm 全局
//! 前缀，而该目录在相当多的机器上只给普通用户读+执行权限（本机实测
//! `D:\Nodejs\node_global` → `BUILTIN\Users:(RX)`），于是更新必然 EPERM。
//! 与其失败后把 npm 的报错丢给用户，不如提前判定并给出可执行的出路。

use std::path::Path;

use serde::Serialize;
use tauri::ipc::Channel;

use crate::locate::{
    clear_dsh_cache, find_dsh, get_installed_dsh_version, npm_global_prefix, DshInstall,
    DSH_PKG_NAME,
};
use crate::npm::{npm_command, npm_view_version, run_npm_cmd};
use crate::process::{dsh_state, DshState};
use crate::proxy::npm_proxy_env;
use crate::util::{app_log_line, is_dir_writable, is_elevated, relaunch_elevated};

/// 更新检查结果：无法连接 registry 与「已是最新」必须区分开，
/// 错误信息携带 npm 真实报错（此前为匿名三元组，前端靠位置解构）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheckResult {
    installed: Option<String>,
    latest: Option<String>,
    error: Option<String>,
}

/// 查询 DSH 本体最新版本（内部复用 npm_view_version）
fn get_latest_dsh_version(env: &[(String, String)]) -> Result<String, String> {
    npm_view_version(DSH_PKG_NAME, env)
}

#[tauri::command]
pub(crate) async fn update_check(proxy_on: bool, proxy_addr: String) -> UpdateCheckResult {
    // npm view 需要访问网络 registry（可能 2~5 秒），必须在后台线程执行。
    // 按前端配置注入代理（npm 需 npm_config_* 变量），失败时返回 npm 真实报错。
    tauri::async_runtime::spawn_blocking(move || {
        clear_dsh_cache(); // 检查前刷新识别缓存（应对运行期间的外部安装/卸载）
        let installed = get_installed_dsh_version();
        let env = npm_proxy_env(proxy_on, &proxy_addr);
        match get_latest_dsh_version(&env) {
            Ok(latest) => UpdateCheckResult {
                installed,
                latest: Some(latest),
                error: None,
            },
            Err(e) => UpdateCheckResult {
                installed,
                latest: None,
                error: Some(e),
            },
        }
    })
    .await
    .unwrap_or(UpdateCheckResult {
        installed: None,
        latest: None,
        error: Some("更新检查异常".to_string()),
    })
}

// ---------------------------------------------------------------------------
// 更新 / 安装的前置检查与提权
// ---------------------------------------------------------------------------

/// 更新（或全局安装）DSH 的前置状态。前端据此分流：
/// 直接更新 / 先停 DSH / 先提权重启启动器。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdatePreflight {
    installed: Option<String>,
    /// 需要写入的目录（不可写时用它告诉用户到底是哪个目录）
    target_dir: String,
    is_global: bool,
    writable: bool,
    is_elevated: bool,
    /// 必须提权才能完成（目标不可写且当前未提权）
    needs_admin: bool,
    dsh_running: bool,
}

/// 判定更新时要写入的目录及其可写性。
/// `bin = …\@deepseek-ai\dsh\lib\bin.js`，上两级即包目录；全局安装还要写
/// npm 前缀（bin shim 与 node_modules 树），所以两处都要可写。
/// 自检面板（checks.rs）也用它，因此是 pub(crate)。
pub(crate) fn update_target(install: Option<&DshInstall>) -> (String, bool) {
    match install {
        Some(i) => {
            let pkg_dir = Path::new(&i.bin)
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf());
            let mut ok = is_dir_writable(Path::new(&i.pkg_root));
            if let Some(d) = &pkg_dir {
                ok = ok && is_dir_writable(d);
            }
            (i.pkg_root.clone(), ok)
        }
        // 未安装：按"将要全局安装"判定写入目标
        None => match npm_global_prefix() {
            Some(p) => {
                let ok = is_dir_writable(Path::new(&p));
                (p, ok)
            }
            None => (String::new(), false),
        },
    }
}

impl UpdatePreflight {
    fn collect() -> Self {
        let install = find_dsh();
        let (target_dir, writable) = update_target(install.as_ref());
        let elevated = is_elevated();
        UpdatePreflight {
            installed: get_installed_dsh_version(),
            is_global: install.as_ref().map(|i| i.is_global).unwrap_or(true),
            target_dir,
            writable,
            is_elevated: elevated,
            needs_admin: !writable && !elevated,
            dsh_running: matches!(dsh_state(), DshState::Running),
        }
    }
}

/// 更新/安装前的前置检查（前端在用户确认后、动手前调用）
#[tauri::command]
pub(crate) async fn update_preflight() -> UpdatePreflight {
    // 写权限探测要落一次临时文件、dsh_state 未命中缓存时会起子进程，仍放后台
    tauri::async_runtime::spawn_blocking(UpdatePreflight::collect)
        .await
        .unwrap_or(UpdatePreflight {
            installed: None,
            target_dir: String::new(),
            is_global: true,
            writable: false,
            is_elevated: false,
            needs_admin: false,
            dsh_running: false,
        })
}

/// 以管理员身份重新启动启动器（用户确认后调用）。
/// 提权实例带 `--elevated` 参数以豁免单实例互斥；稍候再退出本实例，
/// 避免用户看到"窗口凭空消失"而无新窗口出现。
#[tauri::command]
pub(crate) async fn relaunch_as_admin(app: tauri::AppHandle) -> Result<String, String> {
    relaunch_elevated(&["--elevated"])?;
    app_log_line("已请求以管理员身份重启启动器（等待 UAC 授权）");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(900));
        app.exit(0);
    });
    Ok("elevated".into())
}

/// 目标目录不可写时给出可直接照做的三条出路。
/// `*S-1-5-32-545` 即 BUILTIN\Users 的 SID——用 SID 而非名称，避免中文系统上
/// `icacls /grant Users:...` 因本地化名称不匹配而失败。
fn ensure_writable_for_install(target_dir: &str) -> Result<(), String> {
    if target_dir.is_empty() {
        return Ok(()); // 识别不到安装位置时不拦，交给 npm 自己报错
    }
    if is_dir_writable(Path::new(target_dir)) {
        return Ok(());
    }
    Err(format!(
        "目标目录不可写，更新无法完成：\n{}\n\n\
         该目录对当前用户只开放「读取+执行」权限，npm install -g 会被系统拒绝。\n\
         任选一种方式解决：\n\
         1. 回到设置页点「以管理员身份重启启动器」，再重新更新（推荐）\n\
         2. 改为装到用户目录：npm config set prefix \"%APPDATA%\\npm\" 后重新安装\n\
         3. 以管理员身份执行一次授权：\n   \
         icacls \"{}\" /grant *S-1-5-32-545:(OI)(CI)M",
        target_dir, target_dir
    ))
}

/// 更新/安装前必须停止 DSH：Windows 上运行中的 node 会占用待替换的文件，
/// 覆盖会失败（表现为"更新失败"，但根因与权限无关）。
fn ensure_dsh_stopped_for_install() -> Result<(), String> {
    if matches!(dsh_state(), DshState::Running) {
        return Err(
            "更新前需要先停止 DSH：Windows 上运行中的 node 会占用待替换的文件，\
                    直接覆盖会失败。请先在首页点「关闭 Web 界面」，或选择「停止并更新」"
                .into(),
        );
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn update_dsh(
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<(String, String), String> {
    let install =
        find_dsh().ok_or_else(|| "未安装 DSH，请先点击「未安装 DSH」一键安装".to_string())?;
    ensure_dsh_stopped_for_install()?;
    let (target, _) = update_target(Some(&install));
    ensure_writable_for_install(&target)?;
    let old = get_installed_dsh_version().unwrap_or_default();
    let is_global = install.is_global;
    let pkg_root = install.pkg_root.clone();
    let env = npm_proxy_env(proxy_on, &proxy_addr); // npm 不读 HTTP_PROXY，需 npm_config_*
    app_log_line(&format!(
        "开始更新 DSH：当前 v{}，目标目录 {}（全局={}）",
        old, pkg_root, is_global
    ));
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = npm_command();
        let pkg = format!("{}@latest", DSH_PKG_NAME);
        if is_global {
            // 全局安装：npm install -g
            cmd.args(["install", "-g", &pkg, "--no-audit", "--no-fund"]);
        } else {
            // npx 缓存 / 本地安装：在安装根目录执行 npm install
            cmd.args(["install", &pkg, "--no-audit", "--no-fund"])
                .current_dir(&pkg_root);
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        run_npm_cmd(&mut cmd, &progress)?;
        clear_dsh_cache();
        let new = get_installed_dsh_version().unwrap_or_default();
        if new.is_empty() {
            return Err("npm 已执行完成，但更新后未能识别到 DSH 版本号".to_string());
        }
        Ok::<(String, String), String>((old, new))
    })
    .await
    .map_err(|e| e.to_string())?;
    match &result {
        Ok((o, n)) => app_log_line(&format!("DSH 更新完成：v{} → v{}", o, n)),
        Err(e) => app_log_line(&format!("DSH 更新失败：{}", e.replace('\n', " "))),
    }
    result
}

#[tauri::command]
pub(crate) async fn install_dsh(
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    ensure_dsh_stopped_for_install()?;
    let (target, _) = update_target(find_dsh().as_ref());
    ensure_writable_for_install(&target)?;
    let env = npm_proxy_env(proxy_on, &proxy_addr); // npm 不读 HTTP_PROXY，需 npm_config_*
    app_log_line(&format!("开始全局安装 DSH，目标目录 {}", target));
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = npm_command();
        cmd.args([
            "install",
            "-g",
            &format!("{}@latest", DSH_PKG_NAME),
            "--no-audit",
            "--no-fund",
        ]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        run_npm_cmd(&mut cmd, &progress)?;
        clear_dsh_cache();
        // 安装后立即重新识别并返回版本号
        get_installed_dsh_version().ok_or_else(|| {
            "npm 安装已执行成功，但未识别到 DSH 程序文件，请检查 npm 输出".to_string()
        })
    })
    .await
    .map_err(|e| e.to_string())?;
    match &result {
        Ok(v) => app_log_line(&format!("DSH 安装完成：v{}", v)),
        Err(e) => app_log_line(&format!("DSH 安装失败：{}", e.replace('\n', " "))),
    }
    result
}
