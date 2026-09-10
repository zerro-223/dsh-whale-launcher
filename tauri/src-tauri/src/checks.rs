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

    // 更新/安装 DSH 的写入权限与当前权限级别。
    // 这是"更新失败"最常见的原因（npm 全局目录对普通用户只读），必须在自检里
    // 可见——否则用户只能拿到 npm 的 EPERM，完全不知道该做什么。
    let (target, writable) = crate::update::update_target(find_dsh().as_ref());
    let elevated = crate::util::is_elevated();
    items.push(CheckItem {
        name: "运行权限".into(),
        status: "INFO".into(),
        detail: if elevated {
            "以管理员身份运行（受保护目录可写）".into()
        } else {
            "普通用户运行；若下面的「更新权限」为 WARN，可用设置页的「以管理员身份重启启动器」解决"
                .into()
        },
    });
    items.push(CheckItem {
        name: "更新权限".into(),
        status: if writable { "OK" } else { "WARN" }.into(),
        detail: if target.is_empty() {
            "未能识别 DSH 安装目录（尚未安装？）".into()
        } else if writable {
            format!("{} 可写，DSH 可直接更新/安装", target)
        } else {
            format!(
                "{} 对当前用户不可写（只有读+执行权限），更新 DSH 会因权限被拒；\
                 可在设置页点「以管理员身份重启启动器」后再更新",
                target
            )
        },
    });

    let state = dsh_state();
    items.push(CheckItem {
        name: "运行状态".into(),
        status: "INFO".into(),
        detail: match state {
            DshState::Running => format!("DSH 正在运行：{}", web_url()),
            DshState::ForeignPort => {
                // 说清是谁占用——只说"端口被占用"用户没有任何可操作信息
                let owner = crate::process::port_owner_info(settings().web_port)
                    .map(|o| format!("，占用者：{}", o.describe()))
                    .unwrap_or_default();
                format!(
                    "端口 {} 被其他程序占用（非 DSH 进程）{}；可在首页结束该进程，或在设置页修改 Web 端口",
                    settings().web_port,
                    owner
                )
            }
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
