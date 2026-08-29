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
/// - Running：启动器管理的实例（tracked PID）存活，或端口监听进程经命令行
///   核验确认是 DSH（此时顺带把 PID 收编进跟踪表，覆盖"启动器重启后
///   跟踪表丢失"的场景）；
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
    // 端口被监听但跟踪表为空：做一次（带 30 秒缓存的）命令行身份核验
    if let Some(until) = lock_ok(&FOREIGN_PORT_UNTIL).as_ref() {
        if std::time::Instant::now() < *until {
            return DshState::ForeignPort;
        }
    }
    let bin = find_bin().unwrap_or_default();
    let pids = find_dsh_pids(settings().web_port);
    let mine = filter_dsh_pids(&pids, &bin);
    if mine.is_empty() {
        *lock_ok(&FOREIGN_PORT_UNTIL) = Some(std::time::Instant::now() + Duration::from_secs(30));
        DshState::ForeignPort
    } else {
        for pid in mine {
            track_dsh_pid(pid);
        }
        DshState::Running
    }
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

#[tauri::command]
pub(crate) async fn status() -> bool {
    // 与恢复的保守口径一致：tracked 或端口任一命中即视为运行
    tauri::async_runtime::spawn_blocking(|| tracked_dsh_running() || is_running())
        .await
        .unwrap_or(false)
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
}
