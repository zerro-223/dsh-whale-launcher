//! DSH 启动器（Tauri 2 / Rust 后端）
//!
//! 模块划分（按职责）：
//! - [`util`]：跨模块小工具（exe 旁路径、原子写、时间戳、操作互斥 guard）
//! - [`settings`]：settings.json 读写与开机自启（注册表 Run 键）
//! - [`locate`]：DSH 安装位置自动识别与 $DSH_HOME 定位
//! - [`npm`]：which 探测缓存、npm/通用子进程执行（流式进度 + 超时）
//! - [`proxy`]：代理环境变量构造（Node 与 npm 两套开关）与系统代理读取
//! - [`process`]：端口/PID 跟踪与 DSH 运行状态三态判定
//! - [`backup`]：数据备份 / 恢复（zip 打包、ZipSlip 防护、改名换位恢复）
//! - [`plugins`]：插件管理（官方 dsh plugin 通道 + 轻量 YAML 解析）
//! - [`launch`]：Web / TUI / Headless 启动、重启与停止（web.log）
//! - [`checks`]：启动前自检
//! - [`update`]：版本检测、更新检查与一键安装
//! - [`tray`]：系统托盘、单实例互斥与主窗口行为
//!
//! 所有阻塞性操作（端口探测、npm、netstat、子进程）都在后台线程执行
//! （async 命令 + spawn_blocking），前端只负责展示与调用，UI 零阻塞。

mod backup;
mod checks;
mod launch;
mod locate;
mod npm;
mod plugins;
mod process;
mod proxy;
mod settings;
mod tray;
mod update;
mod util;

use std::time::Duration;

use tauri::Manager;

use crate::settings::settings;

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 单实例：已有实例则激活其窗口并退出本进程
    if !tray::acquire_single_instance() {
        return;
    }
    tauri::Builder::default()
        .setup(|app| {
            tray::setup_tray(app)?;
            // 后台清理崩溃遗留的备份临时目录（不阻塞启动）
            std::thread::spawn(backup::cleanup_stale_temp_dirs);
            // 关闭窗口 = 隐藏到后台（托盘常驻），真正退出走托盘菜单「退出」。
            // on_window_event 的注册走异步消息，setup 期间窗口可能未就绪导致
            // 监听被丢弃（时序竞态），因此延迟注册；前端 onCloseRequested
            // 作为第二道保险。
            let app_handle = app.handle().clone();
            std::thread::spawn(move || {
                // 延迟 3.5 秒注册：窗口创建初期 WebView2 会误发一次
                // close-requested（前端有 3 秒启动保护期），避开该窗口期
                std::thread::sleep(Duration::from_millis(3500));
                if let Some(win) = app_handle.get_webview_window("main") {
                    let win_h = win.clone();
                    win.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            // 设置页配置「直接退出」时放行默认关闭流程（窗口销毁、进程退出）
                            if settings().close_action == "quit" {
                                return;
                            }
                            api.prevent_close();
                            // show() 再 hide()：外部 ShowWindow 恢复的窗口会使 tao
                            // 内部可见性 flags 与实际不同步，直接 hide() 会被判定为
                            // "无变化"而空操作；先 show() 同步 flags 再 hide() 才能
                            // 可靠隐藏，同时保证托盘 show() 能正常唤出
                            let _ = win_h.show();
                            let _ = win_h.hide();
                        }
                    });
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            checks::checks,
            process::status,
            process::status_detail,
            launch::start_web,
            launch::open_browser,
            launch::start_tui,
            launch::start_headless,
            launch::restart_dsh,
            launch::stop_web,
            update::update_check,
            update::update_dsh,
            update::install_dsh,
            proxy::get_system_proxy_cmd,
            checks::fix_commands,
            locate::open_install_dir,
            launch::get_web_cmd,
            launch::get_web_url,
            plugins::plugin_list,
            plugins::plugin_install,
            plugins::plugin_remove,
            plugins::plugin_update,
            plugins::plugin_check_updates,
            plugins::plugin_toggle,
            plugins::plugin_detail,
            launch::open_url,
            launch::read_web_log,
            settings::get_settings,
            settings::save_settings,
            settings::get_autostart,
            settings::set_autostart,
            locate::list_profiles,
            backup::backup_dsh,
            backup::restore_dsh,
            backup::list_backups,
            backup::open_backups_dir
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
