//! DSH 进程跟踪与运行状态判定。
//!
//! 判定"DSH 是否在运行"必须以启动器管理的实例为准：webPort（3080）可能被
//! 与本启动器无关的服务占用（例如常驻的 DSH 环境/其它程序），仅凭端口探测
//! 会把无关服务误判为"DSH 在运行"，导致备份/恢复被错误拦截或误杀进程。

use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::locate::find_bin;
use crate::settings::settings;
use crate::util::{lock_ok, CREATE_NO_WINDOW};

/// 端口是否已有监听（廉价探测：TCP connect，500ms 超时）
pub(crate) fn is_running() -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", settings().web_port)
            .parse()
            .unwrap(),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 启动器自己启动的 DSH 进程 PID 注册表。
///
/// webPort 可能被无关服务占用，"是否运行"不能只看端口；但端口被监听时
/// 也可能确实是 DSH（启动器重启后跟踪表丢失的场景），由 dsh_state 的
/// 身份核验负责收编。
static TRACKED_DSH_PIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn track_dsh_pid(pid: u32) {
    let mut g = lock_ok(&TRACKED_DSH_PIDS);
    if !g.contains(&pid) {
        g.push(pid);
    }
}

pub(crate) fn untrack_dsh_pid(pid: u32) {
    lock_ok(&TRACKED_DSH_PIDS).retain(|&p| p != pid);
}

/// 进程是否存活（OpenProcess 探测；对启动器自己的子进程总是可访问）
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// 启动器启动的 DSH 实例是否仍在运行（按 PID 存活检查，顺带清理已退出 PID）
fn tracked_dsh_running() -> bool {
    let mut g = lock_ok(&TRACKED_DSH_PIDS);
    if g.is_empty() {
        return false;
    }
    let alive: Vec<u32> = g.iter().copied().filter(|&p| pid_alive(p)).collect();
    *g = alive.clone();
    !alive.is_empty()
}

/// 备份前的运行检查：只认启动器自己启动的 DSH 实例。
/// 备份是只读操作，外部/常驻实例占用端口不应拦截（运行中备份仅可能
/// 遗漏最后一条会话，不会损坏任何数据）。
pub(crate) fn dsh_running_for_backup() -> bool {
    tracked_dsh_running()
}

/// 恢复前的运行检查：备份判定 + 端口占用兜底。
/// 恢复会替换 $DSH_HOME，任何占用 webPort 的服务（即使无法识别身份）
/// 都按保守口径拦截。
pub(crate) fn dsh_running_for_restore() -> bool {
    tracked_dsh_running() || is_running()
}

/// 端口身份核验结果的短缓存：端口被无关服务占用时，避免状态轮询
/// 每 2 秒都跑一次 Get-CimInstance 查询
static FOREIGN_PORT_UNTIL: std::sync::Mutex<Option<std::time::Instant>> =
    std::sync::Mutex::new(None);

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum DshState {
    Running,
    ForeignPort,
    Stopped,
}

/// DSH 运行状态三态判定。
/// - Running：启动器管理的实例（tracked PID）存活；或端口监听进程经命令行
///   核验确认是 DSH（此时顺带把 PID 收编进跟踪表，覆盖"启动器重启后
///   跟踪表丢失"的场景）；或命令行读不到但 HTTP 协议特征确认是 DSH web
///   （DSH 以管理员权限运行时属于这种情形）；
/// - ForeignPort：webPort 被监听但无法确认是 DSH（不误报为运行中）；
/// - Stopped：端口无监听。
pub(crate) fn dsh_state() -> DshState {
    if tracked_dsh_running() {
        return DshState::Running;
    }
    if !is_running() {
        *lock_ok(&FOREIGN_PORT_UNTIL) = None;
        return DshState::Stopped;
    }
    // 端口被监听但跟踪表为空：做一次（带 30 秒缓存的）身份核验
    if let Some(until) = lock_ok(&FOREIGN_PORT_UNTIL).as_ref() {
        if std::time::Instant::now() < *until {
            return DshState::ForeignPort;
        }
    }
    let port = settings().web_port;
    let bin = find_bin().unwrap_or_default();
    let pids = find_dsh_pids(port);
    let mine = filter_dsh_pids(&pids, &bin);
    if !mine.is_empty() {
        for pid in mine {
            track_dsh_pid(pid);
        }
        return DshState::Running;
    }
    // 命令行核验失败：DSH 以更高权限运行时，非提权查询拿不到它的 CommandLine
    // （实测为 null），按旧逻辑会把自家 DSH 判成"外来占用"。用协议特征兜底。
    // 这里刻意**不**收编进跟踪表——备份"只拦启动器自己启动的实例"的宽松口径保持不变。
    if probe_is_dsh(port) {
        return DshState::Running;
    }
    *lock_ok(&FOREIGN_PORT_UNTIL) = Some(std::time::Instant::now() + Duration::from_secs(30));
    DshState::ForeignPort
}

/// 状态胶囊数据：state ∈ running / foreign-port / stopped
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusDetail {
    state: &'static str,
    port: u16,
}

pub(crate) fn find_dsh_pids(port: u16) -> Vec<u32> {
    let out = match Command::new("netstat")
        .args(["-ano"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Vec::new(),
    };
    let mut pids = Vec::new();
    for line in out.lines() {
        // netstat -ano 的 TCP 监听行：TCP <本地地址> <外部地址> LISTENING <pid>
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 && parts[3] == "LISTENING" {
            // 精确比较本地地址列的端口（取最后一个 ':' 之后），
            // 不能用 contains(":3080")——会模糊命中 :30800 / :13080 等无关端口
            let port_matches = parts[1]
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .map(|p| p == port)
                .unwrap_or(false);
            if port_matches {
                if let Ok(pid) = parts[parts.len() - 1].parse::<u32>() {
                    if !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    pids
}

/// 过滤出命令行包含 DSH bin 路径的进程（PowerShell CIM 查询）。
/// 防止 3080 端口被无关程序占用时 taskkill 误杀；
/// bin 为空或查询失败时返回空列表。无法确认进程身份时必须拒绝终止，
/// 避免端口被无关服务占用时误杀其它进程。
///
/// 匹配用 IndexOf(OrdinalIgnoreCase) 精确子串比较：不能用 -like '*bin*'——
/// 路径中的 [ ] * ? 是通配符元字符，安装路径含这些字符时永远失配，
/// 表现为 restart/stop_web 误报"端口被其他程序占用"。
pub(crate) fn filter_dsh_pids(pids: &[u32], bin: &str) -> Vec<u32> {
    if pids.is_empty() || bin.is_empty() {
        return Vec::new();
    }
    let filter = pids
        .iter()
        .map(|p| format!("ProcessId={}", p))
        .collect::<Vec<_>>()
        .join(" or ");
    let ps = format!(
        "Get-CimInstance Win32_Process -Filter '{}' | Where-Object {{ $_.CommandLine -and \
         ($_.CommandLine.IndexOf('{}', [System.StringComparison]::OrdinalIgnoreCase) -ge 0) }} \
         | ForEach-Object {{ [int]$_.ProcessId }}",
        filter,
        // bin 拼进单引号 PS 字符串：' 翻倍转义，防含撇号路径破坏查询
        bin.replace('\'', "''")
    );
    let out = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(), // 查询失败：拒绝终止
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let matched: Vec<u32> = text
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|pid| pids.contains(pid))
        .collect();
    matched
}

/// 判断一段 HTTP 响应是否来自 DSH web（纯函数，便于单测）。
///
/// 依据：DSH web 对未携带 token 的请求返回 401，正文为
/// `dsh web authentication required; reopen the URL printed by dsh web.`
pub(crate) fn looks_like_dsh_response(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes)
        .to_ascii_lowercase()
        .contains("dsh web")
}

/// 用 HTTP 协议特征确认端口后面是不是 DSH web。
///
/// 为什么需要它：DSH 若**以更高权限（管理员）运行**，非提权的 WMI/CIM 查询读不到
/// 它的 `CommandLine`（本机实测为 null），基于命令行的身份核验会 fail closed，
/// 于是把自家 DSH 误判成"被其他程序占用"。协议探针不依赖进程元数据，不受权限影响。
pub(crate) fn probe_is_dsh(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = match format!("127.0.0.1:{}", port).parse() {
        Ok(a) => a,
        Err(_) => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(600)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
    let req = format!(
        "GET / HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        port
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    // 只读到能覆盖状态行 + 短正文即可，不读完整响应（避免被大页面拖住）
    let mut buf = [0u8; 2048];
    let mut got: Vec<u8> = Vec::new();
    while got.len() < 1024 {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    looks_like_dsh_response(&got)
}

/// 选定"可以安全终止"的 DSH PID 集合：先按命令行精确核验，
/// 失败时用协议探针兜底确认身份（覆盖 DSH 以更高权限运行、读不到命令行的情形）。
/// 两者都失败则返回空——拒绝终止，避免误杀无关进程。
pub(crate) fn dsh_pids_for_termination(port: u16) -> Vec<u32> {
    let pids = find_dsh_pids(port);
    if pids.is_empty() {
        return pids;
    }
    let bin = find_bin().unwrap_or_default();
    let verified = filter_dsh_pids(&pids, &bin);
    if !verified.is_empty() {
        return verified;
    }
    if probe_is_dsh(port) {
        return pids;
    }
    Vec::new()
}

#[tauri::command]
pub(crate) async fn status() -> bool {
    // 与恢复的保守口径一致：tracked 或端口任一命中即视为运行
    tauri::async_runtime::spawn_blocking(|| tracked_dsh_running() || is_running())
        .await
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 端口占用者处置：只说"端口被占用"没有可操作性，必须告诉用户是谁占用、
// 并给出可执行的下一步（结束它 / 换端口）。
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortOwnerInfo {
    pid: u32,
    name: String,
    path: String,
    /// 命令行核验确认是 DSH
    is_dsh: bool,
    /// 命令行读不到（多为以管理员权限运行的进程），但 HTTP 协议特征确认是 DSH web
    likely_dsh: bool,
}

impl PortOwnerInfo {
    /// 一行式描述（进程名或路径 + PID），用于提示文案
    pub(crate) fn describe(&self) -> String {
        let what = if !self.name.is_empty() {
            self.name.clone()
        } else if !self.path.is_empty() {
            self.path.clone()
        } else {
            "未知进程".to_string()
        };
        format!("{}（PID {}）", what, self.pid)
    }
}

/// 查询进程的 (名称, 可执行路径)；查不到时返回空字符串。
/// PowerShell CIM 是唯一能同时拿到 Name 与 ExecutablePath 的常规手段。
///
/// 注意：`ExecutablePath` 在实践中经常为空（非提权查询他人/受保护进程时），
/// 所以调用方必须以 PID 为主、路径为可选信息。命令前置 UTF-8 输出编码声明，
/// 否则中文路径会被控制台默认编码（GBK）写出、读回时变成乱码。
fn process_identity(pid: u32) -> (String, String) {
    let ps = format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         Get-CimInstance Win32_Process -Filter 'ProcessId={}' | ForEach-Object {{ \
         $_.Name + '|' + $_.ExecutablePath }}",
        pid
    );
    let out = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return (String::new(), String::new()),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        Some(l) => match l.split_once('|') {
            Some((n, p)) => (n.trim().to_string(), p.trim().to_string()),
            None => (l.to_string(), String::new()),
        },
        None => (String::new(), String::new()),
    }
}

/// 当前 webPort 的占用者信息（PID / 进程名 / 路径 / 是否 DSH 自身）。
///
/// 刻意做成独立的按需命令，而不是塞进 `status_detail`：后者每 2 秒被轮询，
/// 而这里要起一次 PowerShell CIM 查询，放进轮询会把常态开销抬高一个量级。
pub(crate) fn port_owner_info(port: u16) -> Option<PortOwnerInfo> {
    let pids = find_dsh_pids(port);
    let pid = *pids.first()?;
    let bin = find_bin().unwrap_or_default();
    let is_dsh = !bin.is_empty() && filter_dsh_pids(&[pid], &bin).contains(&pid);
    // 命令行读不到时（DSH 以管理员权限运行时如此）用协议特征兜底，避免把
    // 自家 DSH 展示成"其他程序"
    let likely_dsh = !is_dsh && probe_is_dsh(port);
    let (name, path) = process_identity(pid);
    Some(PortOwnerInfo {
        pid,
        name,
        path,
        is_dsh,
        likely_dsh,
    })
}

/// 查询占用者信息（前端在状态变为"端口被占用"或用户点击「查看占用」时调用）
#[tauri::command]
pub(crate) async fn port_owner() -> Option<PortOwnerInfo> {
    tauri::async_runtime::spawn_blocking(|| port_owner_info(settings().web_port))
        .await
        .ok()
        .flatten()
}

/// 结束占用 webPort 的进程（前端已做二次确认）。
///
/// Windows 上 taskkill /F 结束他人的进程需要提权，失败会如实返回原因；
/// 拒绝结束 DSH 自身（那应该走「关闭 Web 界面」）与系统进程，
/// 并在动手前重新确认该 PID 此刻仍在监听目标端口，避免 PID 复用导致误杀。
#[tauri::command]
pub(crate) async fn kill_port_owner(pid: u32) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let port = settings().web_port;
        if pid == 0 || pid == 4 || pid == std::process::id() {
            return Err("拒绝终止系统进程或启动器自身".into());
        }
        if !find_dsh_pids(port).contains(&pid) {
            return Err(format!(
                "进程 {} 已不再监听端口 {}（可能刚刚退出），请点「重新检测」",
                pid, port
            ));
        }
        let bin = find_bin().unwrap_or_default();
        if !bin.is_empty() && filter_dsh_pids(&[pid], &bin).contains(&pid) {
            return Err("该进程就是 DSH 自身，请改用「关闭 Web 界面」".into());
        }
        // 命令行读不到时（DSH 以管理员权限运行）再用协议探针兜底：
        // 确属 DSH 就绝不能当外来进程强杀
        if probe_is_dsh(port) {
            return Err(
                "该端口由 DSH 自身占用（可能以管理员权限运行），请改用「关闭 Web 界面」；\
                        若关闭失败，请点「以管理员身份重启启动器」后重试"
                    .into(),
            );
        }
        let (name, _) = process_identity(pid);
        let who = if name.is_empty() {
            format!("PID {}", pid)
        } else {
            format!("{} (PID {})", name, pid)
        };
        crate::util::app_log_line(&format!("结束占用端口 {} 的进程：{}", port, who));
        // 如实透传 taskkill 的结果：权限不足时错误文案里已带"以管理员身份…"的下一步
        crate::util::kill_tree(pid)?;
        // 等待端口释放（最多约 3 秒）
        for _ in 0..12 {
            if !is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if is_running() {
            Err(format!(
                "已结束 {}，但端口 {} 仍被监听（可能存在多个占用者）",
                who, port
            ))
        } else {
            Ok(format!("已结束 {}，端口 {} 已释放", who, port))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 状态胶囊的三态数据（前端 pollStatus 使用）。deep 身份核验只在
/// "端口被监听且跟踪表为空"时发生，并带 30 秒缓存，常态轮询仍是廉价探测。
#[tauri::command]
pub(crate) async fn status_detail() -> StatusDetail {
    tauri::async_runtime::spawn_blocking(|| StatusDetail {
        state: match dsh_state() {
            DshState::Running => "running",
            DshState::ForeignPort => "foreign-port",
            DshState::Stopped => "stopped",
        },
        port: settings().web_port,
    })
    .await
    .unwrap_or(StatusDetail {
        state: "stopped",
        port: settings().web_port,
    })
}

#[cfg(test)]
mod tests {
    use super::filter_dsh_pids;

    #[test]
    fn process_filter_fails_closed_without_verified_binary() {
        let pids = vec![1234, 5678];
        assert!(filter_dsh_pids(&pids, "").is_empty());
        assert!(filter_dsh_pids(&[], "some-bin").is_empty());
    }

    /// HTTP 协议特征识别：DSH web 的 401 正文必须被认出，无关服务不得误判。
    /// 这是"命令行读不到时"唯一的身份来源（DSH 以管理员权限运行的情形）。
    /// 用例中的响应取自本机真实抓包。
    #[test]
    fn recognizes_dsh_web_response() {
        use super::looks_like_dsh_response;
        let real = b"HTTP/1.1 401 Unauthorized\r\ncache-control: no-store\r\n\
content-type: text/plain; charset=utf-8\r\nVary: Accept-Encoding\r\n\r\n\
dsh web authentication required; reopen the URL printed by dsh web.";
        assert!(looks_like_dsh_response(real), "真实 401 响应必须被识别");
        // 大小写不敏感
        assert!(looks_like_dsh_response(b"HTTP/1.1 200 OK\r\n\r\nDSH Web"));
        // 无关服务 / 空响应不得命中
        assert!(!looks_like_dsh_response(
            b"HTTP/1.1 200 OK\r\n\r\n<html>nginx</html>"
        ));
        assert!(!looks_like_dsh_response(b""));
        assert!(!looks_like_dsh_response(b"not http at all"));
    }
}
