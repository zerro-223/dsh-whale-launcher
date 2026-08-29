//! 启动前自检：Node 环境 / DSH 程序文件 / npm 缓存 / pnpm / 系统代理 /
//! settings.json 加载状态 / DSH 运行状态。

use serde::Serialize;

use crate::locate::{find_dsh, npm_config_cache, web_url};
use crate::npm::{clear_which_cache, which};
use crate::process::{dsh_state, DshState};
use crate::proxy::get_system_proxy;
use crate::settings::{settings, settings_load_error};

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CheckItem {
    name: String,
    status: String, // OK / FAIL / WARN / INFO
    detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChecksResult {
    items: Vec<CheckItem>,
    running: bool,
}

fn run_checks() -> ChecksResult {
    let mut items = Vec::new();

    if which("node") {
        items.push(CheckItem {
            name: "Node.js 环境".into(),
            status: "OK".into(),
            detail: "已找到 node".into(),
        });
    } else {
        items.push(CheckItem {
            name: "Node.js 环境".into(),
            status: "FAIL".into(),
            detail: "未找到 node，请先安装 Node.js".into(),
        });
    }

    match find_dsh() {
        Some(inst) => items.push(CheckItem {
            name: "DSH 程序文件".into(),
            status: "OK".into(),
            detail: format!("已识别安装位置：{}", inst.pkg_root),
        }),
        None => items.push(CheckItem {
            name: "DSH 程序文件".into(),
            status: "FAIL".into(),
            detail: "未找到 DSH，请点击右上角「未安装 DSH」一键安装，或手动执行：npm install -g @deepseek-ai/dsh".into(),
        }),
    }

    // npm 缓存仅作信息展示，不再强制要求指向特定盘符
    match npm_config_cache() {
        Some(cache) => items.push(CheckItem {
            name: "npm 缓存".into(),
            status: "INFO".into(),
            detail: format!("位于 {}", cache),
        }),
        None => items.push(CheckItem {
            name: "npm 缓存".into(),
            status: "WARN".into(),
            detail: "无法读取 npm 缓存路径".into(),
        }),
    }

    // pnpm：插件安装/更新依赖（dsh plugin 转发 pnpm），缺失不影响 DSH 本体
    if which("pnpm") || which("pnpm.cmd") {
        items.push(CheckItem {
            name: "pnpm 环境".into(),
            status: "OK".into(),
            detail: "已找到 pnpm，插件管理可用".into(),
        });
    } else {
        items.push(CheckItem {
            name: "pnpm 环境".into(),
            status: "WARN".into(),
            detail: "未找到 pnpm，插件安装/更新不可用（DSH 本体功能不受影响）；请执行 npm install -g pnpm".into(),
        });
    }

    let (enabled, server) = get_system_proxy();
    if enabled && !server.is_empty() {
        items.push(CheckItem {
            name: "系统代理".into(),
            status: "INFO".into(),
            detail: format!("已启用：{}", server),
        });
    } else {
        items.push(CheckItem {
            name: "系统代理".into(),
            status: "INFO".into(),
            detail: "系统代理未启用".into(),
        });
    }

    // settings.json 存在但解析失败时提示（load_settings 静默回退默认值的
    // 补偿：用户配置的 webPort 等悄悄失效必须可见）
    if let Some(err) = settings_load_error() {
        items.push(CheckItem {
            name: "settings.json".into(),
            status: "WARN".into(),
            detail: format!("{}（已临时使用默认配置，修复保存后重启启动器生效）", err),
        });
    }

    let state = dsh_state();
    items.push(CheckItem {
        name: "运行状态".into(),
        status: "INFO".into(),
        detail: match state {
            DshState::Running => format!("DSH 正在运行：{}", web_url()),
            DshState::ForeignPort => format!(
                "端口 {} 被其他程序占用（非 DSH 进程）；可修改 webPort 或停止占用程序",
                settings().web_port
            ),
            DshState::Stopped => "DSH 未运行，可点击上方按钮启动".into(),
        },
    });

    ChecksResult {
        items,
        running: state == DshState::Running,
    }
}

#[tauri::command]
pub(crate) async fn checks() -> ChecksResult {
    tauri::async_runtime::spawn_blocking(|| {
        crate::locate::clear_dsh_cache(); // 自检前刷新识别缓存（应对运行期间的外部安装/卸载）
        clear_which_cache(); // 刷新 PATH 探测缓存（应对运行期间新装的 node/npm）
        run_checks()
    })
    .await
    .unwrap_or(ChecksResult {
        items: Vec::new(),
        running: false,
    })
}

/// 根据自检结果给出可复制的修复命令
#[tauri::command]
pub(crate) async fn fix_commands() -> Vec<String> {
    // 缓存未命中时会 spawn where/npm 子进程，必须放后台线程（同步命令在主线程执行会卡 UI）
    tauri::async_runtime::spawn_blocking(|| {
        let mut cmds = Vec::new();
        if !which("node") {
            cmds.push("请先安装 Node.js（官网下载 LTS 版）：https://nodejs.org/".to_string());
        }
        if find_dsh().is_none() {
            cmds.push("npm install -g @deepseek-ai/dsh".to_string());
        }
        cmds
    })
    .await
    .unwrap_or_default()
}
