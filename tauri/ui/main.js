// DSH 启动器前端（Tauri 2）
// 通过 window.__TAURI__.core.invoke 调用 Rust 后端，localStorage 持久化配置。
const { invoke } = window.__TAURI__.core;
const CFG_KEY = "dshLauncher.v1";

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

const CHECK_STATES = { OK: "✓", FAIL: "✕", WARN: "⚠", INFO: "ℹ" };

// ---------------- 自检 ----------------
async function refreshChecks() {
  setActivity("正在自检…");
  let res;
  try { res = await invoke("checks"); } catch (e) { setActivity("自检失败：" + e); return; }
  const box = $("checks");
  box.innerHTML = "";
  const icons = { OK: "#16A34A", FAIL: "#DC2626", WARN: "#D97706", INFO: "#6B7280" };
  if (document.documentElement.dataset.theme === "dark") {
    icons.OK = "#34D399"; icons.FAIL = "#F87171"; icons.WARN = "#FBBF24"; icons.INFO = "#9AA3B2";
  }
  for (const item of res.items) {
    const row = document.createElement("div");
    row.className = "checkRow";
    row.innerHTML = `<span class="checkIcon" style="color:${icons[item.status]}">${CHECK_STATES[item.status]}</span>
      <span class="checkName">${item.name}</span>
      <span class="badge ${item.status}"> ${item.status} </span>
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
  setPill(running ? "●  DSH 正在运行" : "●  DSH 未运行", running);
  setTimeout(pollStatus, 2000);
}

// ---------------- 启动动作 ----------------
async function startWeb(notify = true) {
  try {
    const r = await invoke("start_web", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "already-running") {
      if (notify) setActivity("DSH 已在运行：" + "http://127.0.0.1:3080");
      return true;
    }
    setActivity("Web 模式已启动" + (cfg.proxyEnabled ? "（已启用代理）" : "") + "，等待端口就绪…");
    return true;
  } catch (e) { setActivity("启动失败：" + e); return false; }
}
$("webBtn").addEventListener("click", () => startWeb(true));
$("browserBtn").addEventListener("click", () => { invoke("open_browser"); setActivity("已调用浏览器打开 http://127.0.0.1:3080"); });
$("webOpenBtn").addEventListener("click", async () => {
  if (await startWeb(false)) { setActivity("DSH 启动中，4 秒后自动打开浏览器…"); setTimeout(() => invoke("open_browser"), 4000); }
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
  if (!confirm("将终止当前 DSH（http://127.0.0.1:3080）并重新启动。\n当前 Web 界面（包括正在进行的会话）会中断，重启后恢复。\n\n是否继续？")) return;
  btn.disabled = true; btn.textContent = "重启中…";
  setActivity("正在重启 DSH…");
  try {
    const r = await invoke("restart_dsh", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "ok") setActivity("DSH 已重启完成（Web 模式后台运行中）");
  } catch (e) {
    if (String(e).includes("still-running")) {
      setActivity("未能停止现有 DSH 进程（可能权限不足），未重新启动");
    } else { setActivity("重启失败：" + e); }
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

// ---------------- DSH 更新检测 ----------------
let updateState = { available: false, installed: null, latest: null, busy: false };

function setUpdateBtn(text, accent) {
  const btn = $("updateBtn");
  btn.textContent = text;
  btn.classList.toggle("accent", !!accent);
}
function showBanner(latest, installed) {
  $("bannerText").textContent = "发现 DSH 新版本 v" + latest + "（当前 v" + installed + "），点击右侧「立即更新」手动升级";
  $("banner").hidden = false;
}
function hideBanner() { $("banner").hidden = true; }

async function checkUpdate() {
  if (updateState.busy) return;
  updateState.busy = true;
  setUpdateBtn("检查中…", false);
  setActivity("正在检查 DSH 更新…");
  try {
    const [installed, latest] = await invoke("update_check");
    updateState.installed = installed; updateState.latest = latest;
    if (latest && installed && cmpVer(latest, installed) > 0) {
      updateState.available = true;
      setUpdateBtn("更新到 v" + latest, true);
      showBanner(latest, installed);
      setActivity("发现 DSH 新版本 v" + latest + "（当前 v" + installed + "）");
    } else {
      updateState.available = false;
      setUpdateBtn("已是最新 v" + (installed || latest), false);
      hideBanner();
      setActivity("DSH 已是最新版本 v" + (installed || latest));
    }
  } catch (e) {
    setUpdateBtn("检查更新", false); hideBanner();
    setActivity("检查更新失败：" + e);
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
  if (updateState.available) { confirmUpdate(); } else { checkUpdate(); }
});
$("bannerBtn").addEventListener("click", confirmUpdate);

async function confirmUpdate() {
  if (updateState.busy || !updateState.available) return;
  if (!confirm("发现新版本 v" + updateState.latest + "（当前 v" + updateState.installed + "）。\n\n更新将修改 DSH 安装目录（settings.json 中 dDrivePath 指定的位置）。\n\n若 DSH 正在运行，建议先停止；更新完成后需重启 DSH 生效。\n\n是否现在更新？")) return;
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
    setUpdateBtn("已是最新 v" + newV, false);
    hideBanner();
    setActivity("DSH 更新完成：v" + oldV + " → v" + newV + "（重启 DSH 生效）");
  } catch (e) {
    if (updateState.available && updateState.latest) { setUpdateBtn("更新到 v" + updateState.latest, true); }
    else { setUpdateBtn("检查更新", false); }
    setActivity("DSH 更新失败：" + e);
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
