//! DSH 数据备份 / 恢复。
//!
//! 备份 = 镜像打包 $DSH_HOME（排除 node_modules）到 exe 旁 backups/ 目录，
//! 文件名 dsh-backup-<yyyy-MM-dd_HHmmss>.zip，仅保留最近 5 份；
//! 恢复 = 校验备份文件 → 自动备份现状（pre-restore-*，保留 2 份）→
//! 解压校验（防 ZipSlip）→ 改名换位替换 $DSH_HOME → 对每个 profile 执行
//! pnpm install 重建插件依赖。备份/恢复互斥；备份只拦截启动器自己启动的
//! DSH 实例（端口可能被无关服务占用），恢复则任何端口占用都保守拦截。

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

use serde::Serialize;
use tauri::ipc::Channel;
use zip::read::ZipArchive;
use zip::write::SimpleFileOptions;

use crate::locate::dsh_home_dir;
use crate::npm::{apply_registry, run_cmd_streaming, which};
use crate::process::{dsh_running_for_backup, dsh_running_for_restore};
use crate::proxy::npm_proxy_env;
use crate::util::{now_stamp, OpGuard, CREATE_NO_WINDOW};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupInfo {
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

/// 执行 robocopy 镜像复制（排除 node_modules）。robocopy 退出码 0-7 均表示
/// 成功（复制了文件 / 无变化等），只有 >=8 才是真实失败。
/// /XF 排除备份清单本身：恢复方向不把清单复制进 $DSH_HOME，备份方向
/// 也不会把上次遗留的旧清单误当作数据复制。
fn run_robocopy(src: &Path, dst: &Path) -> Result<(), String> {
    let status = Command::new("robocopy")
        .arg(src)
        .arg(dst)
        .args([
            "/E",
            "/XD",
            "node_modules",
            "/XF",
            "dsh-backup.json",
            "/XJ",
            "/NFL",
            "/NDL",
            "/NJH",
            "/NJS",
            "/R:1",
            "/W:1",
        ])
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

/// 递归收集 src 下的全部条目（相对 zip 名, 绝对路径, 是否目录）。
/// 目录条目也写入 zip，保证空目录（如空的 storages 子目录）在恢复后不丢失；
/// 跳过符号链接（robocopy /XJ 已排除 junction，这里兜底防循环）。
fn collect_zip_entries(
    dir: &Path,
    base: &Path,
    out: &mut Vec<(String, PathBuf, bool)>,
) -> std::io::Result<()> {
    let mut entries: Vec<std::fs::DirEntry> =
        std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let ft = e.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if ft.is_dir() {
            out.push((format!("{}/", rel), path.clone(), true));
            collect_zip_entries(&path, base, out)?;
        } else {
            out.push((rel, path, false));
        }
    }
    Ok(())
}

/// 把 src 目录打包为 zip（stored/deflate，目录条目含空目录），
/// 返回 (条目数, 文件总字节数)——后者用于向用户报告备份规模，
/// 大 $DSH_HOME 打包期间前端才不会看起来像卡死。
fn zip_dir_contents(src: &Path, zip_path: &Path) -> Result<(usize, u64), String> {
    let mut entries = Vec::new();
    collect_zip_entries(src, src, &mut entries).map_err(|e| format!("遍历备份内容失败：{}", e))?;
    let total_bytes: u64 = entries
        .iter()
        .filter(|(_, _, is_dir)| !*is_dir)
        .filter_map(|(_, p, _)| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let file = std::fs::File::create(zip_path).map_err(|e| format!("创建备份文件失败：{}", e))?;
    let mut zw = zip::ZipWriter::new(file);
    // large_file：单条目 >4GB 时自动启用 zip64（会话附件目录可能很大）
    let opts = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(true);
    for (name, path, is_dir) in &entries {
        if *is_dir {
            zw.add_directory(name.clone(), opts)
                .map_err(|e| format!("写入目录条目 {} 失败：{}", name, e))?;
            continue;
        }
        zw.start_file(name.clone(), opts)
            .map_err(|e| format!("写入条目 {} 失败：{}", name, e))?;
        let mut f = std::fs::File::open(path)
            .map_err(|e| format!("读取 {} 失败：{}", path.display(), e))?;
        std::io::copy(&mut f, &mut zw)
            .map_err(|e| format!("压缩 {} 失败：{}", path.display(), e))?;
    }
    zw.finish().map_err(|e| format!("完成压缩失败：{}", e))?;
    Ok((entries.len(), total_bytes))
}

/// 字节数的人类可读形式（备份进度展示用；只区分 KB / MB）
fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

/// 可选进度上报（备份/恢复共用 pack_dsh_home；测试直接传 None）
fn send_progress(progress: Option<&Channel<String>>, msg: &str) {
    if let Some(ch) = progress {
        let _ = ch.send(msg.to_string());
    }
}

/// 写入 dsh-backup.json 清单（恢复时校验"这是启动器的备份"而非任意 zip）
fn write_backup_manifest(dir: &Path, ts: &str) -> Result<(), String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            names.push(e.file_name().to_string_lossy().to_string());
        }
    }
    names.sort();
    let manifest = serde_json::json!({ "app": "dsh-launcher", "time": ts, "entries": names });
    let text =
        serde_json::to_string(&manifest).map_err(|e| format!("序列化备份清单失败：{}", e))?;
    std::fs::write(dir.join("dsh-backup.json"), text)
        .map_err(|e| format!("写入备份清单失败：{}", e))
}

/// 镜像打包 $DSH_HOME 到指定 zip：robocopy 复制（排除 node_modules）到
/// %TEMP% 唯一目录 → 写入 dsh-backup.json 清单 → zip crate 压缩。
/// 临时目录无论成败都清理。backup_dsh 与 restore_dsh 的自动备份共用。
/// （此前经 PowerShell 调 .NET ZipArchive，现已换 zip crate：无脚本拼接
/// 转义面，规避 PS 5.1 通配符/2GB 条目上限，且打包逻辑可单元测试）
fn pack_dsh_home(
    home: &Path,
    zip_path: &Path,
    progress: Option<&Channel<String>>,
) -> Result<(), String> {
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
        send_progress(
            progress,
            "正在复制 DSH 数据到临时目录（排除 node_modules）…",
        );
        run_robocopy(home, &temp)?;
        write_backup_manifest(&temp, &ts)?;
        send_progress(progress, "正在压缩…（数据量较大时需要一段时间）");
        let (count, bytes) = zip_dir_contents(&temp, zip_path)?;
        // 压缩自检：zip 存在、中央目录可读且至少含一个条目
        if !zip_path.is_file() {
            return Err("压缩完成但未生成备份文件".into());
        }
        let verify = std::fs::File::open(zip_path)
            .map_err(|e| format!("压缩完成但备份文件不可读：{}", e))?;
        let n = ZipArchive::new(verify)
            .map_err(|e| format!("压缩产物校验失败：{}", e))?
            .len();
        if n == 0 {
            return Err("压缩完成但备份为空".into());
        }
        let zip_size = std::fs::metadata(zip_path).map(|m| m.len()).unwrap_or(0);
        send_progress(
            progress,
            &format!(
                "压缩完成：{} 个条目（原始 {}）→ 备份文件 {}",
                count,
                human_size(bytes),
                human_size(zip_size)
            ),
        );
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&temp);
    result
}

/// zip 条目名安全校验（防 ZipSlip / Windows 非法字符 / 保留设备名）。
/// 返回条目在目标目录内的相对路径；不安全时返回 None。
/// `..` 等在路径段级别判断（zip crate 的 Path components 解析），
/// 比旧版 PowerShell 对整串模糊 `-match '\.\.'` 更精确：
/// "a..b.txt" 这类合法文件名不再被误拒。
fn sanitize_entry_name(name: &str) -> Option<PathBuf> {
    let name = name.replace('\\', "/");
    if name.is_empty() || name.starts_with('/') || name.contains('\0') {
        return None;
    }
    if name
        .chars()
        .any(|c| matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control())
    {
        return None;
    }
    let mut rel = PathBuf::new();
    for comp in Path::new(&name).components() {
        match comp {
            std::path::Component::Normal(seg) => {
                let seg = seg.to_string_lossy();
                // 保留设备名（CON / NUL / COM1-9 / LPT1-9 等，含带扩展名形式）
                let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
                let reserved = matches!(
                    stem.as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                );
                if reserved {
                    return None;
                }
                rel.push(seg.as_ref());
            }
            std::path::Component::CurDir => {}
            // ParentDir / RootDir / Prefix（盘符、UNC）一律拒绝
            _ => return None,
        }
    }
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel)
    }
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

/// 解压上限：条目数与解压总量（预扫描按声明大小 fail-fast，实际解压时
/// 再按真实写入字节 enforce——声明大小可伪造，实际计数不可绕过）
const MAX_ENTRIES: usize = 200_000;
const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// 解压 zip 到临时目录。逐条目经 sanitize_entry_name 校验后展开（防 ZipSlip
/// / Windows 非法字符 / 保留设备名），同时拒绝条目数超限、解压总量超 8GB
/// 的解压炸弹。使用 zip crate 纯 Rust 实现（此前经 PS 5.1 Expand-Archive，
/// 其自身不防 ZipSlip，需要脚本内逐条校验）。
fn extract_zip_safe(zip: &Path, dst: &Path) -> Result<(), String> {
    let file = std::fs::File::open(zip)
        .map_err(|e| format!("无法读取备份文件 {}：{}", zip.display(), e))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("备份文件无法解析：{}", e))?;
    if archive.len() > MAX_ENTRIES {
        return Err("备份文件条目过多，已中止解压".into());
    }
    // 先扫描一遍：条目名校验与声明总量上限在解压任何文件之前完成（fail-fast，
    // 避免解压到一半才发现恶意条目）
    let mut declared: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| format!("备份条目 #{} 无法读取：{}", i, e))?;
        if sanitize_entry_name(entry.name()).is_none() {
            return Err(format!(
                "备份文件包含不安全条目，已中止解压：{}",
                entry.name()
            ));
        }
        declared = declared.saturating_add(entry.size());
        if declared > MAX_TOTAL_BYTES {
            return Err(format!(
                "备份解压总量超过 8GB 上限，已中止：{} 字节",
                declared
            ));
        }
    }
    // 实际解压：累计真实写入字节（条目声明大小可伪造，按声明放行的炸弹
    // 在这里被硬性拦截；中途失败由调用方清理临时目录）
    let mut written: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("备份条目 #{} 无法读取：{}", i, e))?;
        let rel = sanitize_entry_name(entry.name())
            .ok_or_else(|| format!("备份文件包含不安全条目，已中止解压：{}", entry.name()))?;
        let dest = dst.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest)
                .map_err(|e| format!("创建目录 {} 失败：{}", dest.display(), e))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{}", parent.display(), e))?;
        }
        let out = std::fs::File::create(&dest)
            .map_err(|e| format!("创建文件 {} 失败：{}", dest.display(), e))?;
        let n = std::io::copy(&mut entry, &mut std::io::BufWriter::new(out)).map_err(|e| {
            format!(
                "解压条目 {} 失败（可能为不支持的压缩格式）：{}",
                rel.display(),
                e
            )
        })?;
        written = written.saturating_add(n);
        if written > MAX_TOTAL_BYTES {
            return Err(format!(
                "解压实际总量超过 8GB 上限，已中止：{} 字节",
                written
            ));
        }
    }
    Ok(())
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
            return Err(format!(
                "DSH_HOME 是用户目录的上级目录，已中止：{}",
                home_str
            ));
        }
    }
    // 目录已存在且非空时，必须含 DSH 数据特征
    if home.is_dir() {
        let non_empty = std::fs::read_dir(home)
            .map(|it| it.flatten().next().is_some())
            .unwrap_or(false);
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
            format!(
                "恢复复制失败：{}\n原数据已移动到 {}，请手动恢复",
                e,
                bak.display()
            )
        });
    }
    // 复制成功：尽力删除本次 .bak；删除失败不影响恢复结果，仅提示
    let mut notes: Vec<String> = Vec::new();
    if std::fs::remove_dir_all(&bak).is_err() {
        notes.push(format!("恢复前数据已保留在 {}，可手动删除", bak.display()));
    }
    if let Some(note) = stale_bak_note {
        notes.push(note);
    }
    let note = if notes.is_empty() {
        None
    } else {
        Some(notes.join("\n"))
    };
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
        cmd.arg("install")
            .current_dir(dir)
            .creation_flags(CREATE_NO_WINDOW);
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

/// 备份/恢复互斥：同一时刻只允许一个备份或恢复操作（防同秒撞文件名、
/// 临时目录互踩、并发读写 DSH_HOME）
static BACKUP_OP: AtomicBool = AtomicBool::new(false);

/// 启动时清理进程崩溃/被杀遗留的备份临时目录
/// （%TEMP%\dsh-pack-* / dsh-restore-*）。仅删除修改时间超过 24h 的目录，
/// 避免极端情况下误删仍在使用的目录；启动器有单实例保护，正常不会并发。
pub(crate) fn cleanup_stale_temp_dirs() {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now_secs.saturating_sub(24 * 3600);
    let temp = std::env::temp_dir();
    for prefix in ["dsh-pack-", "dsh-restore-"] {
        let Ok(entries) = std::fs::read_dir(&temp) else {
            return;
        };
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

/// 创建一份新备份（dsh-backup-<ts>.zip），保留最近 5 份。
/// 进度经 Channel 推送：打包大 $DSH_HOME 可能耗时数分钟，
/// 没有阶段反馈时界面上只有一个置灰按钮，看起来像卡死。
#[tauri::command]
pub(crate) async fn backup_dsh(progress: Channel<String>) -> Result<BackupInfo, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = OpGuard::acquire(&BACKUP_OP, "另一项备份/恢复操作正在进行，请稍候再试")?;
        if dsh_running_for_backup() {
            return Err(
                "启动器启动的 DSH 正在运行，请先停止 DSH 再进行备份（运行中备份可能不完整）".into(),
            );
        }
        let home = dsh_home_dir();
        let _ = progress.send(format!("备份数据目录：{}", home.display()));
        let ts = now_stamp()?;
        let backups = backups_dir();
        std::fs::create_dir_all(&backups).map_err(|e| format!("创建备份目录失败：{}", e))?;
        let file_name = format!("dsh-backup-{}.zip", ts);
        pack_dsh_home(&home, &backups.join(&file_name), Some(&progress))?;
        cleanup_old_backups("dsh-backup-", 5);
        let size = std::fs::metadata(backups.join(&file_name))
            .map(|m| m.len())
            .unwrap_or(0);
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
pub(crate) fn list_backups() -> Vec<BackupInfo> {
    list_backup_files()
}

/// 恢复备份：解压覆盖 $DSH_HOME 并自动重建插件。
/// 恢复前自动备份现状（pre-restore-<ts>.zip，保留最近 2 份），可随时回退。
#[tauri::command]
pub(crate) async fn restore_dsh(
    file_name: String,
    proxy_on: bool,
    proxy_addr: String,
    progress: Channel<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        restore_dsh_impl(&file_name, proxy_on, &proxy_addr, &progress)
    })
    .await
    .map_err(|e| e.to_string())?
}

fn restore_dsh_impl(
    file_name: &str,
    proxy_on: bool,
    proxy_addr: &str,
    progress: &Channel<String>,
) -> Result<String, String> {
    let _guard = OpGuard::acquire(&BACKUP_OP, "另一项备份/恢复操作正在进行，请稍候再试")?;

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
    let canon_zip = zip
        .canonicalize()
        .map_err(|_| "无效的备份文件".to_string())?;
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
            pack_dsh_home(
                &dsh_home_dir(),
                &backups.join(format!("pre-restore-{}.zip", ts)),
                Some(progress),
            )?;
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
pub(crate) fn open_backups_dir() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 备份文件名时间戳 → 显示格式（yyyy-MM-dd HH:mm）的解析
    #[test]
    fn backup_time_formatting() {
        assert_eq!(
            format_backup_time("dsh-backup-2026-08-16_113023.zip"),
            "2026-08-16 11:30"
        );
        assert_eq!(
            format_backup_time("pre-restore-2026-08-16_113023.zip"),
            "2026-08-16 11:30"
        );
        assert_eq!(format_backup_time("random.zip"), "");
        assert_eq!(format_backup_time("dsh-backup-2026-08-16_1130.zip"), ""); // 时间戳长度不足
        assert_eq!(format_backup_time("dsh-backup-2026-08-16_11302x.zip"), ""); // 含非数字
        assert_eq!(format_backup_time("dsh-backup-20260816_113023.zip"), ""); // 日期部分含下划线
    }

    /// zip 条目名安全校验：正常名通过，ZipSlip / 非法字符 / 设备名拒绝
    #[test]
    fn entry_name_sanitization() {
        let ok = |n: &str| sanitize_entry_name(n).is_some();
        let bad = |n: &str| sanitize_entry_name(n).is_none();
        // 正常条目（含 @scope 层级、中文、合法的双点文件名）
        assert!(ok("settings.yaml"));
        assert!(ok("profiles/web/package.json"));
        assert!(ok("sessions\\a\\b.json")); // 反斜杠归一化为分隔符
        assert!(ok("a..b.txt"));
        assert!(ok("数据/会话.json"));
        // ZipSlip 与越界
        assert!(bad("../evil.txt"));
        assert!(bad("a/../../evil.txt"));
        assert!(bad("/abs.txt"));
        assert!(bad("\\abs.txt"));
        assert!(bad("C:/evil.txt"));
        assert!(bad("a/b:c.txt"));
        // Windows 非法字符与保留设备名（大小写/带扩展名形式）
        assert!(bad("a<b.txt"));
        assert!(bad("con.txt"));
        assert!(bad("NUL"));
        assert!(bad("com7.log"));
        assert!(bad(""));
    }

    /// 打包 → 解压 往返：文件内容、目录结构（含空目录）完整还原
    #[test]
    fn zip_round_trip() {
        let dir = std::env::temp_dir().join(format!("dsh-zip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("empty-dir")).unwrap();
        std::fs::create_dir_all(src.join("profiles/web")).unwrap();
        std::fs::write(src.join("settings.yaml"), "key: value\n中文内容").unwrap();
        std::fs::write(src.join("profiles/web/package.json"), "{}").unwrap();
        let zip = dir.join("t.zip");
        let (n, bytes) = zip_dir_contents(&src, &zip).unwrap();
        assert!(n >= 4, "应包含文件与目录条目：{}", n);
        assert!(bytes > 0, "应统计出非零原始字节数：{}", bytes);
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        extract_zip_safe(&zip, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("settings.yaml")).unwrap(),
            "key: value\n中文内容"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("profiles/web/package.json")).unwrap(),
            "{}"
        );
        assert!(out.join("empty-dir").is_dir(), "空目录应被保留");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 恶意 zip（ZipSlip 条目）必须在写出任何文件前被拒绝
    #[test]
    fn zip_slip_rejected() {
        let dir = std::env::temp_dir().join(format!("dsh-zip-slip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("evil.zip");
        let f = std::fs::File::create(&zip).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = SimpleFileOptions::default();
        zw.start_file("../evil.txt", opts).unwrap();
        std::io::Write::write_all(&mut zw, b"pwned").unwrap();
        zw.finish().unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert!(extract_zip_safe(&zip, &out).is_err());
        assert!(!dir.join("evil.txt").exists(), "不得写出目标目录之外的文件");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// find_content_root：清单 / DSH 特征 / 单层包装 zip 三种布局识别
    #[test]
    fn content_root_detection() {
        let dir = std::env::temp_dir().join(format!("dsh-root-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 布局 1：有清单
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dsh-backup.json"), "{}").unwrap();
        assert_eq!(find_content_root(&dir).as_deref(), Some(dir.as_path()));
        // 布局 2：无清单但有 DSH 特征
        let dir2 = dir.join("markers");
        std::fs::create_dir_all(dir2.join("profiles")).unwrap();
        std::fs::write(dir2.join("settings.yaml"), "").unwrap();
        assert_eq!(find_content_root(&dir2).as_deref(), Some(dir2.as_path()));
        // 布局 3：恰一个子目录且内含 DSH 数据（"打包了 DSH_HOME 本身"）
        let wrap = dir.join("wrap");
        let inner = wrap.join(".dsh");
        std::fs::create_dir_all(inner.join("profiles")).unwrap();
        std::fs::write(inner.join("settings.yaml"), "").unwrap();
        assert_eq!(find_content_root(&wrap).as_deref(), Some(inner.as_path()));
        // 无特征：拒绝
        let plain = dir.join("plain");
        std::fs::create_dir_all(plain.join("x")).unwrap();
        std::fs::create_dir_all(plain.join("y")).unwrap();
        assert_eq!(find_content_root(&plain), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// validate_home_target：盘符根 / 用户目录拒绝，普通 DSH 形态目录放行
    #[test]
    fn home_target_validation() {
        assert!(validate_home_target(Path::new("relative/path")).is_err());
        assert!(validate_home_target(Path::new("C:\\")).is_err());
        assert!(validate_home_target(Path::new("C:/")).is_err());
        if let Ok(up) = std::env::var("USERPROFILE") {
            assert!(validate_home_target(Path::new(&up)).is_err());
            assert!(validate_home_target(Path::new(&format!("{}\\Desktop", up))).is_err());
        }
        assert!(validate_home_target(Path::new(&std::env::temp_dir())).is_err());
        // 不存在的普通绝对路径：允许（全新安装场景）
        let fresh = std::env::temp_dir().join(format!("dsh-home-new-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        assert!(validate_home_target(&fresh).is_ok());
        // 存在但无 DSH 特征的非空目录：拒绝（防止误删用户数据）
        let foreign = std::env::temp_dir().join(format!("dsh-home-foreign-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&foreign);
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("随便.txt"), "").unwrap();
        assert!(validate_home_target(&foreign).is_err());
        // 有 DSH 特征：放行
        std::fs::create_dir_all(foreign.join("profiles")).unwrap();
        std::fs::write(foreign.join("settings.yaml"), "").unwrap();
        assert!(validate_home_target(&foreign).is_ok());
        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&foreign);
    }

    /// 备份清单必须是含 app 标识的合法 JSON（恢复校验依赖该标识）
    #[test]
    fn manifest_is_valid_json_with_app_marker() {
        let dir = std::env::temp_dir().join(format!("dsh-manifest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("中 文'引号'.yaml"), "x").unwrap();
        write_backup_manifest(&dir, "2026-08-29_120000").unwrap();
        let text = std::fs::read_to_string(dir.join("dsh-backup.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).expect("清单必须是合法 JSON");
        assert_eq!(v["app"], "dsh-launcher");
        assert_eq!(v["time"], "2026-08-29_120000");
        assert!(
            v["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e == "中 文'引号'.yaml"),
            "含空格/引号的文件名必须被正确转义"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
