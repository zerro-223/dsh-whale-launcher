// DSH 启动器前端（Tauri 2）
// 通过 window.__TAURI__.core.invoke 调用 Rust 后端，localStorage 持久化配置。
const { invoke } = window.__TAURI__.core;
const CFG_KEY = "dshLauncher.v1";

// Web 服务地址：启动时从后端获取（按 settings.json webPort 计算），前端不硬编码
let webUrl = "http://127.0.0.1:3080";

// ---------------- 配置 ----------------
let cfg = loadCfg();
function loadCfg() {
  try {
    const raw = localStorage.getItem(CFG_KEY);
    if (raw) return Object.assign({ theme: "dark", proxyEnabled: false, proxyAddr: "http://127.0.0.1:7890", history: [] }, JSON.parse(raw));
  } catch (e) { /* ignore */ }
  return { theme: "dark", proxyEnabled: false, proxyAddr: "http://127.0.0.1:7890", history: [] };
}
function saveCfg() {
  localStorage.setItem(CFG_KEY, JSON.stringify(cfg));
}

// ---------------- 主题 ----------------
function applyTheme() {
  document.documentElement.dataset.theme = cfg.theme;
  themeBtn.textContent = cfg.theme === "dark" ? "浅色模式" : "深色模式";
}
themeBtn.addEventListener("click", () => {
  cfg.theme = cfg.theme === "dark" ? "light" : "dark";
  applyTheme(); saveCfg(); setActivity("已切换为" + (cfg.theme === "dark" ? "深色" : "浅色") + "模式");
});

// ---------------- 工具 ----------------
const $ = (id) => document.getElementById(id);
const activity = $("activity");
function setActivity(msg) { activity.textContent = msg; }

function setPill(text, running) {
  const pill = $("statusPill");
  pill.textContent = text;
  pill.dataset.running = String(running);
}
function toast(title, msg) { /* 简化：活动栏提示 */ setActivity(msg); }
// 后端错误信息可能含多行日志末尾，压成一行并截断（完整内容见 exe 旁 web.log）
function cleanMsg(e) {
  const s = String(e).replace(/\s*\n+\s*/g, " · ");
  return s.length > 160 ? "…" + s.slice(-157) : s;
}

// 自检状态图标（内联 SVG，随 currentColor 着色，颜色由 CSS 状态类控制）
const ICONS = {
  OK: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 8.3l3.4 3.4L13 4.8"/></svg>',
  FAIL: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7"/></svg>',
  WARN: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2.6L14.2 13H1.8L8 2.6z"/><path d="M8 6.8v3.4"/><path d="M8 11.6v.2"/></svg>',
  INFO: '<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"><circle cx="8" cy="8" r="5.6"/><path d="M8 7.4v3.2"/><path d="M8 5.2v.2"/></svg>',
};

// ---------------- 自检 ----------------
async function refreshChecks() {
  setActivity("正在自检…");
  let res;
  try { res = await invoke("checks"); } catch (e) { setActivity("自检失败：" + e); return; }
  const box = $("checks");
  box.innerHTML = "";
  for (const item of res.items) {
    const row = document.createElement("div");
    row.className = "checkRow";
    row.innerHTML = `<span class="checkIcon ${item.status}">${ICONS[item.status] || ""}</span>
      <span class="checkName">${item.name}</span>
      <span class="badge ${item.status}">${item.status}</span>
      <span class="checkDetail">${escapeHtml(item.detail)}</span>`;
    box.appendChild(row);
  }
  setActivity("自检完成：" + (res.running ? "DSH 正在运行" : "DSH 未运行"));
}
function escapeHtml(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/\n/g, "<br>");
}
$("recheckBtn").addEventListener("click", refreshChecks);

$("fixBtn").addEventListener("click", async () => {
  try {
    const cmds = await invoke("fix_commands");
    if (cmds.length === 0) { setActivity("所有检查项正常，无需修复命令"); return; }
    await navigator.clipboard.writeText(cmds.join("\n"));
    setActivity("已复制 " + cmds.length + " 条修复命令到剪贴板");
  } catch (e) {
    setActivity("复制修复命令失败：" + e);
  }
});

// ---------------- 状态轮询 ----------------
async function pollStatus() {
  let running = false;
  try { running = await invoke("status"); } catch (e) { /* ignore */ }
  setPill(running ? "DSH 正在运行" : "DSH 未运行", running);
  setTimeout(pollStatus, 2000);
}

// ---------------- 启动动作 ----------------
async function startWeb(notify = true) {
  try {
    const r = await invoke("start_web", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "already-running") {
      if (notify) setActivity("DSH 已在运行：" + webUrl);
      return true;
    }
    // 后端会等待端口就绪后才返回，此处即已启动完成
    setActivity("Web 模式已启动" + (cfg.proxyEnabled ? "（已启用代理）" : "") + "，服务已就绪");
    return true;
  } catch (e) { setActivity("启动失败：" + cleanMsg(e)); return false; }
}
$("webBtn").addEventListener("click", () => startWeb(true));
$("browserBtn").addEventListener("click", () => { invoke("open_browser"); setActivity("已调用浏览器打开 " + webUrl); });
$("webOpenBtn").addEventListener("click", async () => {
  if (await startWeb(false)) { setActivity("DSH 已就绪，即将打开浏览器…"); setTimeout(() => invoke("open_browser"), 1500); }
});
$("tuiBtn").addEventListener("click", async () => {
  try { await invoke("start_tui", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr }); setActivity("TUI 已在新的命令行窗口启动"); }
  catch (e) { setActivity("启动失败：" + e); }
});

async function startHeadless(task) {
  if (!task) { setActivity("请先输入问题"); return; }
  try {
    await invoke("start_headless", { task, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    setActivity("Headless 问答已在新窗口运行（完成后窗口保持打开）");
    addHistory(task);
  } catch (e) { setActivity("启动失败：" + e); }
}
$("runBtn").addEventListener("click", () => startHeadless($("taskInput").value.trim()));
$("taskInput").addEventListener("keydown", (e) => { if (e.key === "Enter") startHeadless($("taskInput").value.trim()); });

// ---------------- 历史 ----------------
function renderHistory() {
  const box = $("historyBox");
  box.innerHTML = "";
  const list = cfg.history.slice(0, 4);
  if (list.length === 0) { box.innerHTML = '<span class="sub">（暂无，执行过的任务会显示在这里，点击可重跑）</span>'; return; }
  for (const task of list) {
    const b = document.createElement("button");
    b.className = "chip";
    b.textContent = task.length > 12 ? task.slice(0, 12) + "…" : task;
    b.title = task;
    b.addEventListener("click", () => { $("taskInput").value = task; startHeadless(task); });
    box.appendChild(b);
  }
}
function addHistory(task) {
  cfg.history = [task, ...cfg.history.filter(t => t !== task)].slice(0, 6);
  saveCfg(); renderHistory();
}

// ---------------- 快捷工具 ----------------
$("dirBtn").addEventListener("click", async () => setActivity(await invoke("open_install_dir")));
$("cmdBtn").addEventListener("click", async () => {
  try {
    const cmd = await invoke("get_web_cmd");
    await navigator.clipboard.writeText(cmd);
    setActivity("已复制 Web 启动命令到剪贴板");
  } catch (e) { setActivity("复制失败：" + e); }
});

// ---------------- 重启 DSH ----------------
$("restartBtn").addEventListener("click", async () => {
  const btn = $("restartBtn");
  if (btn.disabled) return;
  const running = await invoke("status");
  if (!running) { await startWeb(false); setActivity("DSH 未在运行，已直接启动…"); return; }
  if (!confirm("将终止当前 DSH（" + webUrl + "）并重新启动。\n当前 Web 界面（包括正在进行的会话）会中断，重启后恢复。\n\n是否继续？")) return;
  btn.disabled = true; btn.textContent = "重启中…";
  setActivity("正在重启 DSH…");
  try {
    const r = await invoke("restart_dsh", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "ok") setActivity("DSH 已重启完成（Web 模式后台运行中）");
  } catch (e) {
    if (String(e).includes("still-running")) {
      setActivity("未能停止现有 DSH 进程（可能权限不足），未重新启动");
    } else { setActivity("重启失败：" + cleanMsg(e)); }
  } finally {
    btn.disabled = false; btn.textContent = "重启 DSH";
  }
});

// ---------------- 代理 ----------------
$("proxySwitch").checked = cfg.proxyEnabled;
$("proxyInput").value = cfg.proxyAddr;
$("proxySwitch").addEventListener("change", async (e) => {
  cfg.proxyEnabled = e.target.checked;
  if (cfg.proxyEnabled) {
    const [enabled, server] = await invoke("get_system_proxy_cmd");
    if (enabled && server) {
      cfg.proxyAddr = server; $("proxyInput").value = server;
      $("proxyHint").textContent = "已同步系统代理 " + server + "（与浏览器一致）";
    } else {
      $("proxyHint").textContent = "系统代理未启用，使用下方手动填写的地址";
    }
    setActivity("已启用代理；启动 DSH 时将注入 HTTP(S)_PROXY 与 NODE_USE_ENV_PROXY");
  } else {
    $("proxyHint").textContent = "支持 http://127.0.0.1:7890 或 http=…;https=…";
    setActivity("已关闭代理；DSH 将直连网络");
  }
  saveCfg();
});
$("proxyInput").addEventListener("change", () => { cfg.proxyAddr = $("proxyInput").value.trim(); saveCfg(); });
$("importBtn").addEventListener("click", async () => {
  const [enabled, server] = await invoke("get_system_proxy_cmd");
  if (!enabled || !server) { setActivity("Windows 系统代理未启用"); return; }
  $("proxyInput").value = server;
  cfg.proxyEnabled = true; cfg.proxyAddr = server;
  $("proxySwitch").checked = true;
  $("proxyHint").textContent = "已导入系统代理";
  saveCfg();
  setActivity("已从系统导入代理：" + server);
});

// ---------------- DSH 更新 / 安装 ----------------
let updateState = { available: false, installed: null, latest: null, busy: false };

function setUpdateBtn(text, accent) {
  const btn = $("updateBtn");
  btn.textContent = text;
  btn.classList.toggle("accent", !!accent);
}
function showBanner(latest, installed) {
  $("bannerText").textContent = "发现 DSH 新版本 v" + latest + "（当前 v" + installed + "），点击右侧「立即更新」手动升级";
  $("bannerBtn").textContent = "立即更新";
  $("banner").hidden = false;
}
function showInstallBanner(latest) {
  $("bannerText").textContent = "未检测到 DSH 程序，点击右侧「立即安装」（npm 全局安装）" + (latest ? "，将安装最新版 v" + latest : "");
  $("bannerBtn").textContent = "立即安装";
  $("banner").hidden = false;
}
function hideBanner() { $("banner").hidden = true; }

async function checkUpdate() {
  if (updateState.busy) return;
  updateState.busy = true;
  setUpdateBtn("检查中…", false);
  setActivity("正在检查 DSH 更新…");
  try {
    const [installed, latest, err] = await invoke("update_check", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    updateState.installed = installed; updateState.latest = latest;
    if (err) {
      // 无法连接 registry 与「已是最新」区分开；错误信息含 npm 真实报错
      updateState.available = false;
      hideBanner();
      if (installed) {
        setUpdateBtn("检查更新", false);
        setActivity("更新检查失败：" + cleanMsg(err));
      } else {
        setUpdateBtn("未安装 DSH", true);
        showInstallBanner(null);
        setActivity("未检测到 DSH；更新检查失败：" + cleanMsg(err));
      }
      return;
    }
    if (!installed) {
      // 未安装：提示一键安装
      updateState.available = false;
      setUpdateBtn("未安装 DSH", true);
      showInstallBanner(latest);
      setActivity("DSH 未安装" + (latest ? "（最新版 v" + latest + "）" : "") + "，可点击「未安装 DSH」一键安装");
    } else if (latest && cmpVer(latest, installed) > 0) {
      updateState.available = true;
      setUpdateBtn("更新到 v" + latest, true);
      showBanner(latest, installed);
      setActivity("发现 DSH 新版本 v" + latest + "（当前 v" + installed + "）");
    } else {
      updateState.available = false;
      setUpdateBtn("已是最新 v" + installed, false);
      hideBanner();
      setActivity("DSH 已是最新版本 v" + installed);
    }
  } catch (e) {
    setUpdateBtn("检查更新", false); hideBanner();
    setActivity("检查更新失败：" + cleanMsg(e));
  } finally {
    updateState.busy = false;
  }
}
function cmpVer(a, b) {
  const pa = a.split("-")[0].split(".").map(Number);
  const pb = b.split("-")[0].split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) > (pb[i] || 0) ? 1 : -1;
  }
  const ra = a.includes("-"), rb = b.includes("-");
  if (ra !== rb) return rb ? 1 : -1;
  return a === b ? 0 : (a > b ? 1 : -1);
}

$("updateBtn").addEventListener("click", () => {
  if (updateState.busy) return;
  if (!updateState.installed) { confirmInstall(); }
  else if (updateState.available) { confirmUpdate(); }
  else { checkUpdate(); }
});
$("bannerBtn").addEventListener("click", () => {
  if (updateState.busy) return;
  if (updateState.installed) { confirmUpdate(); } else { confirmInstall(); }
});

// 一键安装（npm install -g @deepseek-ai/dsh）
async function confirmInstall() {
  if (updateState.busy) return;
  if (!confirm("未检测到 DSH 程序。\n\n将执行 npm install -g @deepseek-ai/dsh（全局安装），需联网下载，请稍候。\n\n是否继续？")) return;
  updateState.busy = true;
  setUpdateBtn("安装中…", false);
  setActivity("正在安装 DSH（npm install -g），请稍候…");
  const progress = new window.__TAURI__.core.Channel();
  progress.onmessage = (line) => {
    const s = String(line);
    setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
  };
  try {
    const newV = await invoke("install_dsh", {
      proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress
    });
    updateState.installed = newV;
    updateState.available = false;
    setUpdateBtn("已是最新 v" + newV, false);
    hideBanner();
    setActivity("DSH 安装完成：v" + newV);
    refreshChecks(); // 重新自检
  } catch (e) {
    setUpdateBtn("未安装 DSH", true);
    if (updateState.latest) showInstallBanner(updateState.latest); else { $("bannerBtn").textContent = "立即安装"; $("banner").hidden = false; $("bannerText").textContent = "DSH 安装失败，请重试"; }
    setActivity("DSH 安装失败：" + cleanMsg(e));
  } finally {
    updateState.busy = false;
  }
}

async function confirmUpdate() {
  if (updateState.busy || !updateState.available) return;
  if (!confirm("发现新版本 v" + updateState.latest + "（当前 v" + updateState.installed + "）。\n\n更新将修改 DSH 安装目录（自动识别的位置）。\n\n若 DSH 正在运行，建议先停止；更新完成后需重启 DSH 生效。\n\n是否现在更新？")) return;
  updateState.busy = true;
  setUpdateBtn("更新中…", false);
  setActivity("正在更新 DSH（npm install），请稍候…");
  const progress = new window.__TAURI__.core.Channel();
  progress.onmessage = (line) => {
    const s = String(line);
    setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
  };
  try {
    const [oldV, newV] = await invoke("update_dsh", {
      proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress
    });
    updateState.available = false;
    updateState.installed = newV;
    setUpdateBtn("已是最新 v" + newV, false);
    hideBanner();
    setActivity("DSH 更新完成：v" + oldV + " → v" + newV + "（重启 DSH 生效）");
    refreshChecks();
  } catch (e) {
    if (updateState.available && updateState.latest) { setUpdateBtn("更新到 v" + updateState.latest, true); }
    else { setUpdateBtn("检查更新", false); }
    setActivity("DSH 更新失败：" + cleanMsg(e));
  } finally {
    updateState.busy = false;
  }
}

// ---------------- 初始化 ----------------
applyTheme();
renderHistory();
refreshChecks();
setTimeout(pollStatus, 200);   // 首次状态轮询
setTimeout(checkUpdate, 3500); // 启动后静默检查更新
// Web 地址与版本号从后端读取（版本与 Cargo.toml 保持一致）
invoke("get_web_url").then(u => { if (u) webUrl = u; }).catch(() => {});
window.__TAURI__.app.getVersion().then(v => {
  $("version").textContent = "v" + v;
  $("title").dataset.ver = v; // 标题右侧的版本徽标
}).catch(() => {});
