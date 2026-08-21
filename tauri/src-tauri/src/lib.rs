//! DSH 启动器（Tauri 2 / Rust 后端）
//!
//! 所有阻塞性操作（端口探测、npm、netstat、子进程）都在 Rust 侧执行，
//! 前端只负责展示与调用；异步命令使用 tauri 自带运行时，不阻塞 UI。

use serde::Serialize;
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::ipc::Channel;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;

// ---------------------------------------------------------------------------
// 配置：从 exe 旁的 settings.json 读取（避免源码硬编码用户环境路径，
// 保证开源仓库不泄露个人信息）。
// 字段均为可选：
//   - webPort 默认 3080；junctionPath / dDrivePath 仅在显式配置时作为
//     DSH 安装位置的候选之一，未配置时自动识别（npm 全局 / npx 缓存）
//   - pluginProfile 插件管理目标 profile，默认 web（可在设置页修改）
//   - registry npm registry 镜像地址，空 = 官方源（可在设置页修改）
//   - closeAction 点击关闭按钮的行为：tray（隐藏到托盘）/ quit（直接退出）
// ---------------------------------------------------------------------------
#[derive(serde::Deserialize, serde::Serialize, Clone)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    junction_path: String,
    d_drive_path: String,
    web_port: u16,
    plugin_profile: String,
    registry: String,
    close_action: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            junction_path: String::new(),
            d_drive_path: String::new(),
            web_port: 3080,
            plugin_profile: "web".into(),
            registry: String::new(),
            close_action: "tray".into(),
        }
    }
}

// 运行时可变（设置页保存后更新），不再用 OnceLock
static SETTINGS: std::sync::Mutex<Option<Settings>> = std::sync::Mutex::new(None);

/// 容忍 Mutex 中毒的加锁：某次 panic 污染后仍可继续工作（数据本身无副作用）
fn lock_ok<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// exe 所在目录（进程生命周期内不变，缓存避免热路径反复执行 current_exe 系统调用）
static EXE_DIR: std::sync::LazyLock<Option<PathBuf>> = std::sync::LazyLock::new(|| {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf))
});

/// exe 旁路径；取不到 exe 目录时回退当前工作目录
fn beside_exe(name: &str) -> PathBuf {
    match EXE_DIR.as_ref() {
        Some(dir) => dir.join(name),
        None => std::env::current_dir().unwrap_or_default().join(name),
    }
}

fn settings() -> Settings {
    let mut g = lock_ok(&SETTINGS);
    if g.is_none() {
        *g = Some(load_settings());
    }
    g.clone().unwrap()
}

fn update_settings(s: Settings) {
    *lock_ok(&SETTINGS) = Some(s);
}

fn load_settings() -> Settings {
    // 查找顺序：exe 旁 -> 当前工作目录
    let mut candidates = vec![beside_exe("settings.json")];
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

/// settings.json 的写入位置：优先 exe 旁，其次当前工作目录
fn settings_path() -> PathBuf {
    beside_exe("settings.json")
}

/// 设置页的部分更新补丁
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    plugin_profile: Option<String>,
    registry: Option<String>,
    close_action: Option<String>,
}

#[tauri::command]
fn get_settings() -> Settings {
    settings()
}

/// 保存设置页改动到 settings.json（保留 junctionPath / dDrivePath / webPort 等
/// 原有字段），并立即更新内存配置。
#[tauri::command]
fn save_settings(patch: SettingsPatch) -> Result<Settings, String> {
    let mut s = settings();
    if let Some(v) = patch.plugin_profile {
        let t = v.trim().to_string();
        // 除路径分隔符外，还必须拒绝 . / .. / 点开头名称：
        // profile_dir() = profiles\<name>，"." / ".." 会逃逸到 profiles 之外
        if t.is_empty()
            || t.starts_with('.')
            || t.contains('/')
            || t.contains('\\')
            || t.eq_ignore_ascii_case("node_modules")
        {
            return Err("无效的 profile 名称".into());
        }
        s.plugin_profile = t;
    }
    if let Some(v) = patch.registry {
        let t = v.trim().to_string();
        if !t.is_empty() && !t.starts_with("http://") && !t.starts_with("https://") {
            return Err("registry 地址需以 http:// 或 https:// 开头".into());
        }
        s.registry = t;
    }
    if let Some(v) = patch.close_action {
        if v != "tray" && v != "quit" {
            return Err("closeAction 只能是 tray 或 quit".into());
        }
        s.close_action = v;
    }
    // 合并写回 settings.json（保留文件中未管理的字段）
    let path = settings_path();
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut json: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = json.as_object_mut() {
        obj.insert("pluginProfile".into(), serde_json::json!(s.plugin_profile));
        obj.insert("registry".into(), serde_json::json!(s.registry));
        obj.insert("closeAction".into(), serde_json::json!(s.close_action));
    }
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap_or_default())
        .map_err(|e| format!("写入 settings.json 失败：{}", e))?;
    update_settings(s.clone());
    Ok(s)
}

// ---------------------------------------------------------------------------
// 开机自启（注册表 Run 键）
// ---------------------------------------------------------------------------
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "DSHLauncher";

#[tauri::command]
fn get_autostart() -> bool {
    use windows_registry::*;
    match CURRENT_USER.open(RUN_KEY) {
        Ok(key) => key.get_string(RUN_VALUE).map(|v| !v.is_empty()).unwrap_or(false),
        Err(_) => false,
    }
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<bool, String> {
    use windows_registry::*;
    let exe = std::env::current_exe().map_err(|e| format!("获取 exe 路径失败：{}", e))?;
    let path = exe.to_string_lossy().to_string();
    let key = CURRENT_USER
        .create(RUN_KEY)
        .map_err(|e| format!("打开注册表失败：{}", e))?;
    if enabled {
        key.set_string(RUN_VALUE, &path)
            .map_err(|e| format!("写入注册表失败：{}", e))?;
    } else {
        let _ = key.remove_value(RUN_VALUE);
    }
    Ok(enabled)
}

/// 扫描 $DSH_HOME/profiles/ 下的已初始化 profile（设置页下拉用）
#[tauri::command]
fn list_profiles() -> Vec<String> {
    let dir = dsh_home_dir().join("profiles");
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "node_modules" {
                continue;
            }
            if e.path().join("package.json").is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// DSH 数据备份 / 恢复
//
// 备份 = 镜像打包 $DSH_HOME（排除 node_modules）到 exe 旁 backups/ 目录，
// 文件名 dsh-backup-<yyyy-MM-dd_HHmmss>.zip，仅保留最近 5 份；
// 恢复 = 校验备份文件 → 自动备份现状（pre-restore-*，保留 2 份）→
// 解压校验（防 ZipSlip）→ 改名换位替换 $DSH_HOME → 对每个 profile 执行
// pnpm install 重建插件依赖。备份/恢复互斥；备份只拦截启动器自己启动的
// DSH 实例（端口可能被无关服务占用），恢复则任何端口占用都保守拦截。
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    file_name: String,
    size: u64,
    modified: String,
    kind: String,
}

/// exe 旁 backups/ 目录（备份文件存放位置）
fn backups_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join("backups");
        }
    }
    std::env::current_dir().unwrap_or_default().join("backups")
}

/// 解码子进程控制台输出字节：优先按 UTF-8；失败（中文 Windows 上 PowerShell
/// 的 stdout/stderr 默认用 OEM 代码页 GBK/CP936）则按 CP936 转换，避免错误
/// 信息乱码（如"目录名称无效"变问号）。
fn decode_console_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    use windows_sys::Win32::Globalization::MultiByteToWideChar;
    unsafe {
        let len = MultiByteToWideChar(936, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if len > 0 {
            let mut wbuf = vec![0u16; len as usize];
            if MultiByteToWideChar(936, 0, bytes.as_ptr(), bytes.len() as i32, wbuf.as_mut_ptr(), len) > 0 {
                return String::from_utf16_lossy(&wbuf);
            }
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// 执行 PowerShell 脚本并返回 stdout（trim 后）；非零退出码报错
fn ps_output(script: &str) -> Result<String, String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("powershell 启动失败：{}", e))?;
    if !out.status.success() {
        return Err(format!("powershell 执行失败（退出码 {}）", out.status));
    }
    Ok(decode_console_bytes(&out.stdout).trim().to_string())
}

/// 执行 PowerShell 脚本并检查退出码；失败时附带 stderr 末尾（op 为环节名，
/// 如"解压"→"解压失败：..."）
fn run_ps_script(script: &str, op: &str) -> Result<(), String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("{}启动失败：{}", op, e))?;
    if out.status.success() {
        return Ok(());
    }
    let err = decode_console_bytes(&out.stderr);
    let lines: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines.iter().rev().take(6).rev().cloned().collect::<Vec<_>>().join("\n");
    Err(if tail.is_empty() {
        format!("{}失败（退出码 {}）", op, out.status)
    } else {
        format!("{}失败（退出码 {}）：\n{}", op, out.status, tail)
    })
}

/// 生成本地时间戳 yyyy-MM-dd_HHmmss（Win32 GetLocalTime，
/// 不再为取一个时间戳启动 PowerShell 子进程，每次调用省约 200-500ms）
fn now_stamp() -> Result<String, String> {
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

/// 执行 robocopy 镜像复制（排除 node_modules）。robocopy 退出码 0-7 均表示
/// 成功（复制了文件 / 无变化等），只有 >=8 才是真实失败。
fn run_robocopy(src: &Path, dst: &Path) -> Result<(), String> {
    let status = Command::new("robocopy")
        .arg(src)
        .arg(dst)
        .args(["/E", "/XD", "node_modules", "/XJ", "/NFL", "/NDL", "/NJH", "/NJS", "/R:1", "/W:1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("robocopy 启动失败：{}", e))?;
    let code = status.code().unwrap_or(-1);
    if code >= 8 {
        return Err(format!("robocopy 复制失败（退出码 {}）", code));
    }
    Ok(())
}

/// JSON 字符串转义（仅用于拼写 dsh-backup.json 清单，无需引入 serde_json）
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 镜像打包 $DSH_HOME 到指定 zip：robocopy 复制（排除 node_modules）到
/// %TEMP% 唯一目录 → 写入 dsh-backup.json 清单 → .NET ZipArchive 压缩。
/// 不用 Compress-Archive（PS 5.1 通配符对"单目录"会多包一层、可能漏隐藏文件、
/// 且条目有 2GB 上限）；临时目录无论成败都清理。
/// backup_dsh 与 restore_dsh 的自动备份共用。
fn pack_dsh_home(zip_path: &Path) -> Result<(), String> {
    let home = dsh_home_dir();
    if !home.is_dir() {
        return Err("未找到 DSH 数据目录".into());
    }
    let ts = now_stamp()?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = std::env::temp_dir().join(format!("dsh-pack-{}-{}", std::process::id(), unique));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).map_err(|e| format!("创建临时目录失败：{}", e))?;
    let result = (|| {
        run_robocopy(&home, &temp)?;
        // 备份清单：恢复时校验"这是启动器的备份"而非任意 zip
        let mut names: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&temp) {
            for e in entries.flatten() {
                names.push(e.file_name().to_string_lossy().to_string());
            }
        }
        names.sort();
        let entries_json = names
            .iter()
            .map(|n| format!("\"{}\"", json_escape(n)))
            .collect::<Vec<_>>()
            .join(",");
        let manifest = format!(
            "{{\"app\":\"dsh-launcher\",\"time\":\"{}\",\"entries\":[{}]}}",
            json_escape(&ts),
            entries_json
        );
        std::fs::write(temp.join("dsh-backup.json"), manifest)
            .map_err(|e| format!("写入备份清单失败：{}", e))?;
        // 压缩 + 条目计数一次 PowerShell 完成（原为两次子进程）
        let script = format!(
            "Add-Type -AssemblyName System.IO.Compression.FileSystem; [System.IO.Compression.ZipFile]::CreateFromDirectory('{}', '{}', [System.IO.Compression.CompressionLevel]::Optimal, $false); $z=[System.IO.Compression.ZipFile]::OpenRead('{}'); try {{ ($z.Entries | Measure-Object).Count }} finally {{ $z.Dispose() }}",
            temp.to_string_lossy().replace('\'', "''"),
            zip_path.to_string_lossy().replace('\'', "''"),
            zip_path.to_string_lossy().replace('\'', "''"),
        );
        let count = ps_output(&script).map_err(|e| format!("压缩备份失败：{}", e))?;
        // 压缩自检：zip 存在且至少含一个条目
        if !zip_path.is_file() {
            return Err("压缩完成但未生成备份文件".into());
        }
        let count: u64 = count.trim().parse().unwrap_or(0);
        if count == 0 {
            return Err("压缩完成但备份为空".into());
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&temp);
    result
}

/// 从备份文件名解析时间戳并格式化为 `yyyy-MM-dd HH:mm`；解析失败返回空字符串
/// （如 dsh-backup-2026-08-16_113023.zip → 2026-08-16 11:30）
fn format_backup_time(file_name: &str) -> String {
    let ts = file_name
        .strip_prefix("dsh-backup-")
        .or_else(|| file_name.strip_prefix("pre-restore-"))
        .and_then(|s| s.strip_suffix(".zip"))
        .unwrap_or("");
    let bytes = ts.as_bytes();
    if ts.len() != 17 || bytes[10] != b'_' {
        return String::new();
    }
    if !bytes[..10].iter().all(|b| b.is_ascii_digit() || *b == b'-') {
        return String::new();
    }
    if !bytes[11..].iter().all(|b| b.is_ascii_digit()) {
        return String::new();
    }
    format!("{} {}:{}", &ts[..10], &ts[11..13], &ts[13..15])
}

/// 列出 backups/ 下所有 .zip 备份（目录不存在返回空列表），按文件名倒序（新的在前）
fn list_backup_files() -> Vec<BackupInfo> {
    let dir = backups_dir();
    let mut items = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let file_name = e.file_name().to_string_lossy().to_string();
            if !file_name.ends_with(".zip") {
                continue;
            }
            let meta = match e.metadata() {
                Ok(m) if m.is_file() => m,
                _ => continue,
            };
            let kind = if file_name.starts_with("pre-restore-") {
                "pre-restore"
            } else {
                "backup"
            }
            .to_string();
            let modified = format_backup_time(&file_name);
            items.push(BackupInfo {
                file_name,
                size: meta.len(),
                modified,
                kind,
            });
        }
    }
    items.sort_by(|a, b| b.file_name.cmp(&a.file_name));
    items
}

/// 清理 backups/ 下指定前缀的旧备份，仅保留最近 keep 份（按文件名时间戳倒序）
fn cleanup_old_backups(prefix: &str, keep: usize) {
    let dir = backups_dir();
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix) && name.ends_with(".zip") {
                names.push(name);
            }
        }
    }
    names.sort_by(|a, b| b.cmp(a)); // 倒序：新的在前
    for name in names.into_iter().skip(keep) {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// DSH 数据目录的特征文件/目录（用于识别"这是 DSH_HOME"）
const DSH_MARKERS: [&str; 4] = ["settings.yaml", "sessions", "profiles", "storages"];

fn has_dsh_markers(dir: &Path) -> bool {
    DSH_MARKERS.iter().any(|m| dir.join(m).exists())
}

/// 定位解压内容的真实根目录：有清单 → 解压根；根目录含 DSH 特征 → 根目录；
/// 否则兼容"把 DSH_HOME 目录本身打包"的 zip（恰一个子目录且内含数据）。
fn find_content_root(temp: &Path) -> Option<PathBuf> {
    if temp.join("dsh-backup.json").is_file() {
        return Some(temp.to_path_buf());
    }
    if has_dsh_markers(temp) {
        return Some(temp.to_path_buf());
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(temp) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                dirs.push(e.path());
            }
        }
    }
    if dirs.len() == 1 && has_dsh_markers(&dirs[0]) {
        return Some(dirs[0].clone());
    }
    None
}

/// 解压 zip 到临时目录。PS 5.1 的 Expand-Archive 不防 ZipSlip（恶意 zip 可把
/// 条目写到目标目录之外），改用 .NET ZipArchive 逐条校验后展开，同时规避
/// 2GB / 通配符等兼容性问题。拒绝：`..`、以 / 或 \ 开头、含 : * ? " < > |、
/// 或 CON/NUL 等设备名（大小写不敏感）的条目；条目数与解压总量（8GB）双重上限防解压炸弹。
/// 目录条目（/ 或 \ 结尾）直接跳过——.NET Framework CreateFromDirectory 生成的
/// 空目录条目以 \ 结尾，当作文件传给 ExtractToFile 会抛"目录名称无效"。
fn extract_zip_safe(zip: &Path, dst: &Path) -> Result<(), String> {
    let script = format!(
        r#"Add-Type -AssemblyName System.IO.Compression.FileSystem
$ErrorActionPreference = 'Stop'
$z = [System.IO.Compression.ZipFile]::OpenRead('{}')
try {{
    if ($z.Entries.Count -gt 200000) {{ throw '备份文件条目过多，已中止解压' }}
    $total = [long]0
    foreach ($e in $z.Entries) {{
        $n = $e.FullName
        if ($n -match '\.\.' -or $n.StartsWith('/') -or $n.StartsWith('\') -or $n.Contains(':') -or
            $n.Contains('*') -or $n.Contains('?') -or $n.Contains('"') -or $n.Contains('<') -or
            $n.Contains('>') -or $n.Contains('|') -or $n -match '^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\.|$)') {{
            throw ('备份文件包含不安全条目，已中止解压：' + $n)
        }}
        $total += $e.Length
    }}
    if ($total -gt 8GB) {{ throw ('备份解压总量超过 8GB 上限，已中止：' + $total + ' 字节') }}
    foreach ($e in $z.Entries) {{
        if ($e.FullName.EndsWith('/') -or $e.FullName.EndsWith('\')) {{ continue }}
        $dest = Join-Path '{}' ($e.FullName -replace '/', '\')
        $dir = Split-Path -Parent $dest
        if (-not (Test-Path -LiteralPath $dir)) {{ New-Item -ItemType Directory -Path $dir -Force | Out-Null }}
        [System.IO.Compression.ZipFileExtensions]::ExtractToFile($e, $dest, $true)
    }}
}} finally {{ $z.Dispose() }}"#,
        zip.to_string_lossy().replace('\'', "''"),
        dst.to_string_lossy().replace('\'', "''"),
    );
    run_ps_script(&script, "解压")
}

/// 恢复前校验 $DSH_HOME 指向：必须是绝对路径，不能是盘符根 / 用户目录 /
/// 桌面 / 临时目录等关键位置；目录已存在且有内容时必须含 DSH 数据特征，
/// 防止 DSH_HOME 环境变量被误设为普通目录时误删用户数据。
fn validate_home_target(home: &Path) -> Result<(), String> {
    let home_str = home.to_string_lossy().to_string();
    if !home.is_absolute() {
        return Err(format!("DSH_HOME 不是绝对路径，已中止：{}", home_str));
    }
    let canon = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    if canon.parent().is_none() {
        return Err(format!("DSH_HOME 指向盘符根目录，已中止：{}", home_str));
    }
    let userprofile = std::env::var("USERPROFILE").unwrap_or_default();
    let mut protected: Vec<String> = Vec::new();
    if !userprofile.trim().is_empty() {
        protected.push(userprofile.clone());
        protected.push(format!(r"{}\Desktop", userprofile.trim_end_matches('\\')));
    }
    protected.push(std::env::temp_dir().to_string_lossy().into_owned());
    if let Ok(t) = std::env::var("TEMP") {
        protected.push(t);
    }
    if let Ok(t) = std::env::var("TMP") {
        protected.push(t);
    }
    for p in protected {
        let pc = PathBuf::from(p);
        let pc = pc.canonicalize().unwrap_or(pc);
        if canon == pc {
            return Err(format!("DSH_HOME 指向系统关键目录，已中止：{}", home_str));
        }
        // home 是用户目录的上级（如 C:\Users）同样危险
        if pc.starts_with(&canon) && pc != canon {
            return Err(format!("DSH_HOME 是用户目录的上级目录，已中止：{}", home_str));
        }
    }
    // 目录已存在且非空时，必须含 DSH 数据特征
    if home.is_dir() {
        let non_empty = std::fs::read_dir(home).map(|it| it.flatten().next().is_some()).unwrap_or(false);
        if non_empty && !has_dsh_markers(home) {
            return Err(format!(
                "DSH_HOME 目录不含 DSH 数据特征（settings.yaml / profiles 等），已中止：{}",
                home_str
            ));
        }
    }
    Ok(())
}

/// 恢复落盘：校验 $DSH_HOME 身份后，用"改名换位"流程替换数据目录
/// （home → home.bak-<ts>，复制成功后再删 .bak，失败则回滚），
/// 避免"先清空再写入"在中断时留下半删状态、以及 PS Remove-Item 跟随
/// junction 删除目录外内容的已知问题。
/// 返回 Some(提示) 表示 .bak 未能删除（恢复本身已成功）。
fn swap_restore(root: &Path, ts: &str) -> Result<Option<String>, String> {
    let home = dsh_home_dir();
    validate_home_target(&home)?;
    if !home.is_dir() {
        // 全新安装场景：无现状可保留，直接建目录复制
        std::fs::create_dir_all(&home).map_err(|e| format!("创建数据目录失败：{}", e))?;
        run_robocopy(root, &home).map_err(|e| format!("恢复复制失败：{}", e))?;
        return Ok(None);
    }
    // 动数据前紧邻复核一次（防 TOCTOU：检查后 DSH 才启动）
    if dsh_running_for_restore() {
        return Err("DSH 正在运行，请先停止 DSH 再恢复".into());
    }
    let name = home
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // 扫描中断遗留的旧 .bak：不自动删除——它可能是上次恢复被杀时唯一幸存的
    // 原始数据，删错不可逆；仅在恢复完成后于摘要中提示路径，由用户决定去留
    let mut stale_bak_note: Option<String> = None;
    let bak_prefix = format!("{}.bak-", name);
    if let Ok(entries) = std::fs::read_dir(home.parent().unwrap_or_else(|| Path::new("."))) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&bak_prefix) {
                stale_bak_note = Some(format!(
                    "发现上次恢复遗留的数据备份 {}（确认无需回退后可手动删除）",
                    e.path().display()
                ));
            }
        }
    }
    let bak = home.with_file_name(format!("{}.bak-{}", name, ts));
    std::fs::rename(&home, &bak).map_err(|e| format!("移动当前数据目录失败：{}", e))?;
    if let Err(e) = std::fs::create_dir_all(&home) {
        let _ = std::fs::rename(&bak, &home);
        return Err(format!("创建数据目录失败：{}", e));
    }
    if let Err(e) = run_robocopy(root, &home) {
        let _ = std::fs::remove_dir_all(&home); // 删除不完整的新目录（无 junction，安全）
        let rolled = std::fs::rename(&bak, &home).is_ok();
        return Err(if rolled {
            format!("恢复复制失败：{}（已回滚，原数据未受影响）", e)
        } else {
            format!("恢复复制失败：{}\n原数据已移动到 {}，请手动恢复", e, bak.display())
        });
    }
    // 复制成功：尽力删除本次 .bak；删除失败不影响恢复结果，仅提示
    let mut notes: Vec<String> = Vec::new();
    if let Err(_) = std::fs::remove_dir_all(&bak) {
        notes.push(format!("恢复前数据已保留在 {}，可手动删除", bak.display()));
    }
    if let Some(note) = stale_bak_note {
        notes.push(note);
    }
    let note = if notes.is_empty() { None } else { Some(notes.join("\n")) };
    Ok(note)
}

/// 遍历 $DSH_HOME/profiles/*/package.json 执行 pnpm install 重建插件依赖。
/// 每个 profile 独立执行、互不影响；pnpm 缺失时跳过并在摘要中说明。
fn rebuild_plugins(
    home: &Path,
    proxy_on: bool,
    proxy_addr: &str,
    progress: &Channel<String>,
) -> Result<String, String> {
    let profiles = home.join("profiles");
    let mut profile_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&profiles) {
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if name == "node_modules" {
                continue;
            }
            if path.join("package.json").is_file() {
                profile_dirs.push(path);
            }
        }
    }
    profile_dirs.sort();
    if profile_dirs.is_empty() {
        return Ok("插件重建：未发现需要重建的 profile（profiles 目录为空）".into());
    }

    let pnpm = if which("pnpm.cmd") {
        "pnpm.cmd"
    } else if which("pnpm") {
        "pnpm"
    } else {
        ""
    };
    if pnpm.is_empty() {
        return Ok("插件重建：未找到 pnpm，已跳过插件安装（可在插件管理中手动重建）".into());
    }

    let mut ok = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for dir in &profile_dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let _ = progress.send(format!("正在重建插件依赖：{}", name));
        let mut cmd = Command::new(pnpm);
        cmd.arg("install").current_dir(dir).creation_flags(CREATE_NO_WINDOW);
        for (k, v) in npm_proxy_env(proxy_on, proxy_addr) {
            cmd.env(k, v);
        }
        apply_registry(&mut cmd);
        match run_cmd_streaming(&mut cmd, progress, "pnpm") {
            Ok(_) => ok += 1,
            Err(e) => failed.push(format!("{}：{}", name, e)),
        }
    }
    if failed.is_empty() {
        Ok(format!("插件重建：{} 个 profile 全部成功", ok))
    } else {
        Ok(format!(
            "插件重建：{} 个 profile 成功，{} 个失败：{}",
            ok,
            failed.len(),
            failed.join("\n")
        ))
    }
}

/// 备份/恢复互斥锁：同一时刻只允许一个备份或恢复操作（防同秒撞文件名、
/// 临时目录互踩、并发读写 DSH_HOME）。RAII 释放，任何退出路径都解锁。
static BACKUP_OP: AtomicBool = AtomicBool::new(false);

/// 启动时清理进程崩溃/被杀遗留的备份临时目录
/// （%TEMP%\dsh-pack-* / dsh-restore-*）。仅删除修改时间超过 24h 的目录，
/// 避免极端情况下误删仍在使用的目录；启动器有单实例保护，正常不会并发。
fn cleanup_stale_temp_dirs() {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now_secs.saturating_sub(24 * 3600);
    let temp = std::env::temp_dir();
    for prefix in ["dsh-pack-", "dsh-restore-"] {
        let Ok(entries) = std::fs::read_dir(&temp) else { return };
        for e in entries.flatten() {
            if !e.file_name().to_string_lossy().starts_with(prefix) || !e.path().is_dir() {
                continue;
            }
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX); // 取不到时间就不删，宁可保留
            if mtime < cutoff {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

struct BackupOpGuard;

impl BackupOpGuard {
    fn acquire() -> Result<Self, String> {
        if BACKUP_OP
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err("另一项备份/恢复操作正在进行，请稍候再试".into());
        }
        Ok(BackupOpGuard)
    }
}

impl Drop for BackupOpGuard {
    fn drop(&mut self) {
        BACKUP_OP.store(false, Ordering::SeqCst);
    }
}

/// 创建一份新备份（dsh-backup-<ts>.zip），保留最近 5 份
#[tauri::command]
async fn backup_dsh() -> Result<BackupInfo, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let _guard = BackupOpGuard::acquire()?;
        if dsh_running_for_backup() {
            return Err("启动器启动的 DSH 正在运行，请先停止 DSH 再进行备份（运行中备份可能不完整）".into());
        }
        let ts = now_stamp()?;
        let backups = backups_dir();
        std::fs::create_dir_all(&backups).map_err(|e| format!("创建备份目录失败：{}", e))?;
        let file_name = format!("dsh-backup-{}.zip", ts);
        pack_dsh_home(&backups.join(&file_name))?;
        cleanup_old_backups("dsh-backup-", 5);
        let size = std::fs::metadata(backups.join(&file_name)).map(|m| m.len()).unwrap_or(0);
        let modified = format_backup_time(&file_name);
        Ok(BackupInfo {
            file_name,
            size,
            modified,
            kind: "backup".into(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 列出 backups/ 下所有备份（新的在前）
#[tauri::command]
fn list_backups() -> Vec<BackupInfo> {
    list_backup_files()
}

/// 恢复备份：解压覆盖 $DSH_HOME 并自动重建插件。
/// 恢复前自动备份现状（pre-restore-<ts>.zip，保留最近 2 份），可随时回退。
#[tauri::command]
async fn restore_dsh(file_name: String, proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || restore_dsh_impl(&file_name, proxy_on, &proxy_addr, &progress))
        .await
        .map_err(|e| e.to_string())?
}

fn restore_dsh_impl(
    file_name: &str,
    proxy_on: bool,
    proxy_addr: &str,
    progress: &Channel<String>,
) -> Result<String, String> {
    let _guard = BackupOpGuard::acquire()?;

    // 1. 路径安全校验：只允许 backups/ 下的裸 .zip 文件名（防目录穿越与
    //    cmd / PS 二次解析：拒绝 \ / : * ? " < > | 及控制字符）
    let name = file_name.trim();
    if name.is_empty()
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
        || name.contains("..")
        || name.chars().any(|c| c.is_control())
        || !name.to_ascii_lowercase().ends_with(".zip")
    {
        return Err("无效的备份文件".into());
    }
    let backups = backups_dir();
    let zip = backups.join(name);
    if !zip.is_file() {
        return Err("无效的备份文件".into());
    }
    // canonical 归属断言：文件必须真实位于 backups/ 之内
    let canon_zip = zip.canonicalize().map_err(|_| "无效的备份文件".to_string())?;
    let canon_dir = backups.canonicalize().unwrap_or_else(|_| backups.clone());
    if !canon_zip.starts_with(&canon_dir) {
        return Err("无效的备份文件".into());
    }

    // 2. DSH 运行检查：运行中恢复会覆盖正在使用的数据文件
    if dsh_running_for_restore() {
        return Err("DSH 正在运行，请先停止 DSH 再恢复".into());
    }

    // 3~5：先解压（ZipSlip 防护）并校验 zip 有效性——无效备份快速失败，
    // 不再像旧流程那样先白做一次完整的 pre-restore 备份；临时目录无论成败都清理
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp = std::env::temp_dir().join(format!("dsh-restore-{}-{}", std::process::id(), unique));
    let _ = std::fs::remove_dir_all(&temp);
    std::fs::create_dir_all(&temp).map_err(|e| format!("创建临时目录失败：{}", e))?;
    let result = (|| -> Result<String, String> {
        // 3. 解压到临时目录（逐条目校验，防 ZipSlip）
        let _ = progress.send("正在解压备份…".to_string());
        extract_zip_safe(&zip, &temp)?;

        // 4. 有效性校验：定位内容根目录
        let root = find_content_root(&temp).ok_or("备份文件无效（缺少 DSH 数据）")?;
        let manifest_ok = root.join("dsh-backup.json").is_file()
            && std::fs::read_to_string(root.join("dsh-backup.json"))
                .map(|s| s.contains("dsh-launcher"))
                .unwrap_or(false);
        if !manifest_ok {
            // 手动打包的 zip：必须同时含 settings.yaml 与至少一个数据目录
            let strict = root.join("settings.yaml").is_file()
                && ["sessions", "profiles", "storages"]
                    .iter()
                    .any(|m| root.join(m).exists());
            if !strict {
                return Err("备份文件无效（缺少 DSH 数据或清单损坏）".into());
            }
        }

        // 会话数量（粗略统计，用于恢复摘要）
        let session_count = std::fs::read_dir(root.join("sessions"))
            .map(|it| it.flatten().count())
            .unwrap_or(0);

        // 5. zip 校验通过，此时才备份现状（pre-restore-*，仅保留最近 2 份）；
        //    数据目录不存在时无现状可备份
        let _ = progress.send("正在自动备份当前数据…".to_string());
        if dsh_home_dir().is_dir() {
            let ts = now_stamp()?;
            std::fs::create_dir_all(&backups).map_err(|e| format!("创建备份目录失败：{}", e))?;
            pack_dsh_home(&backups.join(format!("pre-restore-{}.zip", ts)))?;
            cleanup_old_backups("pre-restore-", 2);
        }

        // 6. 改名换位恢复（校验与备份均已完成，此时才动 $DSH_HOME）
        let _ = progress.send("正在恢复数据…".to_string());
        let ts = now_stamp()?;
        let bak_note = swap_restore(&root, &ts)?;

        // 7. 重建插件（pnpm install，失败记录但不中断）
        let home = dsh_home_dir();
        let plugin_summary = rebuild_plugins(&home, proxy_on, proxy_addr, progress)?;

        // 8. 返回摘要
        let mut summary = format!(
            "已恢复 DSH 数据（配置文件、{} 个会话、插件清单）\n{}",
            session_count, plugin_summary
        );
        if let Some(note) = bak_note {
            summary.push_str(&format!("\n{}", note));
        }
        Ok(summary)
    })();
    let _ = std::fs::remove_dir_all(&temp);
    result
}

/// 打开 exe 旁 backups/ 目录（不存在则先创建）
#[tauri::command]
fn open_backups_dir() -> String {
    let dir = backups_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("无法打开备份目录：{}", e);
    }
    let dir_str = dir.to_string_lossy().to_string();
    // 直接用 explorer 打开，不经 cmd start，避免路径含 & 等元字符时被二次解析
    let _ = Command::new("explorer")
        .arg(&dir_str)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    format!("已打开备份目录：{}", dir.display())
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
    *lock_ok(&DSH_CACHE) = None;
}

fn find_dsh() -> Option<DshInstall> {
    if let Some(inst) = lock_ok(&DSH_CACHE).as_ref() {
        return Some(inst.clone());
    }
    let found = find_dsh_uncached();
    if found.is_some() {
        *lock_ok(&DSH_CACHE) = found.clone();
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

/// 启动器自己启动的 DSH 进程 PID 注册表。
///
/// 判定"DSH 是否在运行"必须以启动器管理的实例为准：webPort（3080）可能被
/// 与本启动器无关的服务占用（例如常驻的 DSH 环境/其它程序），仅凭端口探测
/// 会把无关服务误判为"DSH 在运行"，导致备份被错误拦截。
static TRACKED_DSH_PIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

fn track_dsh_pid(pid: u32) {
    let mut g = lock_ok(&TRACKED_DSH_PIDS);
    if !g.contains(&pid) {
        g.push(pid);
    }
}

fn untrack_dsh_pid(pid: u32) {
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
fn dsh_running_for_backup() -> bool {
    tracked_dsh_running()
}

/// 恢复前的运行检查：备份判定 + 端口占用兜底。
/// 恢复会替换 $DSH_HOME，任何占用 webPort 的服务（即使无法识别身份）
/// 都按保守口径拦截。
fn dsh_running_for_restore() -> bool {
    tracked_dsh_running() || is_running()
}

fn find_dsh_pids(port: u16) -> Vec<u32> {
    let out = match Command::new("netstat").args(["-ano"]).creation_flags(CREATE_NO_WINDOW).output() {
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
    if let Some(v) = lock_ok(&WHICH_CACHE).get(name) {
        return *v;
    }
    let found = Command::new("where").arg(name).stdout(Stdio::null()).stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status().map(|s| s.success()).unwrap_or(false);
    lock_ok(&WHICH_CACHE).insert(name.to_string(), found);
    found
}

fn clear_which_cache() {
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
fn apply_registry(cmd: &mut Command) {
    let r = settings().registry;
    if !r.is_empty() {
        cmd.env("npm_config_registry", r);
    }
}

fn run_npm(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new(npm_program());
    cmd.args(args).creation_flags(CREATE_NO_WINDOW);
    apply_registry(&mut cmd);
    cmd.output()
}

fn npm_command() -> Command {
    let mut cmd = Command::new(npm_program());
    cmd.creation_flags(CREATE_NO_WINDOW);
    apply_registry(&mut cmd);
    cmd
}

/// 执行命令：stdout 逐行推送 progress，stderr 末尾收集到错误信息。
/// 非零退出码返回 Err（带 stderr 末尾），不再静默吞掉失败。
fn run_cmd_streaming(cmd: &mut Command, progress: &Channel<String>, op_name: &str) -> Result<(), String> {
    use std::io::BufRead;
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
    let status = child.wait().map_err(|e| format!("{} 等待失败：{}", op_name, e))?;
    if status.success() {
        return Ok(());
    }
    let tail = |lines: &[String], n: usize| -> String {
        lines.iter().rev().take(n).rev().cloned().collect::<Vec<_>>().join("\n")
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

fn run_npm_cmd(cmd: &mut Command, progress: &Channel<String>) -> Result<(), String> {
    run_cmd_streaming(cmd, progress, "npm")
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

    // pnpm：插件安装/更新依赖（dsh plugin 转发 pnpm），缺失不影响 DSH 本体
    if which("pnpm") || which("pnpm.cmd") {
        items.push(CheckItem { name: "pnpm 环境".into(), status: "OK".into(), detail: "已找到 pnpm，插件管理可用".into() });
    } else {
        items.push(CheckItem { name: "pnpm 环境".into(), status: "WARN".into(), detail: "未找到 pnpm，插件安装/更新不可用（DSH 本体功能不受影响）；请执行 npm install -g pnpm".into() });
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

/// 查询 npm registry 上指定包的最新版本。失败时返回具体原因（含 npm stderr 末尾），
/// 并带 30 秒超时保护（npm fetch-timeout 默认 5 分钟，registry 挂起时
/// 不能无限等待）。
fn npm_view_version(pkg: &str, env: &[(String, String)]) -> Result<String, String> {
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

/// 查询 DSH 本体最新版本（内部复用 npm_view_version）
fn get_latest_dsh_version(env: &[(String, String)]) -> Result<String, String> {
    npm_view_version(DSH_PKG_NAME, env)
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

// ---------------------------------------------------------------------------
// Web 模式启动：输出写入 web.log（exe 旁），启动失败 / 端口未就绪时
// 回读日志末尾给出错误信息（不再静默吞掉）
// ---------------------------------------------------------------------------
fn web_log_path() -> std::path::PathBuf {
    beside_exe("web.log")
}

fn tail_file(path: &std::path::Path, max_bytes: usize) -> String {
    // Seek 到尾部再读：web.log 只在启动时截断（>1MB），长跑实例可能增长到数百 MB，
    // 整文件读入再截尾会造成不必要的内存与 IO 开销
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes as u64);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).to_string()
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
            track_dsh_pid(child.id()); // 记录启动器管理的 DSH 实例 PID
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
// DSH 插件管理（web profile）
//
// 插件机制：插件 = profile 目录（$DSH_HOME/profiles/<pluginProfile>，默认 web）
// 的 pnpm 依赖；package.json 声明了 "dsh": { "bundle": { "patch": ... } } 的包是
// "bundle"（profile 层），`dsh.profile.bundles` 按序叠加生效。
// 安装 / 卸载 / 更新全部走官方 `dsh plugin --profile <name> <add|remove|update>`
// （内部转发 pnpm 并对账 bundles 列表，见 @deepseek-ai/dsh 的 plugin 子命令），
// 不直接改 package.json，保证与 CLI / Web UI 行为一致。
// ---------------------------------------------------------------------------
fn dsh_home_dir() -> PathBuf {
    // 与 dsh-home-paths 一致：$DSH_HOME（非空）> %USERPROFILE%\.dsh
    if let Ok(h) = std::env::var("DSH_HOME") {
        let t = h.trim();
        if !t.is_empty() {
            return PathBuf::from(t);
        }
    }
    std::env::var("USERPROFILE")
        .map(|u| PathBuf::from(u).join(".dsh"))
        .unwrap_or_else(|_| PathBuf::from(".dsh"))
}

fn profile_dir() -> PathBuf {
    dsh_home_dir().join("profiles").join(settings().plugin_profile)
}

/// 去掉 spec 中的版本号得到裸包名：`@scope/name@1.0.0` → `@scope/name`；
/// git / file: 等含 `/` 的 spec 原样返回。
fn bare_pkg_name(spec: &str) -> &str {
    let s = spec.trim();
    if let Some(i) = s.rfind('@') {
        if i > 0 && !s[i + 1..].contains('/') {
            return &s[..i];
        }
    }
    s
}

fn validate_plugin_name(name: &str) -> Result<(), String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("请输入要安装的插件包名".into());
    }
    if n.len() > 200 {
        return Err("包名过长（最多 200 字符）".into());
    }
    if n.starts_with('-') {
        return Err("包名不能以 - 开头".into());
    }
    if n.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("包名包含非法字符（空格 / 控制符）".into());
    }
    Ok(())
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    name: String,
    spec: String,          // dependencies 中的 spec（如 ^0.12.2 / file:...），内置组件为空
    version: String,       // 已安装版本（读 node_modules 下 package.json），读不到为空
    description: String,
    is_bundle: bool,       // 是否声明了 dsh.bundle.patch（profile 层）
    is_builtin: bool,      // 在 bundles 中但不在 dependencies（随 DSH 安装的内置层）
    enabled: Option<bool>, // bundle：是否在 bundles 列表；普通依赖：None
    entry_ids: Vec<String>, // bundle patch 中 insert 的入口 id（启用/禁用定位用）
    patch_disabled: bool,  // profile cordis.patch.yml 中是否存在该插件的 disabled 条目
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginListResult {
    profile_dir: String,
    initialized: bool,
    plugins: Vec<PluginInfo>,
}

/// 轻量解析 bundle 的 cordis.patch.yml，提取 insert 块中的条目 id。
/// 只解析本应用需要的结构（顶层数组项的 `- insert:` 下缩进的 `- id: xxx`），
/// 不引入完整 YAML 依赖。
fn extract_insert_ids(content: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut in_insert = false;
    let mut insert_indent = 0usize;
    for line in content.lines() {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !in_insert {
            if t.starts_with("- insert:") || t.starts_with("insert:") {
                in_insert = true;
                insert_indent = indent;
            }
            continue;
        }
        if t.starts_with("- id:") && indent > insert_indent {
            let id = t["- id:".len()..].trim().trim_matches(|c| c == '\'' || c == '"').to_string();
            if !id.is_empty() {
                ids.push(id);
            }
            continue;
        }
        if indent <= insert_indent {
            in_insert = false; // 缩进回退：insert 块结束
        }
    }
    ids
}

/// 轻量解析 profile 的 cordis.patch.yml，提取所有 `- id: xxx` + `disabled: true`
/// 组合的条目 id（含用户手动配置的禁用项）。
fn read_disabled_ids(content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut ids = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("- id:") {
            let id = rest.trim().trim_matches(|c| c == '\'' || c == '"').to_string();
            if id.is_empty() {
                continue;
            }
            let next = lines.get(i + 1).map(|n| n.trim()).unwrap_or("");
            if next.replace(' ', "") == "disabled:true" {
                ids.push(id);
            }
        }
    }
    ids
}

/// 读取已安装包的 (version, description, 是否声明 dsh.bundle.patch, 入口 ids)
fn installed_pkg_info(profile: &Path, name: &str) -> Option<(String, String, bool, Vec<String>)> {
    let p = profile.join("node_modules").join(name).join("package.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    let version = v.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let description = v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let is_bundle = v.get("dsh").and_then(|d| d.get("bundle")).and_then(|b| b.get("patch")).is_some();
    let entry_ids = if is_bundle {
        v.get("dsh")
            .and_then(|d| d.get("bundle"))
            .and_then(|b| b.get("patch"))
            .and_then(|p| p.as_str())
            .and_then(|rel| std::fs::read_to_string(profile.join("node_modules").join(name).join(rel)).ok())
            .map(|c| extract_insert_ids(&c))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Some((version, description, is_bundle, entry_ids))
}

/// 内置 bundle（不在 profile dependencies 中）的版本与描述：
/// 版本优先取 dsh 自身 manifest 的 dependencies 声明，描述从实际安装目录读取。
fn builtin_pkg_info(name: &str) -> (String, String) {
    let mut version = String::new();
    let mut description = String::new();
    if let Some(inst) = find_dsh() {
        let root = PathBuf::from(&inst.pkg_root);
        let dsh_pkg = root.join("node_modules").join(DSH_PKG_NAME).join("package.json");
        if let Ok(content) = std::fs::read_to_string(&dsh_pkg) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(spec) = v.get("dependencies").and_then(|d| d.get(name)).and_then(|x| x.as_str()) {
                    version = spec.trim_start_matches('^').trim_start_matches('~').to_string();
                }
            }
        }
        // 全局安装时 dsh 的依赖嵌套在 dsh/node_modules 下，也兼容 hoisted 布局
        for cand in [
            root.join("node_modules").join(name).join("package.json"),
            root.join("node_modules").join(DSH_PKG_NAME).join("node_modules").join(name).join("package.json"),
        ] {
            if let Ok(content) = std::fs::read_to_string(&cand) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    description = v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if version.is_empty() {
                        version = v.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    }
                    break;
                }
            }
        }
    }
    (version, description)
}

fn run_plugin_list() -> PluginListResult {
    let profile = profile_dir();
    let content = match std::fs::read_to_string(profile.join("package.json")) {
        Ok(c) => c,
        Err(_) => return PluginListResult {
            profile_dir: profile.to_string_lossy().into_owned(),
            initialized: false,
            plugins: Vec::new(),
        },
    };
    let manifest: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return PluginListResult {
            profile_dir: profile.to_string_lossy().into_owned(),
            initialized: false,
            plugins: Vec::new(),
        },
    };
    let deps: Vec<(String, String)> = manifest.get("dependencies")
        .and_then(|d| d.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
        .unwrap_or_default();
    let bundles: Vec<String> = manifest.get("dsh")
        .and_then(|d| d.get("profile"))
        .and_then(|p| p.get("bundles"))
        .and_then(|b| b.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut plugins = Vec::new();
    // profile 层禁用状态（cordis.patch.yml 中的 disabled 条目 id）
    let patch_content = std::fs::read_to_string(profile.join("cordis.patch.yml")).unwrap_or_default();
    let disabled_ids = read_disabled_ids(&patch_content);

    // 用户插件（profile dependencies），保持 manifest 声明顺序
    for (name, spec) in &deps {
        let (version, description, is_bundle, entry_ids) = installed_pkg_info(&profile, name).unwrap_or_default();
        let enabled = if bundles.iter().any(|b| b == name) {
            Some(true)
        } else if is_bundle {
            Some(false)
        } else {
            None
        };
        let patch_disabled = entry_ids.iter().any(|id| disabled_ids.contains(id));
        plugins.push(PluginInfo {
            name: name.clone(),
            spec: spec.clone(),
            version,
            description,
            is_bundle,
            is_builtin: false,
            enabled,
            entry_ids,
            patch_disabled,
        });
    }
    // 内置 bundle（在 bundles 中但不在 dependencies），只读展示
    for b in &bundles {
        if deps.iter().any(|(n, _)| n == b) {
            continue;
        }
        let (version, description) = builtin_pkg_info(b);
        plugins.push(PluginInfo {
            name: b.clone(),
            spec: String::new(),
            version,
            description,
            is_bundle: true,
            is_builtin: true,
            enabled: Some(true),
            entry_ids: Vec::new(),
            patch_disabled: false,
        });
    }
    PluginListResult { profile_dir: profile.to_string_lossy().into_owned(), initialized: true, plugins }
}

/// 执行 `dsh plugin --profile <profile> <args...>`（官方通道，转发 pnpm），
/// stdout 流式推送进度；stderr 末尾在失败时回读。
fn run_profile_cmd(profile: &str, args: &[&str], proxy_on: bool, proxy_addr: &str, progress: &Channel<String>) -> Result<(), String> {
    let bin = find_bin().ok_or_else(|| "未找到 dsh CLI 入口文件".to_string())?;
    let mut cmd = Command::new("node");
    cmd.arg(&bin).arg("plugin").arg("--profile").arg(profile)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW);
    for (k, v) in npm_proxy_env(proxy_on, proxy_addr) {
        cmd.env(k, v);
    }
    apply_registry(&mut cmd);
    run_cmd_streaming(&mut cmd, progress, "pnpm")
}

/// 插件管理命令：目标 profile 取设置里的 pluginProfile（默认 web）
fn run_plugin_cmd(args: &[&str], proxy_on: bool, proxy_addr: &str, progress: &Channel<String>) -> Result<(), String> {
    run_profile_cmd(&settings().plugin_profile, args, proxy_on, proxy_addr, progress)
}

#[tauri::command]
async fn plugin_list() -> PluginListResult {
    tauri::async_runtime::spawn_blocking(run_plugin_list)
        .await
        .unwrap_or_else(|_| PluginListResult { profile_dir: String::new(), initialized: false, plugins: Vec::new() })
}

#[tauri::command]
async fn plugin_install(name: String, proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<String, String> {
    let name = name.trim().to_string();
    validate_plugin_name(&name)?;
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        // 已安装则给出明确提示（避免 pnpm add 静默重装）
        if let Ok(content) = std::fs::read_to_string(profile.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(deps) = v.get("dependencies").and_then(|d| d.as_object()) {
                    let bare = bare_pkg_name(&name).to_string();
                    if deps.contains_key(&bare) {
                        return Err(format!("插件 {} 已安装，如需升级请用「更新」", bare));
                    }
                }
            }
        }
        run_plugin_cmd(&["add", &name], proxy_on, &proxy_addr, &progress)?;
        // 安装后读回实际版本（pnpm add 可能解析出具体版本）
        let bare = bare_pkg_name(&name).to_string();
        Ok(installed_pkg_info(&profile, &bare).map(|(v, _, _, _)| v).unwrap_or_default())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn plugin_remove(name: String, proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        let content = std::fs::read_to_string(profile.join("package.json"))
            .map_err(|_| "web profile 尚未初始化".to_string())?;
        let manifest: serde_json::Value = serde_json::from_str(&content)
            .map_err(|_| "web profile 配置解析失败".to_string())?;
        let deps = manifest.get("dependencies").and_then(|d| d.as_object()).cloned().unwrap_or_default();
        let bundles: Vec<String> = manifest.get("dsh")
            .and_then(|d| d.get("profile"))
            .and_then(|p| p.get("bundles"))
            .and_then(|b| b.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !deps.contains_key(&name) {
            if bundles.iter().any(|b| b == &name) {
                return Err(format!("{} 是 DSH 内置组件，不能卸载", name));
            }
            return Err(format!("插件 {} 不在已安装列表", name));
        }
        run_plugin_cmd(&["remove", &name], proxy_on, &proxy_addr, &progress)?;
        Ok("removed".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn plugin_update(all: bool, name: Option<String>, proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if all {
            run_plugin_cmd(&["update", "--latest"], proxy_on, &proxy_addr, &progress)?;
            return Ok("all".to_string());
        }
        let name = name.ok_or_else(|| "缺少插件名".to_string())?;
        // 内置组件不是 profile 依赖，先给出明确提示
        let profile = profile_dir();
        if let Ok(content) = std::fs::read_to_string(profile.join("package.json")) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let in_deps = v.get("dependencies")
                    .and_then(|d| d.as_object())
                    .map(|d| d.contains_key(&name))
                    .unwrap_or(false);
                if !in_deps {
                    return Err(format!("{} 不在已安装插件列表（内置组件随 DSH 本体更新）", name));
                }
            }
        }
        run_plugin_cmd(&["update", "--latest", &name], proxy_on, &proxy_addr, &progress)?;
        Ok(name)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginUpdateInfo {
    name: String,
    installed: String,
    latest: String,       // registry 最新版本；空表示未检查（本地依赖）或检查失败
    error: Option<String>, // npm view 失败原因（网络 / 包不存在等）
}

/// 该 spec 是否走 npm registry（file: / link: / git: / 路径等本地或 VCS
/// 依赖无法用 npm view 查版本，跳过检查）。
fn is_registry_spec(spec: &str) -> bool {
    let s = spec.trim();
    !(s.starts_with("file:")
        || s.starts_with("link:")
        || s.starts_with("git")
        || s.starts_with("github:")
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with('.')
        || s.contains('\\'))
}

/// 检查所有用户插件的 registry 最新版本（并行执行 npm view，各自 30 秒超时）。
/// 每个插件独立失败，互不影响。
#[tauri::command]
async fn plugin_check_updates(proxy_on: bool, proxy_addr: String) -> Vec<PluginUpdateInfo> {
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        let content = match std::fs::read_to_string(profile.join("package.json")) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let manifest: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let deps: Vec<(String, String)> = manifest.get("dependencies")
            .and_then(|d| d.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        let env = npm_proxy_env(proxy_on, &proxy_addr);
        // 先按依赖顺序整理：无需查询的直接出结果，需要 npm view 的进入待查队列
        let mut slots: Vec<Option<PluginUpdateInfo>> = Vec::with_capacity(deps.len());
        let mut pending: Vec<(usize, String, String)> = Vec::new(); // (slot, name, installed)
        for (name, spec) in &deps {
            let installed = installed_pkg_info(&profile, name).map(|(v, _, _, _)| v).unwrap_or_default();
            if installed.is_empty() || !is_registry_spec(spec) {
                slots.push(Some(PluginUpdateInfo { name: name.clone(), installed, latest: String::new(), error: None }));
            } else {
                slots.push(None);
                pending.push((slots.len() - 1, name.clone(), installed));
            }
        }
        // 并发上限：npm view 是网络子进程，插件很多时避免瞬时拉起大量进程
        const NPM_VIEW_CONCURRENCY: usize = 4;
        std::thread::scope(|s| {
            for chunk in pending.chunks(NPM_VIEW_CONCURRENCY) {
                let mut handles = Vec::new();
                for &(slot, ref name, ref installed) in chunk {
                    let env = env.clone();
                    let name = name.clone();
                    let installed = installed.clone();
                    handles.push(s.spawn(move || (slot, match npm_view_version(&name, &env) {
                        Ok(latest) => PluginUpdateInfo { name, installed, latest, error: None },
                        Err(e) => PluginUpdateInfo { name, installed, latest: String::new(), error: Some(e) },
                    })));
                }
                for h in handles {
                    let (slot, info) = h.join().unwrap_or_else(|_| (
                        usize::MAX,
                        PluginUpdateInfo { name: String::new(), installed: String::new(), latest: String::new(), error: Some("检查线程异常".into()) },
                    ));
                    if slot != usize::MAX {
                        slots[slot] = Some(info);
                    }
                }
            }
        });
        let results: Vec<PluginUpdateInfo> = slots.into_iter().map(|s| s.unwrap_or_else(|| {
            PluginUpdateInfo { name: String::new(), installed: String::new(), latest: String::new(), error: Some("检查线程异常".into()) }
        })).collect();
        results
    })
    .await
    .unwrap_or_default()
}

/// 在 profile 的 cordis.patch.yml 中追加/移除本启动器管理的禁用块
/// （`- id: <entryId>` + `disabled: true`，带标记注释便于识别与移除）。
/// enable 时同时移除同 id 的任何 disabled 条目（含用户手动配置），
/// disable 时跳过已有禁用条目的 id。返回是否发生变更。
fn manage_disabled_blocks(path: &Path, name: &str, ids: &[String], enable: bool) -> Result<bool, String> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let marker = format!("# --- 由 DSH 启动器管理：禁用 {} ---", name);
    let mut changed = false;

    // 1) 移除本启动器写入的管理块（marker 及其后的 id/disabled/空行/注释行）
    let mut kept: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == marker {
            changed = true;
            i += 1;
            while i < lines.len() {
                let t = lines[i].trim();
                if t.starts_with("- id:") || t == "disabled: true" || t.is_empty() || t.starts_with('#') {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }
        kept.push(lines[i].clone());
        i += 1;
    }
    lines = kept;

    // 2) enable：移除任何 "- id: <目标>" + "disabled: ..." 组合（含用户手动条目）
    if enable {
        let mut out: Vec<String> = Vec::new();
        let mut j = 0;
        while j < lines.len() {
            let t = lines[j].trim();
            let is_target = ids.iter().any(|id| {
                t == format!("- id: {}", id)
                    || t == format!("- id: '{}'", id)
                    || t == format!("- id: \"{}\"", id)
            });
            if is_target {
                let next = lines.get(j + 1).map(|n| n.trim()).unwrap_or("");
                if next == "disabled: true" || next.starts_with("disabled:") {
                    changed = true;
                    j += 2;
                    continue;
                }
            }
            out.push(lines[j].clone());
            j += 1;
        }
        lines = out;
    }

    // 3) disable：为尚未有 disabled 条目的 id 追加管理块。
    //    注意：原文件可能是 `[]`（内联空数组）——在其后追加条目会生成非法
    //    YAML（"end of the stream or a document separator is expected"），
    //    导致整个 profile 解析失败、DSH 无法启动。此时必须整文件重写。
    if !enable {
        let stripped: String = lines.iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("");
        let empty_file = stripped.is_empty() || stripped == "[]";
        if empty_file {
            // 保留注释行、去掉空行与 []，直接接管理块（重写为合法条目列表）
            lines.retain(|l| l.trim().starts_with('#'));
            while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.pop();
            }
        }
        let mut to_add: Vec<String> = Vec::new();
        'outer: for id in ids {
            for (k, l) in lines.iter().enumerate() {
                let t = l.trim();
                if t == format!("- id: {}", id) || t == format!("- id: '{}'", id) || t == format!("- id: \"{}\"", id) {
                    if lines.get(k + 1).map(|n| n.trim() == "disabled: true").unwrap_or(false) {
                        continue 'outer; // 已有禁用条目，不重复添加
                    }
                }
            }
            to_add.push(id.clone());
        }
        if !to_add.is_empty() {
            if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            for id in to_add {
                lines.push(marker.clone());
                lines.push(format!("- id: {}", id));
                lines.push("  disabled: true".into());
            }
            changed = true;
        }
    }

    if changed {
        // 启用后若文件只剩注释（无任何条目），恢复 `[]` 占位保证 YAML 合法
        if enable {
            let stripped: String = lines.iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect::<Vec<_>>()
                .join("");
            if stripped.is_empty() {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.push("[]".into());
            }
        }
        let mut text = lines.join("\n");
        if !text.ends_with('\n') {
            text.push('\n');
        }
        std::fs::write(path, text).map_err(|e| format!("写入 cordis.patch.yml 失败：{}", e))?;
    }
    Ok(changed)
}

/// 启用/禁用插件：通过 profile 的 cordis.patch.yml 的 disabled 条目实现
/// （保留安装，仅停用；下次启动 DSH 生效）。
#[tauri::command]
async fn plugin_toggle(name: String, enable: bool) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        let (_, _, is_bundle, entry_ids) = installed_pkg_info(&profile, &name)
            .ok_or_else(|| format!("插件 {} 未安装", name))?;
        if !is_bundle {
            return Err(format!("{} 不是 profile 层插件（普通依赖库），不支持启用/禁用", name));
        }
        if entry_ids.is_empty() {
            return Err(format!("{} 的 bundle 未声明可管理的入口（insert id），无法启用/禁用", name));
        }
        let patch_path = profile.join("cordis.patch.yml");
        let changed = manage_disabled_blocks(&patch_path, &name, &entry_ids, enable)?;
        if !changed && !enable {
            return Err(format!("{} 已处于禁用状态", name));
        }
        if !changed && enable {
            return Err(format!("{} 未处于禁用状态", name));
        }
        Ok(if enable { "enabled" } else { "disabled" }.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDetail {
    name: String,
    version: String,
    spec: String,
    description: String,
    license: String,
    homepage: String,
    repository: String,
    is_bundle: bool,
    entry_ids: Vec<String>,
    readme: String,
}

/// 插件详情：元信息 + README 摘要（供展开面板展示）
#[tauri::command]
async fn plugin_detail(name: String) -> Result<PluginDetail, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let profile = profile_dir();
        let pkg_path = profile.join("node_modules").join(&name).join("package.json");
        let content = std::fs::read_to_string(&pkg_path).map_err(|_| format!("插件 {} 未安装", name))?;
        let v: serde_json::Value = serde_json::from_str(&content).map_err(|_| "插件配置解析失败".to_string())?;
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        // repository 可能是对象 {type, url}
        let repository = v.get("repository")
            .and_then(|r| r.as_str().map(String::from).or_else(|| r.get("url").and_then(|u| u.as_str()).map(String::from)))
            .unwrap_or_default();
        // spec：profile manifest 中声明的依赖范围
        let spec = std::fs::read_to_string(profile.join("package.json")).ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|m| m.get("dependencies").and_then(|d| d.get(&name)).and_then(|x| x.as_str()).map(String::from))
            .unwrap_or_default();
        let (_, _, is_bundle, entry_ids) = installed_pkg_info(&profile, &name).unwrap_or_default();
        // README 摘要（前 2000 字符，前端会清洗 markdown 后展示简介）
        let mut readme = String::new();
        let base = profile.join("node_modules").join(&name);
        for f in ["README.md", "readme.md", "README.MD", "README"] {
            if let Ok(c) = std::fs::read_to_string(base.join(f)) {
                readme = c.chars().take(2000).collect();
                if c.chars().count() > 2000 {
                    readme.push('…');
                }
                break;
            }
        }
        Ok(PluginDetail {
            name: name.clone(),
            version: get("version"),
            spec,
            description: get("description"),
            license: get("license"),
            homepage: get("homepage"),
            repository,
            is_bundle,
            entry_ids,
            readme,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// ShellExecuteW 打开目标（URL / 文件 / 目录）：不走 cmd /c start，
/// 避免目标含 & 等 cmd 元字符时被截断甚至二次解析执行
/// （实证：cmd /c start "" "http://x/?a=1&b=2" 会只打开 a=1 并把 b=2 当命令执行）
fn shell_open(target: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = wide("open");
    let target_w = wide(target);
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target_w.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// 用系统默认浏览器打开 URL（仅允许 http/https，防参数注入）
#[tauri::command]
fn open_url(url: String) -> Result<String, String> {
    let u = url.trim();
    if !u.starts_with("http://") && !u.starts_with("https://") {
        return Err("仅支持打开 http/https 链接".into());
    }
    shell_open(u);
    Ok("opened".into())
}

/// 读取 exe 旁 web.log 末尾（最大 256KB），用于失败排查
#[tauri::command]
fn read_web_log() -> (String, bool) {
    let path = web_log_path();
    if !path.is_file() {
        return (String::new(), false);
    }
    (tail_file(&path, 256 * 1024), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_extraction() {
        assert_eq!(bare_pkg_name("dsh-better-sidebar"), "dsh-better-sidebar");
        assert_eq!(bare_pkg_name("dsh-better-sidebar@0.12.2"), "dsh-better-sidebar");
        assert_eq!(bare_pkg_name("@zerro223/dsh-token-usage"), "@zerro223/dsh-token-usage");
        assert_eq!(bare_pkg_name("@zerro223/dsh-token-usage@1.0.0"), "@zerro223/dsh-token-usage");
        // 含斜杠的 spec（git / file: 等）保持原样，不做版本剥离
        assert_eq!(bare_pkg_name("file:../plugin"), "file:../plugin");
        assert_eq!(bare_pkg_name("git+ssh://git@github.com/x/y.git"), "git+ssh://git@github.com/x/y.git");
    }

    #[test]
    fn name_validation() {
        assert!(validate_plugin_name("dsh-better-sidebar").is_ok());
        assert!(validate_plugin_name("@scope/name@1.2.3").is_ok());
        assert!(validate_plugin_name("").is_err());
        assert!(validate_plugin_name("   ").is_err());
        assert!(validate_plugin_name("-x").is_err());
        assert!(validate_plugin_name("a b").is_err());
    }

    #[test]
    fn registry_spec_detection() {
        // registry 版本范围：可查 npm view
        assert!(is_registry_spec("^0.12.2"));
        assert!(is_registry_spec("0.1.0-rc.6"));
        assert!(is_registry_spec("~1.0.0"));
        // 本地 / VCS / URL 依赖：跳过检查
        assert!(!is_registry_spec("file:E:/work/dsh-token-usage"));
        assert!(!is_registry_spec("file:../plugin.tgz"));
        assert!(!is_registry_spec("link:../plugin"));
        assert!(!is_registry_spec("git+https://github.com/x/y.git"));
        assert!(!is_registry_spec("github:user/repo"));
        assert!(!is_registry_spec("https://example.com/pkg.tgz"));
    }

    /// 验证 cordis.patch.yml 的禁用/启用写入始终生成合法 YAML 结构
    /// （回归：曾在 `[]` 空数组后追加条目导致 DSH 解析失败、Web UI 无法启动）
    #[test]
    fn disabled_blocks_always_valid() {
        let dir = std::env::temp_dir().join(format!("dsh-launcher-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cordis.patch.yml");
        let ids = vec!["ui-skin-maid-atelier".to_string()];

        // 场景 1：空数组文件（[]）→ 禁用 → 必须整文件重写为条目列表
        std::fs::write(&path, "# 注释\n[]\n").unwrap();
        manage_disabled_blocks(&path, "maid-atelier", &ids, false).unwrap();
        let c1 = std::fs::read_to_string(&path).unwrap();
        assert!(c1.contains("- id: ui-skin-maid-atelier"), "禁用后应有条目: {}", c1);
        assert!(!c1.contains("[]"), "禁用后不应残留空数组: {}", c1);

        // 场景 2：启用 → 删除条目后恢复 [] 占位（保证 YAML 合法）
        manage_disabled_blocks(&path, "maid-atelier", &ids, true).unwrap();
        let c2 = std::fs::read_to_string(&path).unwrap();
        let stripped2: String = c2.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
        assert_eq!(stripped2, "[]", "启用后应恢复空数组: {}", c2);

        // 场景 3：已有条目列表的文件 → 追加新条目（保持合法）
        std::fs::write(&path, "- id: other\n  disabled: true\n").unwrap();
        manage_disabled_blocks(&path, "maid-atelier", &ids, false).unwrap();
        let c3 = std::fs::read_to_string(&path).unwrap();
        assert!(c3.contains("- id: ui-skin-maid-atelier"));
        assert!(c3.contains("- id: other"));

        // 场景 4：追加模式下禁用后再启用，保留其他条目且无重复
        manage_disabled_blocks(&path, "maid-atelier", &ids, true).unwrap();
        let c4 = std::fs::read_to_string(&path).unwrap();
        assert!(!c4.contains("ui-skin-maid-atelier"));
        assert!(c4.contains("- id: other"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 备份文件名时间戳 → 显示格式（yyyy-MM-dd HH:mm）的解析
    #[test]
    fn backup_time_formatting() {
        assert_eq!(format_backup_time("dsh-backup-2026-08-16_113023.zip"), "2026-08-16 11:30");
        assert_eq!(format_backup_time("pre-restore-2026-08-16_113023.zip"), "2026-08-16 11:30");
        assert_eq!(format_backup_time("random.zip"), "");
        assert_eq!(format_backup_time("dsh-backup-2026-08-16_1130.zip"), ""); // 时间戳长度不足
        assert_eq!(format_backup_time("dsh-backup-2026-08-16_11302x.zip"), ""); // 含非数字
        assert_eq!(format_backup_time("dsh-backup-20260816_113023.zip"), ""); // 日期部分含下划线
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
            let tail = lines.iter().rev().take(15).rev().cloned().collect::<Vec<_>>().join("\n");
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
async fn start_tui(proxy_on: bool, proxy_addr: String, progress: Channel<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // TUI 不是 DSH 内置模式，而是独立插件 @deepseek-harness-tui/dsh-tui
        // （官方公众号收录，MIT）；其 profile 名为 dsh-tui（不是 tui）。
        // 首次点击时自动初始化并安装，之后直接启动。
        let profile_dir = dsh_home_dir().join("profiles").join("dsh-tui");
        if !profile_dir.join("package.json").is_file() {
            let _ = progress.send("TUI 未安装，正在自动安装 @deepseek-harness-tui/dsh-tui …".to_string());
            run_profile_cmd("dsh-tui", &["add", "@deepseek-harness-tui/dsh-tui"], proxy_on, &proxy_addr, &progress)
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
                if diag.is_empty() { String::new() } else { format!("\n\n--- 诊断输出 ---\n{}", diag) }
            ));
        }
        Ok(format!("TUI 已启动（PID {}），请在弹出的命令行窗口中操作", pid))
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
            untrack_dsh_pid(pid);
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

/// 关闭 Web 界面：仅终止监听 webPort 的 DSH 进程（带命令行身份校验），
/// 等待端口释放后返回，不重新启动。
#[tauri::command]
async fn stop_web() -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let pids = find_dsh_pids(settings().web_port);
        if pids.is_empty() {
            return Ok("not-running".to_string());
        }
        let bin = find_bin().unwrap_or_default();
        let mut killed = 0usize;
        for pid in filter_dsh_pids(&pids, &bin) {
            untrack_dsh_pid(pid);
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid.to_string()])
                .creation_flags(CREATE_NO_WINDOW)
                .stdout(Stdio::null()).stderr(Stdio::null())
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
async fn fix_commands() -> Vec<String> {
    // 缓存未命中时会 spawn where/npm 子进程，必须放后台线程（同步命令在主线程执行会卡 UI）
    tauri::async_runtime::spawn_blocking(|| {
        let mut cmds = Vec::new();
        if !which("node") {
            cmds.push("请先安装 Node.js（官网下载 LTS 版）：https://nodejs.org/".to_string());
        }
        if find_dsh().is_none() {
            cmds.push("npm install -g @deepseek-ai/dsh".to_string());
        }
        cmds
    })
    .await
    .unwrap_or_default()
}

#[tauri::command]
fn open_install_dir() -> String {
    match find_dsh() {
        Some(inst) => {
            shell_open(&inst.pkg_root);
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
// 单实例锁（Windows 命名互斥体）
// 不引入 tauri-plugin-single-instance 依赖（离线构建受限），
// 直接使用 windows-sys 的 CreateMutexW：重复启动时激活已有主窗口并退出。
// ---------------------------------------------------------------------------
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 获取单实例互斥体。已有实例运行时：激活其主窗口并返回 false（本进程应退出）。
fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};
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

// ---------------------------------------------------------------------------
// 托盘与窗口行为
// ---------------------------------------------------------------------------
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        // Win32 兜底：外部隐藏（如托盘关闭时的 Win32 路径）可能使 tao 内部
        // 可见性状态与实际不同步，直接 ShowWindow 确保窗口一定显示
        if let Ok(hwnd) = w.hwnd() {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(hwnd.0 as _, 5); // SW_SHOW
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
            for px in rgba.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            rgba = out;
        }
        _ => return None,
    }
    Some(tauri::image::Image::new_owned(rgba, info.width, info.height))
}

/// 系统托盘：左键单击或菜单「打开主窗口」显示窗口；「退出」才真正结束进程。
/// 窗口关闭按钮改为隐藏到托盘（后台保持状态轮询）。
fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
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

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 单实例：已有实例则激活其窗口并退出本进程
    if !acquire_single_instance() {
        return;
    }
    tauri::Builder::default()
        .setup(|app| {
            setup_tray(app)?;
            // 后台清理崩溃遗留的备份临时目录（不阻塞启动）
            std::thread::spawn(cleanup_stale_temp_dirs);
            // 关闭窗口 = 隐藏到后台（托盘常驻），真正退出走托盘菜单「退出」。
            // on_window_event 的注册走异步消息，setup 期间窗口可能未就绪导致
            // 监听被丢弃（时序竞态），因此延迟 500ms 注册；前端 onCloseRequested
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
            checks, status, start_web, open_browser, start_tui, start_headless,
            restart_dsh, stop_web, update_check, update_dsh, install_dsh,
            get_system_proxy_cmd, fix_commands, open_install_dir, get_web_cmd, get_web_url,
            plugin_list, plugin_install, plugin_remove, plugin_update,
            plugin_check_updates, plugin_toggle, plugin_detail, open_url, read_web_log,
            get_settings, save_settings, get_autostart, set_autostart, list_profiles,
            backup_dsh, restore_dsh, list_backups, open_backups_dir
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
