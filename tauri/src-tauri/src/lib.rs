//! DSH 启动器（Tauri 2 / Rust 后端）
//!
//! 所有阻塞性操作（端口探测、npm、netstat、子进程）都在 Rust 侧执行，
//! 前端只负责展示与调用；异步命令使用 tauri 自带运行时，不阻塞 UI。

use serde::Serialize;
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tauri::ipc::Channel;

// ---------------------------------------------------------------------------
// 配置：从 exe 旁的 settings.json 读取（避免源码硬编码用户环境路径，
// 保证开源仓库不泄露个人信息）。缺省值为空，需按本机环境配置。
// ---------------------------------------------------------------------------
#[derive(serde::Deserialize, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    junction_path: String,
    d_drive_path: String,
    npmrc_path: String,
    web_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            junction_path: String::new(),
            d_drive_path: String::new(),
            npmrc_path: String::new(),
            web_port: 3080,
        }
    }
}

static SETTINGS: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();

fn settings() -> &'static Settings {
    SETTINGS.get_or_init(load_settings)
}

fn load_settings() -> Settings {
    // 查找顺序：exe 旁 -> 当前工作目录
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("settings.json"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("settings.json"));
    }
    for path in candidates {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(s) = serde_json::from_str::<Settings>(&content) {
                return s;
            }
        }
    }
    Settings::default()
}

fn dsh_pkg_dir() -> String {
    format!(r"{}\node_modules\@deepseek-ai\dsh", settings().d_drive_path)
}

fn bin_rel() -> &'static str {
    r"node_modules\@deepseek-ai\dsh\lib\bin.js"
}

fn web_url() -> String {
    format!("http://127.0.0.1:{}", settings().web_port)
}

fn fix_junction_cmd() -> String {
    format!(
        r#"mklink /J "{}" "{}""#,
        settings().junction_path, settings().d_drive_path
    )
}

const DSH_PKG_NAME: &str = "@deepseek-ai/dsh";
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
const NO_PROXY_DEFAULT: &str = "127.0.0.1,localhost,::1";

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------
fn find_bin() -> Option<String> {
    let s = settings();
    for base in [&s.d_drive_path, &s.junction_path] {
        if base.is_empty() {
            continue;
        }
        let p = format!(r"{}\{}", base, bin_rel());
        if Path::new(&p).is_file() {
            return Some(p);
        }
    }
    None
}

fn is_running() -> bool {
    TcpStream::connect_timeout(&format!("127.0.0.1:{}", settings().web_port).parse().unwrap(),
                               Duration::from_millis(500)).is_ok()
}

fn find_dsh_pids(port: u16) -> Vec<u32> {
    let out = match Command::new("netstat").args(["-ano"]).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Vec::new(),
    };
    let target = format!(":{}", port);
    let mut pids = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 5 && parts.contains(&"LISTENING")
            && parts[..parts.len() - 1].iter().any(|p| p.contains(&target))
        {
            if let Ok(pid) = parts[parts.len() - 1].parse::<u32>() {
                if !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
    }
    pids
}

fn junction_ok() -> bool {
    // read_link 直接读取 junction 存储的目标（无 \\?\ 前缀，canonicalize 会返回
    // verbatim 路径导致前缀比较失败，误报 FAIL）
    if settings().junction_path.is_empty() || settings().d_drive_path.is_empty() {
        return false;
    }
    match std::fs::read_link(&settings().junction_path) {
        Ok(target) => {
            let t = target.to_string_lossy().to_lowercase();
            let t = t.trim_end_matches('\\');
            let d = settings().d_drive_path.trim_end_matches('\\').to_lowercase();
            t == d || t.starts_with(&(d.clone() + "\\"))
        }
        Err(_) => false,
    }
}

fn npmrc_cache_ok() -> bool {
    if settings().npmrc_path.is_empty() {
        return false;
    }
    match std::fs::read_to_string(&settings().npmrc_path) {
        Ok(content) => content.lines().any(|l| {
            let l = l.trim();
            l.starts_with("cache=") && l[6..].trim().eq_ignore_ascii_case(r"d:\npm-cache")
        }),
        Err(_) => false,
    }
}

fn get_system_proxy() -> (bool, String) {
    use windows_registry::*;
    match CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings") {
        Ok(key) => {
            let enable = key.get_u32("ProxyEnable").unwrap_or(0) != 0;
            let server = key.get_string("ProxyServer").unwrap_or_default();
            (enable, server)
        }
        Err(_) => (false, String::new()),
    }
}

fn parse_proxy_server(server: &str) -> (Option<String>, Option<String>) {
    if server.contains('=') {
        let mut http = None;
        let mut https = None;
        for part in server.split(';') {
            let part = part.trim();
            if let Some(eq) = part.find('=') {
                let (scheme, addr) = part.split_at(eq);
                let addr = addr[1..].trim();
                match scheme.trim().to_lowercase().as_str() {
                    "http" => http = Some(addr.to_string()),
                    "https" => https = Some(addr.to_string()),
                    _ => {}
                }
            }
        }
        (http, https)
    } else {
        let s = server.trim().to_string();
        if s.is_empty() {
            (None, None)
        } else {
            (Some(s.clone()), Some(s))
        }
    }
}

fn normalize_proxy_url(addr: &str) -> Option<String> {
    let a = addr.trim();
    if a.is_empty() {
        return None;
    }
    if a.contains("://") {
        Some(a.to_string())
    } else {
        Some(format!("http://{}", a))
    }
}

fn build_proxy_env(server: &str) -> Vec<(String, String)> {
    let (http, https) = parse_proxy_server(server);
    if http.is_none() && https.is_none() {
        return Vec::new();
    }
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(h) = &http {
        if let Some(u) = normalize_proxy_url(h) {
            env.push(("HTTP_PROXY".into(), u.clone()));
            env.push(("http_proxy".into(), u));
        }
    }
    if let Some(h) = &https {
        if let Some(u) = normalize_proxy_url(h) {
            env.push(("HTTPS_PROXY".into(), u.clone()));
            env.push(("https_proxy".into(), u));
        }
    }
    let all = normalize_proxy_url(http.as_deref().or(https.as_deref()).unwrap_or(""));
    if let Some(a) = all {
        env.push(("ALL_PROXY".into(), a));
    }
    env.push(("NO_PROXY".into(), NO_PROXY_DEFAULT.into()));
    env.push(("no_proxy".into(), NO_PROXY_DEFAULT.into()));
    // 关键开关：Node 的 fetch 默认忽略环境变量代理
    env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
    env
}

// which 结果缓存：PATH 扫描（where）较慢，避免每次自检都跑子进程
static WHICH_CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn which(name: &str) -> bool {
    if let Ok(cache) = WHICH_CACHE.lock() {
        if let Some(v) = cache.get(name) {
            return *v;
        }
    }
    let found = Command::new("where").arg(name).stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false);
    if let Ok(mut cache) = WHICH_CACHE.lock() {
        cache.insert(name.to_string(), found);
    }
    found
}

// ---------------------------------------------------------------------------
// 自检
// ---------------------------------------------------------------------------
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CheckItem {
    name: String,
    status: String, // OK / FAIL / WARN / INFO
    detail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChecksResult {
    items: Vec<CheckItem>,
    running: bool,
}

fn run_checks() -> ChecksResult {
    let mut items = Vec::new();

    if which("node") {
        items.push(CheckItem { name: "Node.js 环境".into(), status: "OK".into(), detail: "已找到 node".into() });
    } else {
        items.push(CheckItem { name: "Node.js 环境".into(), status: "FAIL".into(), detail: "未找到 node，请先安装 Node.js".into() });
    }

    match find_bin() {
        Some(_) => items.push(CheckItem { name: "DSH 程序文件".into(), status: "OK".into(), detail: format!("位于 {}", settings().d_drive_path) }),
        None => items.push(CheckItem { name: "DSH 程序文件".into(), status: "FAIL".into(), detail: format!("{} 下未找到 dsh CLI", settings().d_drive_path) }),
    }

    if junction_ok() {
        items.push(CheckItem { name: "目录联接 junction".into(), status: "OK".into(), detail: "原路径已指向 D 盘".into() });
    } else {
        items.push(CheckItem { name: "目录联接 junction".into(), status: "FAIL".into(), detail: format!("原路径不可用，需重建：\n    {}", fix_junction_cmd()) });
    }

    if npmrc_cache_ok() {
        items.push(CheckItem { name: "npm 缓存配置".into(), status: "OK".into(), detail: "cache 已指向 D 盘".into() });
    } else {
        items.push(CheckItem { name: "npm 缓存配置".into(), status: "WARN".into(), detail: "~/.npmrc 未配置 cache=D:\\npm-cache".into() });
    }

    let (enabled, server) = get_system_proxy();
    if enabled && !server.is_empty() {
        items.push(CheckItem { name: "系统代理".into(), status: "INFO".into(), detail: format!("已启用：{}", server) });
    } else {
        items.push(CheckItem { name: "系统代理".into(), status: "INFO".into(), detail: "系统代理未启用".into() });
    }

    let running = is_running();
    items.push(CheckItem {
        name: "运行状态".into(),
        status: "INFO".into(),
        detail: if running { format!("DSH 正在运行：{}", web_url()) } else { "DSH 未运行，可点击上方按钮启动".into() },
    });

    ChecksResult { items, running }
}

// ---------------------------------------------------------------------------
// 版本检测与更新
// ---------------------------------------------------------------------------
fn get_installed_dsh_version() -> Option<String> {
    let p = format!(r"{}\package.json", dsh_pkg_dir());
    let content = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("version").and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn get_latest_dsh_version() -> Option<String> {
    let out = Command::new("npm").args(["view", DSH_PKG_NAME, "version"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).last().map(|s| s.to_string())
}

fn version_tuple(v: &str) -> (u64, u64, u64) {
    let mut nums = [0u64; 3];
    for (i, part) in v.splitn(3, '.').enumerate() {
        if i < 3 {
            nums[i] = part.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
        }
    }
    (nums[0], nums[1], nums[2])
}

fn version_gt(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let (ta, tb) = (version_tuple(a), version_tuple(b));
    if ta != tb {
        return ta > tb;
    }
    let (ra, rb) = (a.contains('-'), b.contains('-'));
    if ra != rb {
        return rb;
    }
    a > b
}

// ---------------------------------------------------------------------------
// 子进程启动（统一处理代理环境）
// ---------------------------------------------------------------------------
fn proxy_env_or_none(proxy_on: bool, proxy_addr: &str) -> Vec<(String, String)> {
    if !proxy_on {
        return Vec::new();
    }
    build_proxy_env(proxy_addr)
}

fn spawn_node(args: &[&str], new_console: bool, env: &[(String, String)]) -> Result<(), String> {
    let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
    let mut cmd = Command::new("node");
    cmd.arg(&bin).args(args)
        .stdout(Stdio::null()).stderr(Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if new_console {
        cmd.creation_flags(CREATE_NEW_CONSOLE);
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("启动失败：{}", e))
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------
// 注意：同步命令会在 Tauri 主线程执行，凡是涉及子进程/网络/端口探测的
// 必须写成 async + spawn_blocking，否则会卡死 UI（曾导致打开后自检卡几秒）。

#[tauri::command]
async fn checks() -> ChecksResult {
    tauri::async_runtime::spawn_blocking(run_checks)
        .await
        .unwrap_or(ChecksResult { items: Vec::new(), running: false })
}

#[tauri::command]
async fn status() -> bool {
    tauri::async_runtime::spawn_blocking(is_running).await.unwrap_or(false)
}

#[tauri::command]
async fn start_web(proxy_on: bool, proxy_addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if is_running() {
            return Ok("already-running".to_string());
        }
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        spawn_node(&["web"], false, &env)?;
        Ok("started".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn open_browser() {
    let _ = Command::new("cmd").args(["/c", "start", "", web_url().as_str()]).spawn();
}

#[tauri::command]
async fn start_tui(proxy_on: bool, proxy_addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        spawn_node(&["--profile", "tui"], true, &env)?;
        Ok("started".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn start_headless(task: String, proxy_on: bool, proxy_addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
        let mut cmd = Command::new("cmd");
        // cmd /k 保持窗口：headless 打印完进程即退出，否则窗口秒关看不到回答
        cmd.args(["/k", "node", &bin, "--profile", "headless", &task])
            .creation_flags(CREATE_NEW_CONSOLE)
            .stdout(Stdio::null()).stderr(Stdio::null());
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.spawn().map(|_| "started".to_string()).map_err(|e| format!("启动失败：{}", e))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn restart_dsh(proxy_on: bool, proxy_addr: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pids = find_dsh_pids(settings().web_port);
        for pid in &pids {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null()).stderr(Stdio::null())
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
            return Err("still-running".into());
        }
        let env = proxy_env_or_none(proxy_on, &proxy_addr);
        spawn_node(&["web"], false, &env)?;
        Ok("ok".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn update_check() -> (Option<String>, Option<String>) {
    // npm view 需要访问网络 registry（可能 2~5 秒），必须在后台线程执行
    tauri::async_runtime::spawn_blocking(|| {
        (get_installed_dsh_version(), get_latest_dsh_version())
    })
    .await
    .unwrap_or((None, None))
}

#[tauri::command]
async fn update_dsh(proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<(String, String), String> {
    let old = get_installed_dsh_version().unwrap_or_default();
    let env = proxy_env_or_none(proxy_on, &proxy_addr);
    tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = Command::new("npm");
        cmd.args(["install", &format!("{}@latest", DSH_PKG_NAME), "--no-audit", "--no-fund"])
            .current_dir(&settings().d_drive_path);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn()
            .map_err(|e| format!("npm 启动失败：{}", e))?;
        if let Some(mut out) = child.stdout.take() {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(out);
            for line in reader.lines() {
                if let Ok(l) = line {
                    let _ = progress.send(l);
                }
            }
        }
        let _ = child.wait();
        let new = get_installed_dsh_version().unwrap_or_default();
        Ok::<(String, String), String>((old, new))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_system_proxy_cmd() -> (bool, String) {
    get_system_proxy()
}

#[tauri::command]
fn junction_ok_cmd() -> bool {
    junction_ok()
}

#[tauri::command]
fn npmrc_ok_cmd() -> bool {
    npmrc_cache_ok()
}

#[tauri::command]
fn fix_commands() -> Vec<String> {
    let mut cmds = Vec::new();
    if !junction_ok() {
        cmds.push(fix_junction_cmd());
    }
    if !npmrc_cache_ok() {
        cmds.push(r#"npm config set cache "D:\npm-cache""#.to_string());
    }
    cmds
}

#[tauri::command]
fn open_install_dir() -> String {
    let s = settings();
    for base in [&s.d_drive_path, &s.junction_path] {
        if !base.is_empty() && Path::new(base).is_dir() {
            let _ = Command::new("cmd").args(["/c", "start", "", base]).spawn();
            return format!("已打开 {}", base);
        }
    }
    "未找到 DSH 安装目录".into()
}

#[tauri::command]
fn get_web_cmd() -> Result<String, String> {
    let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
    Ok(format!("node \"{}\" web", bin))
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            checks, status, start_web, open_browser, start_tui, start_headless,
            restart_dsh, update_check, update_dsh, get_system_proxy_cmd,
            fix_commands, open_install_dir, get_web_cmd
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
