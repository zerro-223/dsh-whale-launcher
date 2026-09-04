//! DSH 启动方式：Web 模式（后台 + web.log）、TUI 终端，
//! 以及重启 / 停止与 web.log 查看命令。
//!
//! 所有启动类操作共用 START_OP 互斥：Web 启动要等待端口就绪（最长 8 秒），
//! 双击或并发触发会拉起两个 node 进程（第二个绑定端口失败），产生假错误。

use std::io::Write;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use tauri::ipc::Channel;

use crate::locate::{dsh_home_dir, find_bin, web_url};
use crate::npm::apply_registry;
use crate::plugins::run_profile_cmd;
use crate::process::{
    dsh_state, filter_dsh_pids, find_dsh_pids, is_running, track_dsh_pid, untrack_dsh_pid, DshState,
};
use crate::proxy::{npm_proxy_env, proxy_env_or_none};
use crate::settings::settings;
use crate::util::{beside_exe, shell_open, OpGuard, CREATE_NEW_CONSOLE, CREATE_NO_WINDOW};

/// 启动类操作互斥（start_web / start_tui / restart_dsh）
static START_OP: AtomicBool = AtomicBool::new(false);
const START_BUSY_MSG: &str = "另一个启动操作正在进行，请稍候…";

// ---------------------------------------------------------------------------
// Web 模式：输出写入 web.log（exe 旁），启动失败 / 端口未就绪时
// 回读日志末尾给出错误信息（不再静默吞掉）
// ---------------------------------------------------------------------------
fn web_log_path() -> std::path::PathBuf {
    beside_exe("web.log")
}

fn tail_file(path: &std::path::Path, max_bytes: usize) -> String {
    // Seek 到尾部再读：web.log 只在启动时截断（>1MB），长跑实例可能增长到数百 MB，
    // 整文件读入再截尾会造成不必要的内存与 IO 开销
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes as u64);
    if std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if std::io::Read::read_to_end(&mut f, &mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn append_line(path: &std::path::Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", line);
    }
}

/// 打开 web.log（追加模式）；日志超过 1MB 时先截断，防止无限增长
fn open_web_log() -> Result<std::fs::File, std::io::Error> {
    let path = web_log_path();
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// 启动 Web 模式：stdout/stderr 重定向到 web.log，随后等待端口就绪
/// （最多 8 秒）。
/// 返回值语义：
/// - "started"：端口已就绪；
/// - "starting"：超时但进程仍存活（冷启动慢）——不是失败，交由状态轮询确认；
/// - Err：进程提前退出（回读 web.log 末尾给出上下文）。
fn spawn_web(args: &[&str], env: &[(String, String)]) -> Result<String, String> {
    let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
    let path = web_log_path();
    let log = open_web_log().map_err(|e| format!("无法创建日志文件 {}：{}", path.display(), e))?;
    let mut cmd = Command::new("node");
    cmd.arg(&bin)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW) // Web 模式后台运行，不弹控制台窗口
        .stdout(Stdio::from(
            log.try_clone()
                .map_err(|e| format!("日志文件错误：{}", e))?,
        ))
        .stderr(Stdio::from(log));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动失败：{}", e))?;
    append_line(
        &path,
        &format!("==== DSH web 启动（PID {}）====", child.id()),
    );

    // 等待端口就绪；进程提前退出则回读日志末尾
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if is_running() {
            track_dsh_pid(child.id()); // 记录启动器管理的 DSH 实例 PID
            return Ok("started".to_string());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = tail_file(&path, 8192).trim().to_string();
            if tail.is_empty() {
                return Err(format!(
                    "DSH 启动后立即退出（退出码 {}），web.log 无输出",
                    status
                ));
            }
            return Err(format!(
                "DSH 启动失败（退出码 {}）\n\n--- web.log 末尾 ---\n{}",
                status, tail
            ));
        }
        if std::time::Instant::now() >= deadline {
            // 进程仍存活、只是启动慢：按"启动中"返回而非失败，
            // 避免用户在 DSH 即将就绪时看到误导性的错误弹窗
            return Ok("starting".to_string());
        }
    }
}

#[tauri::command]
pub(crate) async fn start_web(proxy_on: bool, proxy_addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = OpGuard::acquire(&START_OP, START_BUSY_MSG)?;
        match dsh_state() {
            DshState::Running => return Ok("already-running".to_string()),
            DshState::ForeignPort => {
                // 端口被无关服务占用：此前仅凭端口探测会误报 already-running，
                // 现在给出可行动的错误信息
                let port = settings().web_port;
                return Err(format!(
                    "端口 {} 已被其他程序占用，无法启动 DSH。可停止占用程序，\
                     或在 settings.json 中修改 webPort",
                    port
                ));
            }
            DshState::Stopped => {}
        }
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        // --no-open：dsh web 默认自行打开默认浏览器，会与启动器的
        // autoOpenBrowser 设置各开一次（弹出两个页面窗口），统一交启动器控制
        spawn_web(&["web", "--no-open"], &env)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) fn open_browser() {
    shell_open(&web_url());
}

/// TUI 快速退出时补跑一次诊断。TUI 必须运行在真实终端里（无 TTY 的
/// CREATE_NO_WINDOW 跑法会直接挂在 isTTY 检查上、拿不到真实错误），
/// 所以诊断同样用 CREATE_NEW_CONSOLE 起真实控制台，stderr 落到临时文件，
/// 进程退出后回读文件末尾；正常运行（6 秒未退出）则终止并提示无需诊断。
fn tui_diagnose(bin: &str, proxy_on: bool, proxy_addr: &str) -> String {
    let err_path = std::env::temp_dir().join(format!("dsh-tui-diag-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&err_path);
    let file = match std::fs::File::create(&err_path) {
        Ok(f) => f,
        Err(e) => return format!("诊断日志创建失败：{}", e),
    };
    let mut cmd = Command::new("node");
    cmd.arg(bin)
        .args(["--profile", "dsh-tui"])
        .creation_flags(CREATE_NEW_CONSOLE)
        .stderr(Stdio::from(file));
    for (k, v) in npm_proxy_env(proxy_on, proxy_addr) {
        cmd.env(k, v);
    }
    apply_registry(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&err_path);
            return format!("诊断执行失败：{}", e);
        }
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let text = std::fs::read_to_string(&err_path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            let tail = lines
                .iter()
                .rev()
                .take(15)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let _ = std::fs::remove_file(&err_path);
            return if tail.is_empty() {
                format!("（退出码 {}，无 stderr 输出）", status)
            } else {
                tail
            };
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = std::fs::remove_file(&err_path);
            return "（诊断运行 6 秒未退出，可能已正常启动）".to_string();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[tauri::command]
pub(crate) async fn start_tui(
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = OpGuard::acquire(&START_OP, START_BUSY_MSG)?;
        // TUI 不是 DSH 内置模式，而是独立插件 @deepseek-harness-tui/dsh-tui
        // （官方公众号收录，MIT）；其 profile 名为 dsh-tui（不是 tui）。
        // 首次点击时自动初始化并安装，之后直接启动。
        let profile_dir = dsh_home_dir().join("profiles").join("dsh-tui");
        if !profile_dir.join("package.json").is_file() {
            let _ = progress
                .send("TUI 未安装，正在自动安装 @deepseek-harness-tui/dsh-tui …".to_string());
            run_profile_cmd(
                "dsh-tui",
                &["add", "@deepseek-harness-tui/dsh-tui"],
                proxy_on,
                &proxy_addr,
                &progress,
            )
            .map_err(|e| format!("TUI 自动安装失败：{}", e))?;
            let _ = progress.send("TUI 安装完成，正在启动…".to_string());
        }
        let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        let mut cmd = Command::new("node");
        cmd.arg(&bin)
            .args(["--profile", "dsh-tui"])
            // TUI 需要真实终端：stdout/stderr 必须继承新控制台（不要置 null！
            // null 会让 isTTY=false，dsh-tui 直接报 "requires an interactive
            // terminal" 退出，表现为控制台窗口一闪即逝——这正是此前崩溃的根因）。
            .creation_flags(CREATE_NEW_CONSOLE);
        for (k, v) in env {
            cmd.env(k, v);
        }
        apply_registry(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("启动失败：{}", e))?;
        let pid = child.id();
        track_dsh_pid(pid);
        // 进程快速退出 = 启动即失败（旧行为：控制台一闪即逝、错误不可见）。
        // 等待 2.5 秒：存活则视为成功；退出则补一次诊断捕获真实错误。
        std::thread::sleep(Duration::from_millis(2500));
        if let Ok(Some(status)) = child.try_wait() {
            let diag = tui_diagnose(&bin, proxy_on, &proxy_addr);
            return Err(format!(
                "TUI 启动失败（退出码 {}）{}",
                status,
                if diag.is_empty() {
                    String::new()
                } else {
                    format!("\n\n--- 诊断输出 ---\n{}", diag)
                }
            ));
        }
        Ok(format!(
            "TUI 已启动（PID {}），请在弹出的命令行窗口中操作",
            pid
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub(crate) async fn restart_dsh(proxy_on: bool, proxy_addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = OpGuard::acquire(&START_OP, START_BUSY_MSG)?;
        let pids = find_dsh_pids(settings().web_port);
        // 仅终止命令行包含 DSH bin 路径的进程，避免 3080 被无关程序占用时误杀；
        // /T 连同子进程树一起终止（node 的 worker 子进程不留孤儿）
        let bin = find_bin().unwrap_or_default();
        let dsh_pids = filter_dsh_pids(&pids, &bin);
        for pid in &dsh_pids {
            untrack_dsh_pid(*pid);
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        // 等待端口释放（最多约 6 秒）
        for _ in 0..24 {
            if !is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if is_running() {
            return Err(if dsh_pids.is_empty() {
                format!(
                    "端口 {} 被其他程序占用，无法重启 DSH。可修改 webPort 或停止占用程序",
                    settings().web_port
                )
            } else {
                "still-running".into()
            });
        }
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        // 同 start_web：禁用 dsh web 自带的开浏览器行为（防双开）
        let spawned = spawn_web(&["web", "--no-open"], &env)?;
        Ok(if spawned == "starting" {
            "starting"
        } else {
            "ok"
        }
        .to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 关闭 Web 界面：仅终止监听 webPort 的 DSH 进程（带命令行身份校验，
/// /T 连同子进程树一起终止），等待端口释放后返回，不重新启动。
#[tauri::command]
pub(crate) async fn stop_web() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pids = find_dsh_pids(settings().web_port);
        if pids.is_empty() {
            return Ok("not-running".to_string());
        }
        let bin = find_bin().unwrap_or_default();
        let dsh_pids = filter_dsh_pids(&pids, &bin);
        if dsh_pids.is_empty() {
            return Err(format!(
                "端口 {} 被其他程序占用，无法安全终止（启动器只终止 DSH 进程）",
                settings().web_port
            ));
        }
        let mut killed = 0usize;
        for pid in &dsh_pids {
            untrack_dsh_pid(*pid);
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            killed += 1;
        }
        // 等待端口释放（最多约 6 秒）
        for _ in 0..24 {
            if !is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if is_running() {
            return Err("still-running".into());
        }
        Ok(format!("stopped:{}", killed))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 用系统默认浏览器打开 URL（仅允许 http/https，防参数注入）
#[tauri::command]
pub(crate) fn open_url(url: String) -> Result<String, String> {
    let u = url.trim();
    if (!u.starts_with("http://") && !u.starts_with("https://"))
        || u.chars().any(|c| c.is_control() || c == '\n' || c == '\r')
    {
        return Err("仅支持打开 http/https 链接".into());
    }
    if !shell_open(u) {
        return Err("系统无法打开该链接".into());
    }
    Ok("opened".into())
}

/// 读取 exe 旁 web.log 末尾（最大 256KB），用于失败排查
#[tauri::command]
pub(crate) fn read_web_log() -> (String, bool) {
    let path = web_log_path();
    if !path.is_file() {
        return (String::new(), false);
    }
    (tail_file(&path, 256 * 1024), true)
}

#[tauri::command]
pub(crate) fn get_web_url() -> String {
    web_url()
}

#[tauri::command]
pub(crate) async fn get_web_cmd() -> Result<String, String> {
    // find_bin 冷缓存时同步探测 npm（run_npm 已带 15 秒超时），仍须后台执行
    tauri::async_runtime::spawn_blocking(|| {
        let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
        Ok(format!("node \"{}\" web", bin))
    })
    .await
    .map_err(|e| e.to_string())?
}
