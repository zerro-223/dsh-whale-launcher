//! 子进程执行工具：which 探测缓存、npm 调用辅助、流式命令执行与 npm view。

use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use tauri::ipc::Channel;

use crate::settings::settings;
use crate::util::{lock_ok, CREATE_NO_WINDOW};

// which 结果缓存：PATH 扫描（where）较慢，避免每次自检都跑子进程
static WHICH_CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub(crate) fn which(name: &str) -> bool {
    if let Some(v) = lock_ok(&WHICH_CACHE).get(name) {
        return *v;
    }
    let found = Command::new("where")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    lock_ok(&WHICH_CACHE).insert(name.to_string(), found);
    found
}

pub(crate) fn clear_which_cache() {
    lock_ok(&WHICH_CACHE).clear();
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

/// 为 npm / pnpm 命令注入配置的 registry 镜像（npm_config_registry 两者都读）
pub(crate) fn apply_registry(cmd: &mut Command) {
    let r = settings().registry;
    if !r.is_empty() {
        cmd.env("npm_config_registry", r);
    }
}

/// 执行 npm 快速查询（root -g / config get cache），带 15 秒超时。
/// 这类查询可能被同步命令路径调用，npm 挂起时不能无限等待拖死调用方；
/// npm install 等长任务走 run_npm_cmd（流式进度，不做硬超时）。
/// stdout/stderr 由独立线程持续排空：轮询等待期间若输出超过管道缓冲
/// （64KB）会令子进程阻塞、永远等不到退出（与 npm_view_version 同款防护）。
pub(crate) fn run_npm(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new(npm_program());
    cmd.args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_registry(&mut cmd);
    let mut child = cmd.spawn()?;
    let out_pipe = child.stdout.take().expect("piped stdout");
    let err_pipe = child.stderr.take().expect("piped stderr");
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = BufReader::new(out_pipe).read_to_end(&mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = BufReader::new(err_pipe).read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = t_out.join();
                let _ = t_err.join();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "npm 查询超时（15 秒）",
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = t_out.join();
                let _ = t_err.join();
                return Err(e);
            }
        }
    };
    Ok(std::process::Output {
        status,
        stdout: t_out.join().unwrap_or_default(),
        stderr: t_err.join().unwrap_or_default(),
    })
}

pub(crate) fn npm_command() -> Command {
    let mut cmd = Command::new(npm_program());
    cmd.creation_flags(CREATE_NO_WINDOW);
    apply_registry(&mut cmd);
    cmd
}

/// 执行命令：stdout 逐行推送 progress，stderr 末尾收集到错误信息。
/// 非零退出码返回 Err（带 stderr 末尾），不再静默吞掉失败。
/// 两侧输出只保留末尾 TAIL_KEEP 行：npm/pnpm 输出可能极大，
/// 错误展示只用末尾几行，全量保存会无谓占用内存。
pub(crate) fn run_cmd_streaming(
    cmd: &mut Command,
    progress: &Channel<String>,
    op_name: &str,
) -> Result<(), String> {
    const TAIL_KEEP: usize = 200;
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{} 启动失败：{}", op_name, e))?;
    let out = child.stdout.take().expect("piped stdout");
    let err = child.stderr.take().expect("piped stderr");
    // 同时读取 stdout/stderr，避免单管道缓冲写满导致死锁
    let progress = progress.clone();
    let t_out = std::thread::spawn(move || {
        let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let _ = progress.send(line.clone());
            if lines.len() == TAIL_KEEP {
                lines.pop_front();
            }
            lines.push_back(line);
        }
        lines
    });
    let t_err = std::thread::spawn(move || {
        let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if lines.len() == TAIL_KEEP {
                lines.pop_front();
            }
            lines.push_back(line);
        }
        lines
    });
    let deadline = std::time::Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = t_out.join();
                let _ = t_err.join();
                return Err(format!(
                    "{} 超时（{} 分钟），已终止子进程",
                    op_name,
                    COMMAND_TIMEOUT.as_secs() / 60
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = t_out.join();
                let _ = t_err.join();
                return Err(format!("{} 等待失败：{}", op_name, e));
            }
        }
    };
    let (out_lines, err_lines) = (
        t_out.join().unwrap_or_default(),
        t_err.join().unwrap_or_default(),
    );
    if status.success() {
        return Ok(());
    }
    let tail = |lines: &std::collections::VecDeque<String>, n: usize| -> String {
        lines
            .iter()
            .rev()
            .take(n)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let e = tail(&err_lines, 15);
    let o = tail(&out_lines, 5);
    let mut msg = format!("{} 执行失败（退出码 {}）", op_name, status);
    if !e.is_empty() {
        msg.push_str(&format!("\n--- 错误输出末尾 ---\n{}", e));
    } else if !o.is_empty() {
        msg.push_str(&format!("\n--- 输出末尾 ---\n{}", o));
    }
    Err(msg)
}

pub(crate) fn run_npm_cmd(cmd: &mut Command, progress: &Channel<String>) -> Result<(), String> {
    run_cmd_streaming(cmd, progress, "npm")
}

/// 查询 npm registry 上指定包的最新版本。失败时返回具体原因（含 npm stderr 末尾），
/// 并带 30 秒超时保护（npm fetch-timeout 默认 5 分钟，registry 挂起时
/// 不能无限等待）。stdout/stderr 由独立线程持续排空：轮询等待期间若输出
/// 超过管道缓冲（64KB）会令子进程阻塞，旧实现只读不排会假超时。
pub(crate) fn npm_view_version(pkg: &str, env: &[(String, String)]) -> Result<String, String> {
    let mut cmd = Command::new(npm_program());
    cmd.args(["view", pkg, "version"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    apply_registry(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("npm 启动失败：{}", e))?;
    let out_pipe = child.stdout.take().expect("piped stdout");
    let err_pipe = child.stderr.take().expect("piped stderr");
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = BufReader::new(out_pipe).read_to_end(&mut buf);
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = BufReader::new(err_pipe).read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                status = st;
                break;
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = t_out.join();
                let _ = t_err.join();
                return Err(format!("npm 等待失败：{}", e));
            }
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = t_out.join();
            let _ = t_err.join();
            return Err("npm view 超时（30 秒），registry 响应过慢".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let out_bytes = t_out.join().unwrap_or_default();
    let err_bytes = t_err.join().unwrap_or_default();
    if !status.success() {
        let stderr_text = String::from_utf8_lossy(&err_bytes);
        let lines: Vec<&str> = stderr_text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        let tail = lines
            .iter()
            .rev()
            .take(8)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        return Err(if tail.is_empty() {
            format!("npm view 失败（退出码 {}）", status)
        } else {
            format!("npm view 失败（退出码 {}）：\n{}", status, tail)
        });
    }
    String::from_utf8_lossy(&out_bytes)
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| "npm view 无输出".into())
}
