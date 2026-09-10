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

/// 当前 Unix 时间（秒）
pub(crate) fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 解析 npm registry 的发布时间戳（形如 `2026-09-10T06:53:19.720Z`）为 Unix 秒。
///
/// 只处理 registry 实际返回的这一种固定形态（UTC、`Z` 结尾），为它引入
/// chrono/time 依赖不划算；解析失败返回 None，由调用方降级为"不显示年龄"——
/// 宁可少一条提示，也不要拿错的时间去误导用户。
pub(crate) fn parse_rfc3339_utc_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let seg = s.get(r)?;
        if seg.is_empty() || !seg.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        seg.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // 只接受 `Z` 或 `.fffZ`。刻意不支持 `+08:00` 这类偏移：把非 UTC 的时间
    // 当成 UTC 会让"发布年龄"整体偏掉数小时，比不显示年龄更有害。
    let rest = s.get(19..)?;
    let tail = match rest.strip_prefix('.') {
        Some(frac) => {
            let end = frac
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(frac.len());
            if end == 0 {
                return None; // 小数点后必须有数字
            }
            &frac[end..]
        }
        None => rest,
    };
    if tail != "Z" {
        return None;
    }
    // days_from_civil（Howard Hinnant 算法）：以 1970-01-01 为 0，无需查表
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400; // [0, 399]
    let mp = (mo + 9) % 12; // 3 月 = 0 … 2 月 = 11
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// 解析 `major.minor.patch[-prerelease][+build]`；非法形式返回 None。
/// build metadata 按 SemVer 不参与优先级，这里直接丢弃。
fn parse_semver(s: &str) -> Option<([u64; 3], Option<Vec<String>>)> {
    let s = s.trim();
    let (core, rest) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    let mut nums = [0u64; 3];
    let mut it = core.split('.');
    for n in nums.iter_mut() {
        *n = it.next()?.parse::<u64>().ok()?;
    }
    if it.next().is_some() {
        return None; // 多于三段（如 1.2.3.4）
    }
    let pre = match rest.strip_prefix('-') {
        Some(p) => {
            let p = p.split('+').next().unwrap_or(p);
            if p.is_empty() {
                return None;
            }
            Some(p.split('.').map(|x| x.to_string()).collect::<Vec<_>>())
        }
        None => None,
    };
    Some((nums, pre))
}

/// SemVer 比较，规则与前端 `util.js` 的 `cmpVer` 完全一致——
/// 后端要判断"是否真的有新版本"（据此决定是否再查发布时间），
/// 两边判定不一致会让 UI 与后端对同一个包给出不同结论。
/// 两侧都解析失败时退化为字符串比较（稳定、不 panic）。
pub(crate) fn cmp_ver(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (Some(x), Some(y)) = (parse_semver(a), parse_semver(b)) else {
        return a.cmp(b);
    };
    for i in 0..3 {
        if x.0[i] != y.0[i] {
            return x.0[i].cmp(&y.0[i]);
        }
    }
    match (x.1, y.1) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater, // 正式版 > 预发布
        (Some(_), None) => Ordering::Less,
        (Some(p), Some(q)) => {
            for i in 0..p.len().max(q.len()) {
                match (p.get(i), q.get(i)) {
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(u), Some(v)) => {
                        if u == v {
                            continue;
                        }
                        // 纯数字标识按数值比较，否则按字典序；数字 < 非数字
                        return match (u.parse::<u64>().ok(), v.parse::<u64>().ok()) {
                            (Some(un), Some(vn)) => un.cmp(&vn),
                            (Some(_), None) => Ordering::Less,
                            (None, Some(_)) => Ordering::Greater,
                            (None, None) => u.cmp(v),
                        };
                    }
                    (None, None) => break,
                }
            }
            Ordering::Equal
        }
    }
}

/// 字符串转 UTF-16（含终止符），供 Win32 API 使用
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
// GUI 程序派生控制台子进程（npm/where/netstat 等）时默认会弹出一个新控制台窗口，
// 必须显式加 CREATE_NO_WINDOW 隐藏（TUI 使用 CREATE_NEW_CONSOLE 保持可见）
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 终止进程及其整棵子进程树，返回 taskkill 的真实结果。
///
/// Windows 上 npm / pnpm 只有 .cmd shim，实际链路是 cmd.exe → node.exe：
/// 只 `Child::kill()` 直接子进程会留下真正在干活的 node 继续运行——启动器已经
/// 报告"超时/失败"，后台却还在写 node_modules，后续操作会撞上文件占用。
/// /T 连子进程树一起终止（与 restart_dsh / stop_web 的终止口径一致）。
///
/// 返回 Err 时调用方**必须如实上报**：对以更高权限（管理员）运行的进程，
/// taskkill 会"拒绝访问"，此前把错误吞掉后只能猜"可能权限不足"，
/// 用户既看不到原因，也不知道下一步该做什么。
pub(crate) fn kill_tree(pid: u32) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let out = Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("taskkill 启动失败：{}", e))?;
    if out.status.success() {
        return Ok(());
    }
    // taskkill 的 stderr 受控制台编码影响可能含乱码，因此结论以"提示下一步"为主
    let raw = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let detail = if raw.is_empty() {
        format!("taskkill 退出码 {}", out.status)
    } else {
        raw
    };
    if is_elevated() {
        Err(format!("无法终止进程 {}（{}）", pid, detail))
    } else {
        Err(format!(
            "无法终止进程 {}（{}）。若该进程以管理员权限启动，\
             需要以管理员身份运行启动器才能结束它——可在设置页点「以管理员身份重启启动器」后重试",
            pid, detail
        ))
    }
}

/// 目录是否可写——实际"创建并删除"探测文件来判断。
///
/// 不解析 ACL：UAC 会从进程令牌里过滤掉 Administrators 组，只有真实写入才能
/// 反映"当前进程到底能不能写"。这正是更新 DSH 失败的根因所在——npm 全局目录
/// （如 `D:\Nodejs\node_global`）常常只给 Users 读+执行权限。
pub(crate) fn is_dir_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(
        ".dsh-writeprobe-{}-{}.tmp",
        std::process::id(),
        nanos
    ));
    match std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 当前进程是否以管理员（高完整性）令牌运行
pub(crate) fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// 以管理员身份重新启动本程序（UAC 提权），用于"必须提权才能写入"的操作。
///
/// 调用方负责在成功后退出当前实例（否则会同时存在两个启动器）。
/// 提权实例通过 `--elevated` 参数豁免单实例互斥——否则它会被已存在的实例
/// 判定为重复启动并立即退出，表现为"点了没反应"。
pub(crate) fn relaunch_elevated(extra_args: &[&str]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败：{}", e))?;
    let exe_w = wide(&exe.to_string_lossy());
    let verb = wide("runas");
    let params = wide(&extra_args.join(" "));
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe_w.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result as usize;
    if code <= 32 {
        // 5 = SE_ERR_ACCESSDENIED：用户在 UAC 对话框点了「否」
        return Err(if code == 5 {
            "提权请求被取消（UAC 未同意），启动器未重启".into()
        } else {
            format!("提权重启失败（ShellExecuteW 返回 {}）", code)
        });
    }
    Ok(())
}

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

// ---------------------------------------------------------------------------
// 运行日志（exe 旁 launcher.log）
// ---------------------------------------------------------------------------

/// 单份日志的滚动上限：超过 LOG_MAX_BYTES 时保留最后 LOG_KEEP_BYTES
const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const LOG_KEEP_BYTES: u64 = 512 * 1024;

/// exe 旁 launcher.log 的写入句柄（一次操作打开一次，按行写入）。
///
/// npm / pnpm / dsh plugin 的完整输出此前只推给 UI Channel，操作失败后
/// **无从回溯**——排障只能靠 npm 自己的调试日志，而 pnpm 的报错会彻底丢失。
/// 落盘后失败现场可复查（也是"按发布年龄兜底"能对症的前提）。
pub(crate) struct AppLog {
    file: Option<std::fs::File>,
}

impl AppLog {
    pub(crate) fn open() -> Self {
        let path = beside_exe("launcher.log");
        // 超限：整体重写为尾部，并从行边界开始（避免留下半行）
        if std::fs::metadata(&path)
            .map(|m| m.len() > LOG_MAX_BYTES)
            .unwrap_or(false)
        {
            if let Ok(bytes) = std::fs::read(&path) {
                let keep_from = bytes.len().saturating_sub(LOG_KEEP_BYTES as usize);
                let start = bytes[keep_from..]
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|i| keep_from + i + 1)
                    .unwrap_or(keep_from);
                let _ = std::fs::write(&path, &bytes[start..]);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        AppLog { file }
    }

    pub(crate) fn line(&mut self, msg: &str) {
        use std::io::Write;
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{}", msg);
        }
    }

    /// 分节标题（带本地时间戳），便于在日志里定位一次操作
    pub(crate) fn section(&mut self, title: &str) {
        let stamp = now_stamp().unwrap_or_default();
        self.line("");
        self.line(&format!("===== {} {} =====", stamp, title));
    }
}

/// launcher.log 的绝对路径（前端"查看运行日志"用）
pub(crate) fn app_log_path() -> PathBuf {
    beside_exe("launcher.log")
}

/// 一次性写一行到 launcher.log（打开-写入-关闭）。
/// 用于操作边界与错误摘要这类低频事件；高频逐行输出请复用同一个 [`AppLog`]。
pub(crate) fn app_log_line(msg: &str) {
    let mut g = AppLog::open();
    let stamp = now_stamp().unwrap_or_default();
    g.line(&format!("[{}] {}", stamp, msg));
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

#[cfg(test)]
mod tests {
    use super::parse_rfc3339_utc_secs;

    /// registry 发布时间戳的解析（期望值由 `date -u -d <t> +%s` 校验）
    #[test]
    fn parses_registry_publish_timestamps() {
        assert_eq!(parse_rfc3339_utc_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc_secs("2000-01-01T00:00:00Z"),
            Some(946_684_800)
        );
        // 真实证据：dsh-better-sidebar@0.19.0 的发布时间
        assert_eq!(
            parse_rfc3339_utc_secs("2026-09-10T06:53:19.720Z"),
            Some(1_789_023_199)
        );
        // 闰年 2 月 29 日、跨年边界
        assert_eq!(
            parse_rfc3339_utc_secs("2024-02-29T12:00:00Z"),
            Some(1_709_208_000)
        );
        assert_eq!(
            parse_rfc3339_utc_secs("2025-12-31T23:59:59Z"),
            Some(1_767_225_599)
        );
    }

    /// 畸形输入必须返回 None（调用方据此降级为"不显示年龄"）
    #[test]
    fn rejects_malformed_publish_timestamps() {
        for bad in [
            "",
            "2026-09-10",
            "2026/09/10T06:53:19Z", // 分隔符错误
            "2026-09-10T06:53:19",  // 缺 Z
            "2026-09-10T06:53:19.", // 小数点后无数字
            "2026-09-10T06:53:19.Z",
            "2026-09-10T06:53:19+08:00", // 不支持时区偏移（会把年龄算偏）
            "2026-13-10T06:53:19Z",      // 月份越界
            "2026-09-10T25:53:19Z",      // 小时越界
            "2026-09-10T06-53-19Z",      // 时间分隔符错位
            "abcd-09-10T06:53:19Z",      // 非数字
        ] {
            assert!(parse_rfc3339_utc_secs(bad).is_none(), "应拒绝：{}", bad);
        }
    }

    /// SemVer 比较：用例与前端 util.test.mjs 的 cmpVer 一一对应，
    /// 后端据此判断"是否真的有新版本"，两边规则必须一致
    #[test]
    fn compares_semver_like_frontend() {
        use super::cmp_ver;
        use std::cmp::Ordering::{Equal, Greater, Less};

        assert_eq!(cmp_ver("1.10.0", "1.9.9"), Greater); // 数字位而非字典序
        assert_eq!(cmp_ver("0.12.2", "0.12.2"), Equal);
        assert_eq!(cmp_ver("1.2.3", "1.2.4"), Less);
        // 预发布低于同号正式版
        assert_eq!(cmp_ver("1.0.0-rc.1", "1.0.0"), Less);
        assert_eq!(cmp_ver("1.0.0", "1.0.0-alpha"), Greater);
        // 预发布标识按数字/字典序规则
        assert_eq!(cmp_ver("1.0.0-2", "1.0.0-10"), Less);
        assert_eq!(cmp_ver("1.0.0-alpha", "1.0.0-beta"), Less);
        assert_eq!(cmp_ver("1.0.0-alpha.1", "1.0.0-alpha"), Greater);
        // 本机真实场景：DSH 0.1.5-rc.1 > 0.1.5-alpha.2
        assert_eq!(cmp_ver("0.1.5-rc.1", "0.1.5-alpha.2"), Greater);
        // build metadata 不参与优先级
        assert_eq!(cmp_ver("1.0.0+build.1", "1.0.0+build.2"), Equal);
        assert_eq!(cmp_ver("1.0.0+build", "1.0.0"), Equal);
        // 无效版本走字符串兜底
        assert_eq!(cmp_ver("abc", "abc"), Equal);
        assert_eq!(cmp_ver("abc", "abd"), Less);
        assert_eq!(cmp_ver("", ""), Equal);
    }

    /// 写权限探测：可写为 true、路径不存在为 false，且探测文件必须被清理
    /// （这个探测会落在 npm 全局前缀等目录里，留下垃圾文件是不可接受的）
    #[test]
    fn detects_directory_writability_and_cleans_probe() {
        let dir = std::env::temp_dir().join(format!("dsh-writable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(super::is_dir_writable(&dir));
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(leftovers.is_empty(), "探测文件未清理：{:?}", leftovers);
        // 不存在的目录不能报告为可写
        assert!(!super::is_dir_writable(&dir.join("does-not-exist")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
