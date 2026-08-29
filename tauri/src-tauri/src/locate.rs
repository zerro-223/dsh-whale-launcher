//! DSH 安装位置自动识别与 $DSH_HOME 定位。
//!
//! 识别顺序（找到即返回，结果缓存；只缓存命中，未命中每次都重扫，
//! 便于安装完成后立即被识别）：
//!   1. settings.json 显式配置的 dDrivePath / junctionPath（向后兼容）
//!   2. npm 全局安装（npm root -g 目录下的 @deepseek-ai\dsh）
//!   3. npx 缓存目录扫描：npm 配置的 cache、默认 %LocalAppData%\npm-cache、
//!      以及 settings 路径下的 _npx\<hash>（取最新含 DSH 的一个）
//!
//! 不再强制要求"挪到 D 盘 + junction"的布局，任何安装位置都能工作。

use std::path::{Path, PathBuf};

use crate::npm::run_npm;
use crate::settings::settings;

pub(crate) const DSH_PKG_NAME: &str = "@deepseek-ai/dsh";
pub(crate) const DSH_BIN_REL: &str = r"node_modules\@deepseek-ai\dsh\lib\bin.js";

#[derive(Clone)]
pub(crate) struct DshInstall {
    pub(crate) bin: String,      // lib\bin.js 完整路径
    pub(crate) pkg_root: String, // 安装根目录（node_modules 的父目录，如 _npx\<hash>）
    pub(crate) is_global: bool,  // 是否为 npm 全局安装
}

static DSH_CACHE: std::sync::Mutex<Option<DshInstall>> = std::sync::Mutex::new(None);

pub(crate) fn clear_dsh_cache() {
    *crate::util::lock_ok(&DSH_CACHE) = None;
}

pub(crate) fn find_dsh() -> Option<DshInstall> {
    if let Some(inst) = crate::util::lock_ok(&DSH_CACHE).as_ref() {
        return Some(inst.clone());
    }
    let found = find_dsh_uncached();
    if found.is_some() {
        *crate::util::lock_ok(&DSH_CACHE) = found.clone();
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
    let pkg_root = p
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|r| r.to_string_lossy().to_string())
        .unwrap_or_default();
    DshInstall {
        bin,
        pkg_root,
        is_global,
    }
}

fn npm_root_global() -> Option<String> {
    let out = run_npm(&["root", "-g"]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) fn npm_config_cache() -> Option<String> {
    let out = run_npm(&["config", "get", "cache"]).ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "undefined" {
        None
    } else {
        Some(s)
    }
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
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            hits.push((mtime, path));
        }
    }
    hits.sort_by_key(|(t, _)| *t);
    hits.last()
        .map(|(_, p)| p.join(DSH_BIN_REL).to_string_lossy().to_string())
}

pub(crate) fn find_bin() -> Option<String> {
    find_dsh().map(|i| i.bin)
}

/// DSH 数据目录：与 dsh-home-paths 一致，$DSH_HOME（非空）> %USERPROFILE%\.dsh
pub(crate) fn dsh_home_dir() -> PathBuf {
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

pub(crate) fn web_url() -> String {
    format!("http://127.0.0.1:{}", settings().web_port)
}

pub(crate) fn get_installed_dsh_version() -> Option<String> {
    let install = find_dsh()?;
    let p = Path::new(&install.pkg_root).join(r"node_modules\@deepseek-ai\dsh\package.json");
    let content = std::fs::read_to_string(p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("version")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

/// 扫描 $DSH_HOME/profiles/ 下的已初始化 profile（设置页下拉用）
#[tauri::command]
pub(crate) fn list_profiles() -> Vec<String> {
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

/// 打开 DSH 安装目录（快捷工具）
#[tauri::command]
pub(crate) async fn open_install_dir() -> String {
    // find_dsh 未命中时会同步跑 where/npm 子进程，必须放后台线程
    // （同步命令在 Tauri 主线程执行，npm 挂起会冻结整个窗口）
    tauri::async_runtime::spawn_blocking(|| match find_dsh() {
        Some(inst) => {
            crate::util::shell_open(&inst.pkg_root);
            format!("已打开 {}", inst.pkg_root)
        }
        None => "未找到 DSH 安装目录（尚未安装）".to_string(),
    })
    .await
    .unwrap_or_else(|_| "打开安装目录失败".to_string())
}

#[cfg(test)]
mod tests {
    use super::make_install;

    #[test]
    fn pkg_root_is_five_levels_above_bin() {
        let inst = make_install(
            r"C:\cache\_npx\abc\node_modules\@deepseek-ai\dsh\lib\bin.js".to_string(),
            false,
        );
        assert_eq!(inst.pkg_root, r"C:\cache\_npx\abc");
        assert!(!inst.is_global);
    }
}
