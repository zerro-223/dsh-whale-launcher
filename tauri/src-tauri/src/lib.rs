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
// 保证开源仓库不泄露个人信息）。
// 所有字段均为可选：webPort 默认 3080；junctionPath / dDrivePath 仅在
// 显式配置时作为 DSH 安装位置的候选之一，未配置时自动识别
// （npm 全局安装 / npx 缓存目录）。
// ---------------------------------------------------------------------------
#[derive(serde::Deserialize, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    junction_path: String,
    d_drive_path: String,
    web_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            junction_path: String::new(),
            d_drive_path: String::new(),
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

// ---------------------------------------------------------------------------
// DSH 安装位置自动识别
//
// 识别顺序（找到即返回，结果缓存；只缓存命中，未命中每次都重扫，
// 便于安装完成后立即被识别）：
//   1. settings.json 显式配置的 dDrivePath / junctionPath（向后兼容）
//   2. npm 全局安装（npm root -g 目录下的 @deepseek-ai\dsh）
//   3. npx 缓存目录扫描：npm 配置的 cache、默认 %LocalAppData%\npm-cache、
//      以及 settings 路径下的 _npx\<hash>（取最新含 DSH 的一个）
// 不再强制要求"挪到 D 盘 + junction"的布局，任何安装位置都能工作。
// ---------------------------------------------------------------------------
const DSH_PKG_NAME: &str = "@deepseek-ai/dsh";
const DSH_BIN_REL: &str = r"node_modules\@deepseek-ai\dsh\lib\bin.js";

#[derive(Clone)]
struct DshInstall {
    bin: String,      // lib\bin.js 完整路径
    pkg_root: String, // 安装根目录（node_modules 的父目录，如 _npx\<hash>）
    is_global: bool,  // 是否为 npm 全局安装
}

static DSH_CACHE: std::sync::Mutex<Option<DshInstall>> = std::sync::Mutex::new(None);

fn clear_dsh_cache() {
    if let Ok(mut c) = DSH_CACHE.lock() {
        *c = None;
    }
}

fn find_dsh() -> Option<DshInstall> {
    if let Ok(cache) = DSH_CACHE.lock() {
        if let Some(inst) = cache.as_ref() {
            return Some(inst.clone());
        }
    }
    let found = find_dsh_uncached();
    if found.is_some() {
        if let Ok(mut cache) = DSH_CACHE.lock() {
            *cache = found.clone();
        }
    }
    found
}

fn find_dsh_uncached() -> Option<DshInstall> {
    let s = settings();

    // 1. settings 显式路径
    for base in [&s.d_drive_path, &s.junction_path] {
        if base.is_empty() {
            continue;
        }
        let bin = format!(r"{}\{}", base, DSH_BIN_REL);
        if Path::new(&bin).is_file() {
            return Some(make_install(bin, false));
        }
    }

    // 2. npm 全局安装。注意：npm root -g 返回的即全局 node_modules 目录本身，
    //    不能再拼 node_modules\ 前缀（否则变成 node_modules\node_modules\...）
    if let Some(root) = npm_root_global() {
        let bin = format!(r"{}\@deepseek-ai\dsh\lib\bin.js", root);
        if Path::new(&bin).is_file() {
            return Some(make_install(bin, true));
        }
    }

    // 3. npx 缓存目录扫描
    let mut roots: Vec<String> = Vec::new();
    if let Some(cache) = npm_config_cache() {
        roots.push(cache);
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        roots.push(format!(r"{}\npm-cache", local));
    }
    for base in [&s.d_drive_path, &s.junction_path] {
        if !base.is_empty() && !roots.iter().any(|r| r.eq_ignore_ascii_case(base)) {
            roots.push(base.clone());
        }
    }
    for root in roots {
        if let Some(bin) = scan_npx_cache(&root) {
            return Some(make_install(bin, false));
        }
    }
    None
}

fn make_install(bin: String, is_global: bool) -> DshInstall {
    // bin = {root}\node_modules\@deepseek-ai\dsh\lib\bin.js，向上 5 级得 root
    let p = Path::new(&bin);
    let pkg_root = p.parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_default();
    DshInstall { bin, pkg_root, is_global }
}

fn npm_root_global() -> Option<String> {
    let out = run_npm(&["root", "-g"]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn npm_config_cache() -> Option<String> {
    let out = run_npm(&["config", "get", "cache"]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "undefined" { None } else { Some(s) }
}

/// 扫描 {root}\_npx\ 下所有哈希目录，取最新一个含 DSH 的
fn scan_npx_cache(root: &str) -> Option<String> {
    let npx_dir = Path::new(root).join("_npx");
    let entries = std::fs::read_dir(&npx_dir).ok()?;
    let mut hits: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let bin = path.join(DSH_BIN_REL);
        if bin.is_file() {
            let mtime = e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            hits.push((mtime, path));
        }
    }
    hits.sort_by_key(|(t, _)| *t);
    hits.last().map(|(_, p)| p.join(DSH_BIN_REL).to_string_lossy().to_string())
}

fn find_bin() -> Option<String> {
    find_dsh().map(|i| i.bin)
}

fn web_url() -> String {
    format!("http://127.0.0.1:{}", settings().web_port)
}

const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
// GUI 程序派生控制台子进程（npm/where/netstat 等）时默认会弹出一个新控制台窗口，
// 必须显式加 CREATE_NO_WINDOW 隐藏（TUI / Headless 用 CREATE_NEW_CONSOLE 保持可见）
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const NO_PROXY_DEFAULT: &str = "127.0.0.1,localhost,::1";

// ---------------------------------------------------------------------------
// 基础工具
// ---------------------------------------------------------------------------
fn is_running() -> bool {
    TcpStream::connect_timeout(&format!("127.0.0.1:{}", settings().web_port).parse().unwrap(),
                               Duration::from_millis(500)).is_ok()
}

fn find_dsh_pids(port: u16) -> Vec<u32> {
    let out = match Command::new("netstat").args(["-ano"]).creation_flags(CREATE_NO_WINDOW).output() {
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

/// 过滤出命令行包含 DSH bin 路径的进程（PowerShell CIM 查询）。
/// 防止 3080 端口被无关程序占用时 taskkill 误杀；
/// bin 为空或查询失败时回退为全部保留（旧行为），保证重启不会因无法验证而失效。
fn filter_dsh_pids(pids: &[u32], bin: &str) -> Vec<u32> {
    if pids.is_empty() || bin.is_empty() {
        return pids.to_vec();
    }
    let filter = pids
        .iter()
        .map(|p| format!("ProcessId={}", p))
        .collect::<Vec<_>>()
        .join(" or ");
    let ps = format!(
        "Get-CimInstance Win32_Process -Filter '{}' | Where-Object {{ $_.CommandLine -like '*{}*' }} | ForEach-Object {{ [int]$_.ProcessId }}",
        filter, bin
    );
    let out = match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return pids.to_vec(), // 查询失败：回退旧行为
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let matched: Vec<u32> = text
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|pid| pids.contains(pid))
        .collect();
    if matched.is_empty() {
        pids.to_vec() // 命令行均未匹配（如 DSH 已更新到新路径）：回退旧行为
    } else {
        matched
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
        .creation_flags(CREATE_NO_WINDOW)
        .status().map(|s| s.success()).unwrap_or(false);
    if let Ok(mut cache) = WHICH_CACHE.lock() {
        cache.insert(name.to_string(), found);
    }
    found
}

fn clear_which_cache() {
    if let Ok(mut cache) = WHICH_CACHE.lock() {
        cache.clear();
    }
}

// ---------------------------------------------------------------------------
// npm 调用辅助
//
// Windows 上 npm 只有 .cmd shim（无 npm.exe），CreateProcess 按扩展名解析时
// Command::new("npm") 会报 "program not found"（实测确认）。必须显式使用
// npm.cmd（std 会自动经 cmd /c 包装执行）；个别环境只有 npm.exe 时退回 npm。
// ---------------------------------------------------------------------------
fn npm_program() -> &'static str {
    if which("npm.cmd") {
        "npm.cmd"
    } else {
        "npm"
    }
}

fn run_npm(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(npm_program()).args(args).creation_flags(CREATE_NO_WINDOW).output()
}

fn npm_command() -> Command {
    let mut cmd = Command::new(npm_program());
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// 执行 npm 命令：stdout 逐行推送 progress，stderr 末尾收集到错误信息。
/// 非零退出码返回 Err（带 stderr 末尾），不再静默吞掉 npm 失败。
fn run_npm_cmd(cmd: &mut Command, progress: &Channel<String>) -> Result<(), String> {
    use std::io::BufRead;
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("npm 启动失败：{}", e))?;
    let out = child.stdout.take().expect("piped stdout");
    let err = child.stderr.take().expect("piped stderr");
    // 同时读取 stdout/stderr，避免单管道缓冲写满导致死锁
    let progress = progress.clone();
    let t_out = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(out).lines() {
            if let Ok(l) = line {
                let _ = progress.send(l.clone());
                lines.push(l);
            }
        }
        lines
    });
    let t_err = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(err).lines() {
            if let Ok(l) = line {
                lines.push(l);
            }
        }
        lines
    });
    let (out_lines, err_lines) = (
        t_out.join().unwrap_or_default(),
        t_err.join().unwrap_or_default(),
    );
    let status = child.wait().map_err(|e| format!("npm 等待失败：{}", e))?;
    if status.success() {
        return Ok(());
    }
    let tail = |lines: &[String], n: usize| -> String {
        lines.iter().rev().take(n).rev().cloned().collect::<Vec<_>>().join("\n")
    };
    let e = tail(&err_lines, 15);
    let o = tail(&out_lines, 5);
    let mut msg = format!("npm 执行失败（退出码 {}）", status);
    if !e.is_empty() {
        msg.push_str(&format!("\n--- 错误输出末尾 ---\n{}", e));
    } else if !o.is_empty() {
        msg.push_str(&format!("\n--- 输出末尾 ---\n{}", o));
    }
    Err(msg)
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

    match find_dsh() {
        Some(inst) => items.push(CheckItem { name: "DSH 程序文件".into(), status: "OK".into(), detail: format!("已识别安装位置：{}", inst.pkg_root) }),
        None => items.push(CheckItem { name: "DSH 程序文件".into(), status: "FAIL".into(), detail: "未找到 DSH，请点击右上角「未安装 DSH」一键安装，或手动执行：npm install -g @deepseek-ai/dsh".into() }),
    }

    // npm 缓存仅作信息展示，不再强制要求指向特定盘符
    match npm_config_cache() {
        Some(cache) => items.push(CheckItem { name: "npm 缓存".into(), status: "INFO".into(), detail: format!("位于 {}", cache) }),
        None => items.push(CheckItem { name: "npm 缓存".into(), status: "WARN".into(), detail: "无法读取 npm 缓存路径".into() }),
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
    let install = find_dsh()?;
    let p = Path::new(&install.pkg_root).join(r"node_modules\@deepseek-ai\dsh\package.json");
    let content = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("version").and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// npm 专用的代理环境：npm 不读 HTTP_PROXY 环境变量（与 node 不同），
/// 必须用 npm_config_proxy / npm_config_https_proxy 显式指定，否则开了
/// 代理的机器上 npm install / npm view 仍然直连导致失败。
fn npm_proxy_env(proxy_on: bool, proxy_addr: &str) -> Vec<(String, String)> {
    let mut env = proxy_env_or_none(proxy_on, proxy_addr);
    if !env.is_empty() {
        let (http, https) = parse_proxy_server(proxy_addr);
        if let Some(h) = normalize_proxy_url(http.as_deref().or(https.as_deref()).unwrap_or("")) {
            env.push(("npm_config_proxy".into(), h.clone()));
            env.push(("npm_config_https_proxy".into(), h));
        }
    }
    env
}

/// 查询 npm registry 上的最新版本。失败时返回具体原因（含 npm stderr 末尾），
/// 并带 30 秒超时保护（npm fetch-timeout 默认 5 分钟，registry 挂起时
/// 不能无限等待）。
fn get_latest_dsh_version(env: &[(String, String)]) -> Result<String, String> {
    let mut cmd = Command::new(npm_program());
    cmd.args(["view", DSH_PKG_NAME, "version"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("npm 启动失败：{}", e))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("npm view 超时（30 秒），registry 响应过慢".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let out = child.wait_with_output().map_err(|e| format!("读取 npm 输出失败：{}", e))?;
    if !out.status.success() {
        let stderr_text = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<&str> = stderr_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let tail = lines.iter().rev().take(8).rev().cloned().collect::<Vec<_>>().join("\n");
        return Err(if tail.is_empty() {
            format!("npm view 失败（退出码 {}）", out.status)
        } else {
            format!("npm view 失败（退出码 {}）：\n{}", out.status, tail)
        });
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .last()
        .map(|s| s.to_string())
        .ok_or_else(|| "npm view 无输出".into())
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

/// 在新建控制台窗口（TUI 等）中启动 DSH：输出直接显示在窗口里，无需日志
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
// Web 模式启动：输出写入 web.log（exe 旁），启动失败 / 端口未就绪时
// 回读日志末尾给出错误信息（不再静默吞掉）
// ---------------------------------------------------------------------------
fn web_log_path() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("web.log");
        }
    }
    std::env::current_dir().unwrap_or_default().join("web.log")
}

fn tail_file(path: &std::path::Path, max_bytes: usize) -> String {
    match std::fs::read(path) {
        Ok(data) => {
            let start = data.len().saturating_sub(max_bytes);
            String::from_utf8_lossy(&data[start..]).to_string()
        }
        Err(_) => String::new(),
    }
}

fn append_line(path: &std::path::Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(f, "{}", line);
    }
}

/// 打开 web.log（追加模式）；日志超过 1MB 时先截断，防止无限增长
fn open_web_log() -> Result<std::fs::File, std::io::Error> {
    let path = web_log_path();
    if std::fs::metadata(&path).map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::OpenOptions::new().write(true).truncate(true).open(&path);
    }
    std::fs::OpenOptions::new().create(true).append(true).open(path)
}

/// 启动 Web 模式：stdout/stderr 重定向到 web.log，随后等待端口就绪
/// （最多 8 秒）。进程提前退出或超时未就绪时，回读日志末尾并返回
/// 带上下文的错误信息。
fn spawn_web(args: &[&str], env: &[(String, String)]) -> Result<String, String> {
    let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
    let path = web_log_path();
    let log = open_web_log().map_err(|e| format!("无法创建日志文件 {}：{}", path.display(), e))?;
    let mut cmd = Command::new("node");
    cmd.arg(&bin).args(args)
        .creation_flags(CREATE_NO_WINDOW) // Web 模式后台运行，不弹控制台窗口
        .stdout(Stdio::from(log.try_clone().map_err(|e| format!("日志文件错误：{}", e))?))
        .stderr(Stdio::from(log));
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().map_err(|e| format!("启动失败：{}", e))?;
    append_line(&path, &format!("==== DSH web 启动（PID {}）====", child.id()));

    // 等待端口就绪；进程提前退出或超时则回读日志末尾
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if is_running() {
            return Ok("started".to_string());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = tail_file(&path, 8192).trim().to_string();
            if tail.is_empty() {
                return Err(format!("DSH 启动后立即退出（退出码 {}），web.log 无输出", status));
            }
            return Err(format!("DSH 启动失败（退出码 {}）\n\n--- web.log 末尾 ---\n{}", status, tail));
        }
        if std::time::Instant::now() >= deadline {
            let tail = tail_file(&path, 8192).trim().to_string();
            if tail.is_empty() {
                return Err("DSH 进程已启动，但 8 秒内端口未就绪（web.log 无输出）".to_string());
            }
            return Err(format!(
                "DSH 进程已启动，但 8 秒内端口 {} 未就绪\n\n--- web.log 末尾 ---\n{}",
                settings().web_port, tail
            ));
        }
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------
// 注意：同步命令会在 Tauri 主线程执行，凡是涉及子进程/网络/端口探测的
// 必须写成 async + spawn_blocking，否则会卡死 UI（曾导致打开后自检卡几秒）。

#[tauri::command]
async fn checks() -> ChecksResult {
    tauri::async_runtime::spawn_blocking(|| {
        clear_dsh_cache(); // 自检前刷新识别缓存（应对运行期间的外部安装/卸载）
        clear_which_cache(); // 刷新 PATH 探测缓存（应对运行期间新装的 node/npm）
        run_checks()
    })
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
        spawn_web(&["web"], &env)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn open_browser() {
    let _ = Command::new("cmd").args(["/c", "start", "", web_url().as_str()])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
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
        // 用 PowerShell 而不是 cmd /k：任务文本直接拼进 cmd 命令行时，
        // 含 " % & | < > 等字符会被 cmd 二次解析（参数错乱甚至命令注入），
        // 含空格的英文任务也会被拆成多个参数。PowerShell 单引号字符串
        // 仅需把 ' 翻倍即可安全传递任意文本。
        // -NoExit 保持窗口：headless 打印完进程即退出，否则窗口秒关看不到回答
        let script = format!(
            "& node '{}' --profile headless '{}'",
            bin.replace('\'', "''"),
            task.replace('\'', "''"),
        );
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoExit", "-Command", &script])
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
        // 仅终止命令行包含 DSH bin 路径的进程，避免 3080 被无关程序占用时误杀
        let bin = find_bin().unwrap_or_default();
        for pid in filter_dsh_pids(&pids, &bin) {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .creation_flags(CREATE_NO_WINDOW)
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
        spawn_web(&["web"], &env)?;
        Ok("ok".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn update_check(proxy_on: bool, proxy_addr: String) -> (Option<String>, Option<String>, Option<String>) {
    // npm view 需要访问网络 registry（可能 2~5 秒），必须在后台线程执行。
    // 返回值第三项为错误信息：把「已是最新」与「无法连接 registry」区分开；
    // 按前端配置注入代理（npm 需 npm_config_* 变量），失败时返回 npm 真实报错。
    tauri::async_runtime::spawn_blocking(move || {
        clear_dsh_cache(); // 检查前刷新识别缓存（应对运行期间的外部安装/卸载）
        let installed = get_installed_dsh_version();
        let env = npm_proxy_env(proxy_on, &proxy_addr);
        match get_latest_dsh_version(&env) {
            Ok(latest) => (installed, Some(latest), None),
            Err(e) => (installed, None, Some(e)),
        }
    })
    .await
    .unwrap_or((None, None, Some("更新检查异常".to_string())))
}

#[tauri::command]
async fn update_dsh(proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<(String, String), String> {
    let install = find_dsh().ok_or_else(|| "未安装 DSH，请先点击「未安装 DSH」一键安装".to_string())?;
    let old = get_installed_dsh_version().unwrap_or_default();
    let is_global = install.is_global;
    let pkg_root = install.pkg_root.clone();
    let env = npm_proxy_env(proxy_on, &proxy_addr); // npm 不读 HTTP_PROXY，需 npm_config_*
    tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = npm_command();
        let pkg = format!("{}@latest", DSH_PKG_NAME);
        if is_global {
            // 全局安装：npm install -g
            cmd.args(["install", "-g", &pkg, "--no-audit", "--no-fund"]);
        } else {
            // npx 缓存 / 本地安装：在安装根目录执行 npm install
            cmd.args(["install", &pkg, "--no-audit", "--no-fund"])
                .current_dir(&pkg_root);
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        run_npm_cmd(&mut cmd, &progress)?;
        clear_dsh_cache();
        let new = get_installed_dsh_version().unwrap_or_default();
        if new.is_empty() {
            return Err("npm 已执行完成，但更新后未能识别到 DSH 版本号".to_string());
        }
        Ok::<(String, String), String>((old, new))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn install_dsh(proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<String, String> {
    let env = npm_proxy_env(proxy_on, &proxy_addr); // npm 不读 HTTP_PROXY，需 npm_config_*
    tauri::async_runtime::spawn_blocking(move || {
        let mut cmd = npm_command();
        cmd.args(["install", "-g", &format!("{}@latest", DSH_PKG_NAME), "--no-audit", "--no-fund"]);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        run_npm_cmd(&mut cmd, &progress)?;
        clear_dsh_cache();
        // 安装后立即重新识别并返回版本号
        get_installed_dsh_version().ok_or_else(|| {
            "npm 安装已执行成功，但未识别到 DSH 程序文件，请检查 npm 输出".to_string()
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_system_proxy_cmd() -> (bool, String) {
    get_system_proxy()
}

#[tauri::command]
fn fix_commands() -> Vec<String> {
    let mut cmds = Vec::new();
    if !which("node") {
        cmds.push("请先安装 Node.js（官网下载 LTS 版）：https://nodejs.org/".to_string());
    }
    if find_dsh().is_none() {
        cmds.push("npm install -g @deepseek-ai/dsh".to_string());
    }
    cmds
}

#[tauri::command]
fn open_install_dir() -> String {
    match find_dsh() {
        Some(inst) => {
            let _ = Command::new("cmd").args(["/c", "start", "", &inst.pkg_root])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
            format!("已打开 {}", inst.pkg_root)
        }
        None => "未找到 DSH 安装目录（尚未安装）".into(),
    }
}

#[tauri::command]
fn get_web_url() -> String {
    web_url()
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
            restart_dsh, update_check, update_dsh, install_dsh, get_system_proxy_cmd,
            fix_commands, open_install_dir, get_web_cmd, get_web_url
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
