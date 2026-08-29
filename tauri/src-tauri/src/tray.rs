//! 系统托盘、单实例互斥与主窗口行为。

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

use crate::util::wide;

/// 获取单实例互斥体。已有实例运行时：激活其主窗口并返回 false（本进程应退出）。
pub(crate) fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE,
    };
    let name = wide("DSH-Launcher-SingleInstance");
    unsafe {
        let mutex = CreateMutexW(std::ptr::null(), 1, name.as_ptr());
        if mutex.is_null() {
            // 互斥体创建失败（权限等）：不阻止运行，避免启动器无法打开
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // 已有实例：按窗口标题找到主窗口并激活，然后本进程退出
            let title = wide("DSH 启动器");
            let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
            if !hwnd.is_null() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            return false;
        }
    }
    true
}

pub(crate) fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        // Win32 兜底：外部隐藏（如托盘关闭时的 Win32 路径）可能使 tao 内部
        // 可见性状态与实际不同步，直接 ShowWindow 确保窗口一定显示
        if let Ok(hwnd) = w.hwnd() {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(hwnd.0 as _, 5);
                // SW_SHOW
            }
        }
    }
}

/// 用 png crate 把内置图标解码为 RGBA（tauri 的 image-png feature 依赖 image
/// crate，离线构建环境不可用，这里直接用 png crate 解码，不引入额外依赖）。
fn load_tray_icon() -> Option<tauri::image::Image<'static>> {
    let bytes = include_bytes!("../icons/32x32.png");
    let mut cursor = std::io::Cursor::new(bytes);
    let decoder = png::Decoder::new(&mut cursor);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let mut rgba = buf[..info.buffer_size()].to_vec();
    match reader.output_color_type() {
        (png::ColorType::Rgba, _) => {}
        (png::ColorType::Rgb, _) => {
            let mut out = Vec::with_capacity(rgba.len() / 3 * 4);
            for i in (0..rgba.len()).step_by(3) {
                out.extend_from_slice(&[rgba[i], rgba[i + 1], rgba[i + 2], 255]);
            }
            rgba = out;
        }
        _ => return None,
    }
    Some(tauri::image::Image::new_owned(
        rgba,
        info.width,
        info.height,
    ))
}

/// 系统托盘：左键单击或菜单「打开主窗口」显示窗口；「退出」才真正结束进程。
/// 窗口关闭按钮改为隐藏到托盘（后台保持状态轮询）。
pub(crate) fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show_i = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_i, &quit_i])?;
    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("DSH 启动器")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });
    // 图标解码失败时跳过图标（托盘仍可用），不阻塞应用启动
    if let Some(icon) = load_tray_icon() {
        builder = builder.icon(icon);
    }
    builder.build(app)?;
    Ok(())
}
