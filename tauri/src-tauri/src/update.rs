//! DSH 本体的版本检测、更新检查、更新与一键安装（npm 通道）。

use serde::Serialize;
use tauri::ipc::Channel;

use crate::locate::{clear_dsh_cache, find_dsh, get_installed_dsh_version, DSH_PKG_NAME};
use crate::npm::{npm_command, npm_view_version, run_npm_cmd};
use crate::proxy::npm_proxy_env;

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

#[tauri::command]
pub(crate) async fn update_dsh(
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<(String, String), String> {
    let install =
        find_dsh().ok_or_else(|| "未安装 DSH，请先点击「未安装 DSH」一键安装".to_string())?;
    let old = get_installed_dsh_version().unwrap_or_default();
    let is_global = install.is_global;
    let pkg_root = install.pkg_root.clone();
    let env = npm_proxy_env(proxy_on, &proxy_addr); // npm 不读 HTTP_PROXY，需 npm_config_*
    tauri::async_runtime::spawn_blocking(move || {
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
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn install_dsh(
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    let env = npm_proxy_env(proxy_on, &proxy_addr); // npm 不读 HTTP_PROXY，需 npm_config_*
    tauri::async_runtime::spawn_blocking(move || {
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
    .map_err(|e| e.to_string())?
}
