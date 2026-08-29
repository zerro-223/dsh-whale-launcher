//! 跨模块小工具：exe 旁路径、原子写、本地时间戳、UTF-16 宽字符、
//! ShellExecute 打开目标，以及通用的一次性操作互斥 guard。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 容忍 Mutex 中毒的加锁：某次 panic 污染后仍可继续工作（数据本身无副作用）
pub(crate) fn lock_ok<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// exe 所在目录（进程生命周期内不变，缓存避免热路径反复执行 current_exe 系统调用）
pub(crate) static EXE_DIR: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
});

/// exe 旁路径；取不到 exe 目录时回退当前工作目录
pub(crate) fn beside_exe(name: &str) -> PathBuf {
    match EXE_DIR.as_ref() {
        Some(dir) => dir.join(name),
        None => std::env::current_dir().unwrap_or_default().join(name),
    }
}

/// 原子写文件：先写同目录临时文件再 rename 替换。
/// Windows 上 std::fs::rename = MoveFileExW(MOVEFILE_REPLACE_EXISTING)，
/// 可直接替换已存在文件；不要"先删后改"——进程死在中间会留下无配置窗口期。
pub(crate) fn write_atomic(path: &Path, content: &str, temp_prefix: &str) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(".{}-{}.tmp", temp_prefix, std::process::id()));
    std::fs::write(&tmp, content).map_err(|e| format!("写入临时文件失败：{}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换文件失败：{}", e)
    })
}

/// 生成本地时间戳 yyyy-MM-dd_HHmmss（Win32 GetLocalTime，
/// 不再为取一个时间戳启动 PowerShell 子进程，每次调用省约 200-500ms）
pub(crate) fn now_stamp() -> Result<String, String> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe {
        let mut st: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut st);
        st
    };
    Ok(format!(
        "{:04}-{:02}-{:02}_{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    ))
}

/// 字符串转 UTF-16（含终止符），供 Win32 API 使用
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
// GUI 程序派生控制台子进程（npm/where/netstat 等）时默认会弹出一个新控制台窗口，
// 必须显式加 CREATE_NO_WINDOW 隐藏（TUI / Headless 用 CREATE_NEW_CONSOLE 保持可见）
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// ShellExecuteW 打开目标（URL / 文件 / 目录）：不走 cmd /c start，
/// 避免目标含 & 等 cmd 元字符时被截断甚至二次解析执行
/// （实证：cmd /c start "" "http://x/?a=1&b=2" 会只打开 a=1 并把 b=2 当命令执行）
pub(crate) fn shell_open(target: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide("open");
    let target_w = wide(target);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    result as usize > 32
}

/// 通用的一次性操作互斥（备份/恢复、DSH 启动类操作共用）：
/// 同一时刻只允许一个持有者，RAII 释放，任何退出路径都解锁。
pub(crate) struct OpGuard(&'static AtomicBool);

impl OpGuard {
    pub(crate) fn acquire(flag: &'static AtomicBool, busy_message: &str) -> Result<Self, String> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| busy_message.to_string())?;
        Ok(OpGuard(flag))
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
