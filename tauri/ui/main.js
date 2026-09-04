// DSH 启动器前端业务逻辑（Tauri 2）
// 通过 window.__TAURI__.core.invoke 调用 Rust 后端。
// 文件组织：core.js = 工具/主题/配置；chrome.js = 窗口外观与日志面板；
// 本文件 = 业务动作（自检/启动/更新/插件/备份/设置）。
// 配置存放约定：UI 偏好（主题/代理等）存 localStorage，
// 影响子进程行为的配置（closeAction/pluginProfile/registry）存 exe 旁 settings.json。
const DSH_REPOSITORY_URL = "https://github.com/deepseek-ai/deepseek-harness";

// 全局 cfg（core.js 的 loadCfg/saveCfg 读写此对象）
window.cfg = loadCfg();

// Web 服务地址：启动时从后端获取（按 settings.json webPort 计算），前端不硬编码
let webUrl = "http://127.0.0.1:3080";

applyTheme();
applyAccent();
initTheme();
initAccent();

// ---------------- 自检状态图标（内联 SVG，随 currentColor 着色） ----------------
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
      <span class="checkName">${escapeHtml(item.name)}</span>
      <span class="badge ${item.status}">${item.status}</span>
      <span class="checkDetail">${escapeHtml(item.detail)}</span>`;
    box.appendChild(row);
  }
  // stagger 入场门控（与插件列表同一模式）：逐行淡入，动画结束后移除；空结果时跳过
  if (box.children.length > 0) {
    box.classList.remove("enter");
    void box.offsetWidth; // 强制 reflow，确保动画可重放
    box.classList.add("enter");
    // once: 避免连点重渲染时监听器累积，提前摘除 .enter 导致后续行瞬显
    box.addEventListener("animationend", function onEnd() {
      box.classList.remove("enter");
      box.removeEventListener("animationend", onEnd);
    }, { once: true });
  }
  setActivity("自检完成：" + (res.running ? "DSH 正在运行" : "DSH 未运行"));
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

// ---------------- 状态轮询（三态：running / foreign-port / stopped） ----------------
// 偶发 IPC 失败保持上次状态（避免状态胶囊误闪），连续 3 次失败才显示未知；
// 窗口隐藏（托盘）时胶囊不可见，降为 10 秒低频轮询，恢复显示时回到 2 秒。
// foreign-port = webPort 被无关程序占用（后端已做命令行身份核验），
// 单独提示而非误报「正在运行」。
let pollFailures = 0;
function setPill(state) {
  const pill = $("statusPill");
  pill.dataset.running = state;
  pill.textContent = {
    running: "DSH 运行中",
    "foreign-port": "端口被其他程序占用",
    stopped: "DSH 未运行",
    unknown: "状态检测异常",
  }[state] || "检测中…";
}
async function pollStatus() {
  try {
    const d = await invoke("status_detail");
    pollFailures = 0;
    if (d.state === "running") setPill("running");
    else if (d.state === "foreign-port") setPill("foreign-port");
    else setPill("stopped");
  } catch (e) {
    pollFailures += 1;
    if (pollFailures >= 3) setPill("unknown");
  }
  setTimeout(pollStatus, document.hidden ? 10000 : 2000);
}

// ---------------- 启动动作 ----------------
// 返回 "running"（已在运行）/ "starting"（进程已启动、端口就绪中）/ "ready" / "failed"
async function startWeb(notify = true) {
  try {
    const r = await invoke("start_web", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "already-running") {
      if (notify) setActivity("DSH 已在运行：" + webUrl);
      return "running";
    }
    if (r === "starting") {
      // 后端 8 秒等待窗口内端口未就绪但进程存活：不是失败，交给状态轮询确认
      setActivity("DSH 进程已启动，端口就绪中…（就绪后状态胶囊会显示运行状态）");
      return "starting";
    }
    // 后端会等待端口就绪后才返回，此处即已启动完成
    setActivity("Web 模式已启动" + (cfg.proxyEnabled ? "（已启用代理）" : "") + "，服务已就绪");
    return "ready";
  } catch (e) { showError("启动失败", e); return "failed"; }
}
// 「启动 Web 界面」：启动（已在运行则跳过）后按设置决定是否自动打开浏览器。
// busy 防抖：启动要等端口就绪（最长 8 秒），双击会并发拉起两个 DSH 进程
let webStartBusy = false;
$("webBtn").addEventListener("click", async () => {
  if (webStartBusy) return;
  webStartBusy = true;
  const btn = $("webBtn");
  btn.disabled = true;
  try {
    const r = await startWeb(false);
    if (r === "ready" && cfg.autoOpenBrowser !== false) {
      setActivity("DSH 已就绪，即将打开浏览器…");
      setTimeout(() => invoke("open_browser"), 1500);
    } else if (r === "ready") {
      setActivity("DSH 已就绪：" + webUrl);
    }
    // starting / running / failed 的提示已由 startWeb 设置；
    // starting 时不自动开浏览器（端口未就绪只会打开错误页）
  } finally {
    webStartBusy = false;
    btn.disabled = false;
  }
});
$("browserBtn").addEventListener("click", () => { invoke("open_browser"); setActivity("已调用浏览器打开 " + webUrl); });
// 「关闭 Web 界面」：终止 DSH Web 进程，不重启
$("webCloseBtn").addEventListener("click", async () => {
  const btn = $("webCloseBtn");
  if (btn.disabled) return;
  btn.disabled = true; btn.textContent = "关闭中…";
  try {
    const r = await invoke("stop_web");
    if (r === "not-running") { setActivity("DSH Web 界面未在运行"); return; }
    setActivity("Web 界面已关闭");
  } catch (e) {
    if (String(e).includes("still-running")) {
      setActivity("未能停止 DSH 进程（可能权限不足）");
    } else { setActivity("关闭失败：" + cleanMsg(e)); }
  } finally {
    btn.disabled = false; btn.textContent = "关闭 Web 界面";
  }
});
// TUI 首次点击会自动安装插件（可能耗时数十秒），同样需要 busy 防抖
let tuiStartBusy = false;
$("tuiBtn").addEventListener("click", async () => {
  if (tuiStartBusy) return;
  tuiStartBusy = true;
  const btn = $("tuiBtn");
  btn.disabled = true;
  try {
    // 首次使用会自动安装 TUI（@deepseek-harness-tui/dsh-tui），
    // 安装进度推送到活动栏 + 日志面板（有进度输出时才打开面板）
    const progress = new window.__TAURI__.core.Channel();
    let opened = false;
    progress.onmessage = (line) => {
      const s = String(line).trim();
      if (!s) return;
      if (!opened) { logPanel.title("启动 TUI"); logPanel.clear(); logPanel.open(); opened = true; }
      setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
      logPanel.append(s);
    };
    const r = await invoke("start_tui", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress });
    setActivity(r);
  } catch (e) { showError("启动 TUI 失败", e); }
  finally {
    tuiStartBusy = false;
    btn.disabled = false;
  }
});

// ---------------- 快捷工具 ----------------
$("dirBtn").addEventListener("click", async () => {
  try { setActivity(await invoke("open_install_dir")); }
  catch (e) { setActivity("打开安装目录失败：" + cleanMsg(e)); }
});
$("cmdBtn").addEventListener("click", async () => {
  try {
    const cmd = await invoke("get_web_cmd");
    await navigator.clipboard.writeText(cmd);
    setActivity("已复制 Web 启动命令到剪贴板");
  } catch (e) { setActivity("复制失败：" + cleanMsg(e)); }
});

// ---------------- 重启 DSH ----------------
$("restartBtn").addEventListener("click", async () => {
  const btn = $("restartBtn");
  if (btn.disabled) return;
  let running = false;
  try { running = await invoke("status"); }
  catch (e) { setActivity("状态检测失败：" + cleanMsg(e)); return; }
  if (!running) { await startWeb(false); setActivity("DSH 未在运行，已直接启动…"); return; }  if (!(await uiConfirm({ title: "重启 DSH", message: "将终止当前 DSH（" + webUrl + "）并重新启动。\n当前 Web 界面（包括正在进行的会话）会中断，重启后恢复。", okText: "重启" }))) return;
  btn.disabled = true; btn.textContent = "重启中…";
  setActivity("正在重启 DSH…");
  try {
    const r = await invoke("restart_dsh", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "ok") setActivity("DSH 已重启完成（Web 模式后台运行中）");
    else if (r === "starting") setActivity("DSH 重启中：进程已启动，等待端口就绪…");
  } catch (e) {
    if (String(e).includes("still-running")) {
      setActivity("未能停止现有 DSH 进程（可能权限不足），未重新启动");
    } else { showError("重启失败", e); }
  } finally {
    btn.disabled = false; btn.textContent = "重启 DSH";
  }
});

// ---------------- 代理（设置页） ----------------
$("setProxySwitch").checked = cfg.proxyEnabled;
$("setProxyInput").value = cfg.proxyAddr;
$("setProxySwitch").addEventListener("change", async (e) => {
  cfg.proxyEnabled = e.target.checked;
  if (cfg.proxyEnabled) {
    try {
      const [enabled, server] = await invoke("get_system_proxy_cmd");
      if (enabled && server) {
        cfg.proxyAddr = server; $("setProxyInput").value = server;
        // 合并为一条提示：连续两次 setActivity 会互相覆盖，第一条永远看不到
        setActivity("已启用代理并同步系统代理 " + server + "（与浏览器一致）");
      } else {
        setActivity("已启用代理；系统代理未启用，使用手动填写的地址。启动 DSH 时将注入 HTTP(S)_PROXY 与 NODE_USE_ENV_PROXY");
      }
    } catch (e) { setActivity("读取系统代理失败：" + cleanMsg(e)); }
  } else {
    setActivity("已关闭代理；DSH 将直连网络");
  }
  saveCfg();
});
$("setProxyInput").addEventListener("change", () => { cfg.proxyAddr = $("setProxyInput").value.trim(); saveCfg(); });
$("setProxyImport").addEventListener("click", async () => {
  try {
    const [enabled, server] = await invoke("get_system_proxy_cmd");
    if (!enabled || !server) { setActivity("Windows 系统代理未启用"); return; }
    $("setProxyInput").value = server;
    cfg.proxyEnabled = true; cfg.proxyAddr = server;
    $("setProxySwitch").checked = true;
    saveCfg();
    setActivity("已从系统导入代理：" + server);
  } catch (e) { setActivity("读取系统代理失败：" + cleanMsg(e)); }
});

// ---------------- DSH 更新 / 安装 ----------------
let updateState = { available: false, installed: null, latest: null, busy: false };

function setUpdateBtn(text, accent) {
  const btn = $("updateBtn");
  btn.textContent = text;
  btn.classList.toggle("accent", !!accent);
}
// 横幅可见性 = bannerVisible（更新检查逻辑维护） && 当前在首页（视图切换维护），
// 二者解耦：离开首页不丢状态，返回首页按状态恢复；非首页时检查更新也不会误显示
let bannerVisible = false;
function applyBannerVisibility() {
  $("banner").hidden = !(bannerVisible && currentView === "home");
}
function showBanner(latest, installed) {
  $("bannerText").textContent = "发现 DSH 新版本 v" + latest + "（当前 v" + installed + "），点击右侧「立即更新」手动升级";
  $("bannerBtn").textContent = "立即更新";
  bannerVisible = true;
  applyBannerVisibility();
}
function showInstallBanner(latest) {
  $("bannerText").textContent = "未检测到 DSH 程序，点击右侧「立即安装」（npm 全局安装）" + (latest ? "，将安装最新版 v" + latest : "");
  $("bannerBtn").textContent = "立即安装";
  bannerVisible = true;
  applyBannerVisibility();
}
function hideBanner() { bannerVisible = false; applyBannerVisibility(); }

async function checkUpdate() {
  if (updateState.busy) return;
  updateState.busy = true;
  setUpdateBtn("检查中…", false);
  setActivity("正在检查 DSH 更新…");
  try {
    const res = await invoke("update_check", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    updateState.installed = res.installed; updateState.latest = res.latest;
    if (res.error) {
      // 无法连接 registry 与「已是最新」区分开；错误信息含 npm 真实报错
      updateState.available = false;
      hideBanner();
      if (res.installed) {
        setUpdateBtn("检查更新", false);
        setActivity("更新检查失败：" + cleanMsg(res.error));
      } else {
        setUpdateBtn("未安装 DSH", true);
        showInstallBanner(null);
        setActivity("未检测到 DSH；更新检查失败：" + cleanMsg(res.error));
      }
      return;
    }
    const latest = res.latest, installed = res.installed;
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

$("updateBtn").addEventListener("click", () => {
  if (updateState.busy) return;
  if (!updateState.installed) { confirmInstall(); }
  else if (updateState.available) { confirmUpdate(); }
  else { checkUpdate(); }
});
$("repoBtn").addEventListener("click", async () => {
  try {
    await invoke("open_url", { url: DSH_REPOSITORY_URL });
    setActivity("已在浏览器打开 DeepSeek Harness GitHub 仓库");
  } catch (e) {
    setActivity("打开 GitHub 仓库失败：" + cleanMsg(e));
  }
});
$("bannerBtn").addEventListener("click", () => {
  if (updateState.busy) return;
  if (updateState.installed) { confirmUpdate(); } else { confirmInstall(); }
});

// 一键安装（npm install -g @deepseek-ai/dsh）
async function confirmInstall() {
  if (updateState.busy) return;
  if (!(await uiConfirm({ title: "安装 DSH", message: "未检测到 DSH 程序。\n将执行 npm install -g @deepseek-ai/dsh（全局安装），需联网下载，请稍候。", okText: "安装" }))) return;
  updateState.busy = true;
  setUpdateBtn("安装中…", false);
  setActivity("正在安装 DSH（npm install -g），请稍候…");
  logPanel.title("DSH 安装");
  logPanel.clear();
  logPanel.open();
  const progress = new window.__TAURI__.core.Channel();
  progress.onmessage = (line) => {
    const s = String(line);
    setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
    logPanel.append(s);
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
    if (updateState.latest) showInstallBanner(updateState.latest); else { $("bannerBtn").textContent = "立即安装"; $("bannerText").textContent = "DSH 安装失败，请重试"; bannerVisible = true; applyBannerVisibility(); }
    showError("DSH 安装失败", e);
  } finally {
    updateState.busy = false;
  }
}

async function confirmUpdate() {
  if (updateState.busy || !updateState.available) return;
  if (!(await uiConfirm({ title: "更新 DSH", message: "发现新版本 v" + updateState.latest + "（当前 v" + updateState.installed + "）。\n更新将修改 DSH 安装目录（自动识别的位置）。\n若 DSH 正在运行，建议先停止；更新完成后需重启 DSH 生效。", okText: "更新" }))) return;
  updateState.busy = true;
  setUpdateBtn("更新中…", false);
  setActivity("正在更新 DSH（npm install），请稍候…");
  logPanel.title("DSH 更新");
  logPanel.clear();
  logPanel.open();
  const progress = new window.__TAURI__.core.Channel();
  progress.onmessage = (line) => {
    const s = String(line);
    setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
    logPanel.append(s);
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
    showError("DSH 更新失败", e);
  } finally {
    updateState.busy = false;
  }
}

// ---------------- 视图切换 ----------------
// 首页 / 插件管理 / 设置 三个视图在同一窗口内切换（集成度高，避免多窗口混乱）。
// header 按钮与状态元素通过 data-view 属性按视图显隐。
let currentView = "home";
function setView(view) {
  currentView = view;
  $("viewHome").hidden = view !== "home";
  $("viewPlugins").hidden = view !== "plugins";
  $("viewSettings").hidden = view !== "settings";
  document.querySelectorAll("[data-view]").forEach((el) => {
    el.hidden = el.dataset.view !== view;
  });
  // 更新横幅显隐 = bannerVisible && 首页，状态不因切换视图丢失
  applyBannerVisibility();
  if (view === "plugins") {
    $("title").textContent = "DSH 插件管理";
    $("subtitle").textContent = "Plugins";
    $("pluginMgrBtn").textContent = "返回";
    $("settingsBtn").hidden = true;
    loadPlugins(); // 每次进入插件页刷新列表
  } else if (view === "settings") {
    $("title").textContent = "设置";
    $("subtitle").textContent = "Settings";
    $("settingsBtn").textContent = "返回";
    $("pluginMgrBtn").hidden = true;
    loadSettings(); // 每次进入设置页刷新配置
  } else {
    $("title").textContent = "DeepSeek Harness 启动器";
    $("subtitle").textContent = "DSH · Desktop Launcher";
    $("pluginMgrBtn").textContent = "插件管理";
    $("pluginMgrBtn").hidden = false;
    $("settingsBtn").textContent = "设置";
    $("settingsBtn").hidden = false;
  }
}
$("pluginMgrBtn").addEventListener("click", () => setView(currentView === "home" ? "plugins" : "home"));
$("settingsBtn").addEventListener("click", () => setView(currentView === "home" ? "settings" : "home"));

// ---------------- 设置页 ----------------
// settings.json 相关配置（closeAction / pluginProfile / registry）加载与保存
let settingsState = { closeAction: "tray", pluginProfile: "web", registry: "" };
let activePluginProfile = "web";
// 请求竞态防护（与 loadPlugins 同款）：快速切换视图时旧响应不得覆盖新状态
let settingsRequest = 0;
async function loadSettings() {
  const request = ++settingsRequest;
  try {
    const s = await invoke("get_settings");
    if (request !== settingsRequest) return;
    settingsState = s;
    activePluginProfile = s.pluginProfile || "web";
    // 窗口行为单选
    $("setCloseSeg").querySelectorAll(".segBtn").forEach((b) => {
      b.classList.toggle("on", b.dataset.val === s.closeAction);
    });
    // profile 下拉（保留当前选中，若列表中不存在则补一项）
    const profiles = await invoke("list_profiles");
    if (request !== settingsRequest) return;
    const sel = $("setProfileSelect");
    const current = sel.value || s.pluginProfile;
    sel.innerHTML = "";
    const all = profiles.includes(s.pluginProfile) ? profiles : [s.pluginProfile, ...profiles];
    for (const name of all) {
      const opt = document.createElement("option");
      opt.value = name;
      opt.textContent = name;
      sel.appendChild(opt);
    }
    sel.value = all.includes(current) ? current : s.pluginProfile;
    // registry
    $("setRegistryInput").value = s.registry;
  } catch (e) {
    if (request !== settingsRequest) return;
    setActivity("设置加载失败：" + cleanMsg(e));
  }
  // 开机自启
  try { $("setAutoStart").checked = await invoke("get_autostart"); } catch (e) { /* ignore */ }
  if (request !== settingsRequest) return;
  // localStorage 配置
  $("setAutoCheck").checked = cfg.autoCheckUpdate !== false;
  $("setAutoBrowser").checked = cfg.autoOpenBrowser !== false;
  loadBackups(); // 每次进入设置页刷新备份列表
}
// 行为开关（localStorage）
$("setAutoCheck").addEventListener("change", (e) => {
  cfg.autoCheckUpdate = e.target.checked;
  saveCfg();
  setActivity(e.target.checked ? "已开启启动时自动检查更新" : "已关闭启动时自动检查更新");
});
$("setAutoBrowser").addEventListener("change", (e) => {
  cfg.autoOpenBrowser = e.target.checked;
  saveCfg();
  setActivity(e.target.checked ? "已开启自动打开浏览器" : "已关闭自动打开浏览器");
});
// 开机自启（注册表）
$("setAutoStart").addEventListener("change", async (e) => {
  const btn = $("setAutoStart");
  btn.disabled = true;
  try {
    await invoke("set_autostart", { enabled: e.target.checked });
    setActivity(e.target.checked ? "已开启开机自启" : "已关闭开机自启");
  } catch (err) {
    btn.checked = !e.target.checked;
    setActivity("设置开机自启失败：" + cleanMsg(err));
  } finally {
    btn.disabled = false;
  }
});
// 窗口行为单选
$("setCloseSeg").addEventListener("click", async (e) => {
  const btn = e.target.closest(".segBtn");
  if (!btn || btn.classList.contains("on")) return;
  try {
    const s = await invoke("save_settings", { patch: { closeAction: btn.dataset.val } });
    settingsState = s;
    closeAction = s.closeAction; // 同步 onCloseRequested 的行为，否则当次会话不生效
    $("setCloseSeg").querySelectorAll(".segBtn").forEach((b) => b.classList.toggle("on", b.dataset.val === s.closeAction));
    setActivity(btn.dataset.val === "tray" ? "关闭按钮将隐藏到托盘" : "关闭按钮将直接退出程序");
  } catch (err) {
    setActivity("保存窗口行为失败：" + cleanMsg(err));
  }
});
// profile 下拉
$("setProfileSelect").addEventListener("change", async (e) => {
  try {
    const s = await invoke("save_settings", { patch: { pluginProfile: e.target.value } });
    settingsState = s;
    activePluginProfile = s.pluginProfile;
    resetPluginUpdates(); // 插件视图的更新缓存作废
    setActivity("插件管理 profile 已切换为 " + s.pluginProfile + "，进入插件视图生效");
  } catch (err) {
    setActivity("保存 profile 失败：" + cleanMsg(err));
    loadSettings();
  }
});
// registry 镜像
$("setRegistryInput").addEventListener("change", async (e) => {
  try {
    const s = await invoke("save_settings", { patch: { registry: e.target.value.trim() } });
    settingsState = s;
    setActivity(s.registry ? "registry 镜像已设为 " + s.registry : "已恢复官方 npm registry");
  } catch (err) {
    setActivity("保存 registry 失败：" + cleanMsg(err));
    $("setRegistryInput").value = settingsState.registry;
  }
});
// ---------------- 数据备份 / 恢复 ----------------
// 互斥保护：备份与恢复不能同时进行（恢复含 pnpm 重建插件，可能耗时较长）
let backupBusy = false;
function setBackupBusy(b) {
  backupBusy = b;
  $("backupList").classList.toggle("busy", b); // busy 类由 style.css 提供置灰/禁用观感
  $("backupBtn").disabled = b;
  $("backupDirBtn").disabled = b;
}

// 备份列表：从后端读取（按时间倒序），渲染 文件名 + 恢复前徽标 + 时间/大小 + 恢复按钮
let backupListRequest = 0;
async function loadBackups() {
  const request = ++backupListRequest;
  const box = $("backupList");
  try {
    const list = await invoke("list_backups");
    if (request !== backupListRequest) return;
    if (!list.length) {
      box.innerHTML = '<div class="backupEmpty">暂无备份 —— 点击「备份 DSH 数据」创建第一份</div>';
      return;
    }
    box.innerHTML = "";
    for (const b of list) {
      const row = document.createElement("div");
      row.className = "backupRow";
      const badge = b.kind === "pre-restore" ? '<span class="backupBadge pre">恢复前</span>' : "";
      row.innerHTML = `
        <span class="backupName" title="${escapeAttr(b.fileName)}">${escapeHtml(b.fileName)}</span>
        ${badge}
        <span class="backupMeta">${escapeHtml(b.modified)} · ${escapeHtml(formatBytes(b.size))}</span>
        <button class="btn small danger" data-restore="${escapeAttr(b.fileName)}">恢复</button>`;
      box.appendChild(row);
    }
  } catch (e) {
    if (request !== backupListRequest) return;
    box.innerHTML = '<div class="backupEmpty">备份列表加载失败</div>';
  }
}

// 「备份 DSH 数据」：确认后打包配置/对话历史/插件清单到 exe 旁 backups/（后端自动保留 5 份）
$("backupBtn").addEventListener("click", async () => {
  if (backupBusy) return;
  if (!(await uiConfirm({ title: "备份 DSH 数据", message: "将打包 DSH 配置、对话历史与插件清单到备份目录。\n\n备份文件包含 API 密钥等敏感凭据，请妥善保管。\n（DSH 运行中不可备份，请先停止）", okText: "备份" }))) return;
  setBackupBusy(true);
  try {
    const r = await invoke("backup_dsh");
    setActivity("备份完成：" + r.fileName);
    await loadBackups();
  } catch (e) { showError("备份失败", e); }
  finally { setBackupBusy(false); }
});

// 「打开备份目录」：在资源管理器中定位 backups/
$("backupDirBtn").addEventListener("click", async () => {
  try {
    setActivity(await invoke("open_backups_dir"));
  } catch (e) { setActivity("打开备份目录失败：" + cleanMsg(e)); }
});

// 备份列表事件委托：点击行内「恢复」按钮进入恢复流程
$("backupList").addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-restore]");
  if (!btn || backupBusy) return;
  const fileName = btn.dataset.restore;
  // 1. DSH 运行中禁止恢复（文件被占用，且会覆盖运行中的配置）
  let running = false;
  try { running = await invoke("status"); } catch (err) { /* ignore */ }
  if (running) {
    showError("无法恢复", "DSH 正在运行，请先到首页点击「关闭 Web 界面」停止后再恢复");
    return;
  }
  // 2. 危险操作二次确认（恢复前后端会自动备份当前状态，可回退）
  if (!(await uiConfirm({
    title: "恢复 DSH 数据",
    message: "将用备份覆盖当前 DSH 配置与对话历史：\n" + fileName + "\n\n恢复前会自动备份当前状态（可回退），恢复后自动重建插件。\n\n确定恢复？",
    okText: "恢复",
    danger: true,
  }))) return;
  // 3. busy 保护 + 进度 Channel（复用插件安装模式：日志面板完整输出 + 活动栏截断显示）
  setBackupBusy(true);
  try {
    const progress = new window.__TAURI__.core.Channel();
    progress.onmessage = (line) => {
      const s = String(line).trim();
      if (!s) return;
      setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
      logPanel.append(s);
    };
    logPanel.title("恢复 DSH 数据");
    logPanel.clear();
    logPanel.open();
    const result = await invoke("restore_dsh", {
      fileName, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress
    });
    setActivity("恢复完成：" + result);
    await loadBackups();
  } catch (e) { showError("恢复失败", e); }
  finally { setBackupBusy(false); }
});

// ---------------- 插件管理 ----------------
let pluginBusy = false;
let pluginListRequest = 0;
// 更新检查结果缓存：{ [name]: { installed, latest, error } }；null = 尚未检查
let updates = null;
function setPluginBusy(b) {
  pluginBusy = b;
  $("pluginList").classList.toggle("busy", b);
  $("pluginInstallBtn").disabled = b;
  $("pluginCheckBtn").disabled = b;
  $("pluginUpdateAllBtn").disabled = b;
}
function resetPluginUpdates() {
  updates = null;
  const allBtn = $("pluginUpdateAllBtn");
  allBtn.hidden = true;
  allBtn.textContent = "全部更新";
}
// npm / pnpm 输出逐行推送到活动栏 + 全局日志面板（完整输出）
function pluginProgress(label) {
  logPanel.title(label);
  logPanel.clear();
  logPanel.open();
  const progress = new window.__TAURI__.core.Channel();
  progress.onmessage = (line) => {
    const s = String(line).trim();
    if (!s) return;
    setActivity(label + "：" + (s.length > 90 ? s.slice(0, 87) + "…" : s));
    logPanel.append(s);
  };
  return progress;
}

function renderPlugins(res) {
  const box = $("pluginList");
  box.innerHTML = "";
  // 初始 HTML 带 busy 类（"加载中"），渲染完成必须移除，否则整列表灰置不可点
  box.classList.toggle("busy", pluginBusy);
  $("profilePath").textContent = res.profileDir;
  if (!res.initialized) {
    box.innerHTML = '<div class="pluginEmpty"><div class="emptyIcon">🧩</div><div class="emptyTitle">' + escapeHtml(activePluginProfile) + ' profile 尚未初始化</div><div class="sub">安装第一个插件时会自动创建（' + escapeHtml(res.profileDir) + '）</div></div>';
    return;
  }
  // 内置插件（随 DSH 安装）不展示，仅管理用户插件
  const userPlugins = res.plugins.filter((p) => !p.isBuiltin);
  if (userPlugins.length === 0) {
    box.innerHTML = '<div class="pluginEmpty"><div class="emptyIcon">🧩</div><div class="emptyTitle">暂无用户插件</div><div class="sub">在上方输入 npm 包名安装一个吧</div></div>';
    return;
  }
  for (const p of userPlugins) box.appendChild(pluginRow(p));
  // stagger 入场门控：加 .enter 触发行逐条淡入，动画结束后移除，
  // 避免每次刷新/重渲染都整体重放
  box.classList.remove("enter");
  void box.offsetWidth; // 强制 reflow，确保动画可重放
  box.classList.add("enter");
  // once: 避免连点重渲染时监听器累积，提前摘除 .enter 导致后续行瞬显
  box.addEventListener("animationend", function onEnd() {
    box.classList.remove("enter");
    box.removeEventListener("animationend", onEnd);
  }, { once: true });
}
function pluginRow(p) {
  const row = document.createElement("div");
  row.className = "pluginRow";
  // 徽标：已启用 / 已禁用（patch 层）/ 可更新
  const badges = [];
  if (p.patchDisabled) badges.push('<span class="badge WARN">已禁用</span>');
  else if (p.enabled === true) badges.push('<span class="badge OK">已启用</span>');
  else if (p.enabled === false) badges.push('<span class="badge WARN">未启用</span>');
  // 仅在「检查更新」发现新版本后显示更新入口
  const up = updates && updates[p.name];
  const hasUpdate = !!(up && up.latest && cmpVer(up.latest, up.installed) > 0);
  if (hasUpdate) badges.push('<span class="badge WARN">可更新</span>');
  const ver = p.version ? '<span class="pluginVer">v' + escapeHtml(p.version) + '</span>' : "";
  const newVer = hasUpdate ? '<span class="pluginNew">→ v' + escapeHtml(up.latest) + '</span>' : "";
  // 摘要行右侧：仅在有更新时放快捷更新按钮 + 展开箭头
  const quick = hasUpdate
    ? `<button class="btn mini accent" data-act="update" data-name="${escapeAttr(p.name)}">更新 v${escapeAttr(up.latest)}</button>`
    : "";
  // 详情面板操作（展开后可见）
  const ops = [];
  if (p.isBundle && p.entryIds && p.entryIds.length) {
    ops.push(`<button class="btn mini" data-act="toggle" data-name="${escapeAttr(p.name)}" data-enable="${p.patchDisabled ? "1" : "0"}">${p.patchDisabled ? "启用" : "禁用"}</button>`);
  }
  if (hasUpdate) {
    ops.push(`<button class="btn mini accent" data-act="update" data-name="${escapeAttr(p.name)}">更新 v${escapeAttr(up.latest)}</button>`);
  }
  ops.push(`<button class="btn mini" data-act="remove" data-name="${escapeAttr(p.name)}">卸载</button>`);
  row.innerHTML = `
    <div class="pluginMain">
      <span class="pluginName" title="${escapeAttr(p.name)}">${escapeHtml(p.name)}</span>
      ${badges.join("")}
      ${ver}${newVer}
    </div>
    <div class="pluginActions">
      ${quick}
      <button class="rowToggle" data-act="toggle-detail" data-name="${escapeAttr(p.name)}" title="详情与操作">▾</button>
    </div>
    <div class="pluginDesc">${escapeHtml(p.description || "（无描述）")}</div>
    <div class="pluginDetail" hidden></div>
    <div class="pluginOpRow" hidden>${ops.join("")}</div>`;
  row.dataset.name = p.name; // 整行点击展开时使用
  // 详情按钮是唯一的键盘展开入口，避免可交互行内嵌套 role=button。
  row.querySelector(".rowToggle").setAttribute("aria-expanded", "false");
  return row;
}
// 清洗 README：去掉对使用者无用的 markdown 原文（图片/徽章/代码块/链接地址/HTML），
// 只留可读文本，最多 400 字符
function cleanReadme(md) {
  let s = String(md);
  s = s.replace(/```[\s\S]*?```/g, " ");      // 代码块
  s = s.replace(/`[^`]*`/g, " ");             // 行内代码
  s = s.replace(/!\[[^\]]*\]\([^)]*\)/g, ""); // 图片（含徽章内嵌图）
  s = s.split("\n").map((l) => {
    const t = l.trim();
    if (!t) return "";
    if (/^!\[/.test(t) || /^\[!\[/.test(t) || /^<[^>]+>$/.test(t)) return ""; // 图片行/徽章行/HTML 行
    return l;
  }).join("\n");
  s = s.replace(/\[([^\]]*)\]\([^)]*\)/g, "$1"); // 链接文本化
  s = s.replace(/<[^>]+>/g, "");                // 残留 HTML 标签
  s = s.split("\n").filter((l) => {
    const t = l.trim();
    return t && !/^[\s:：\-—•*#]+$/.test(t);    // 去掉清洗后只剩标点/符号的行
  }).join("\n");
  s = s.replace(/\n{3,}/g, "\n\n");             // 压缩空行
  s = s.trim();
  if (s.length > 400) s = s.slice(0, 400) + "…";
  return s;
}
function renderDetail(d) {
  const meta = [];
  if (d.version) meta.push(`<span class="metaItem"><b>版本</b> v${escapeHtml(d.version)}</span>`);
  if (d.spec) meta.push(`<span class="metaItem"><b>依赖范围</b> ${escapeHtml(d.spec)}</span>`);
  if (d.license) meta.push(`<span class="metaItem"><b>许可</b> ${escapeHtml(d.license)}</span>`);
  if (d.repository) meta.push(`<span class="metaItem"><b>仓库</b> ${escapeHtml(d.repository)}</span>`);
  if (d.entryIds && d.entryIds.length) meta.push(`<span class="metaItem"><b>入口</b> ${escapeHtml(d.entryIds.join(", "))}</span>`);
  let html = meta.length ? '<div class="pluginMeta">' + meta.join("") + "</div>" : "";
  if (d.homepage) {
    html += '<div class="pluginLinks"><a href="#" data-url="' + escapeAttr(d.homepage) + '">主页 ↗</a></div>';
  }
  // README 只展示清洗后的可读简介（纯 markdown 原文对使用者无意义）
  const intro = d.readme ? cleanReadme(d.readme) : "";
  if (intro) {
    html += '<div class="pluginReadme">' + escapeHtml(intro) + "</div>";
  }
  return html;
}
async function toggleDetail(target) {
  const row = target.closest(".pluginRow");
  const detail = row.querySelector(".pluginDetail");
  const opRow = row.querySelector(".pluginOpRow");
  // 展开/收起：箭头旋转由 CSS（.pluginRow.expanded .rowToggle）负责，不再改文本
  const expanded = row.classList.toggle("expanded");
  row.querySelector(".rowToggle").setAttribute("aria-expanded", String(expanded));
  if (!expanded) {
    detail.hidden = true;
    opRow.hidden = true;
    return;
  }
  detail.hidden = false;
  opRow.hidden = false;
  if (detail.dataset.loaded) return;
  detail.innerHTML = '<div class="pluginDetailLoading sub">加载中…</div>';
  try {
    const d = await invoke("plugin_detail", { name: row.dataset.name });
    detail.dataset.loaded = "1";
    detail.innerHTML = renderDetail(d);
  } catch (e) {
    detail.innerHTML = '<div class="pluginDetailLoading sub">加载失败：' + escapeHtml(String(e)) + "</div>";
  }
}

async function loadPlugins() {
  const request = ++pluginListRequest;
  try {
    const res = await invoke("plugin_list");
    if (request !== pluginListRequest) return;
    renderPlugins(res);
  } catch (e) {
    if (request !== pluginListRequest) return;
    const box = $("pluginList");
    box.classList.toggle("busy", pluginBusy);
    box.innerHTML = '<div class="pluginEmpty sub">加载失败：' + escapeHtml(String(e)) + "</div>";
    setActivity("插件列表加载失败：" + cleanMsg(e));
  }
}
async function installPlugin() {
  const input = $("pluginNameInput");
  const name = input.value.trim();
  if (!name) { setActivity("请输入要安装的插件包名"); return; }
  if (pluginBusy) return;
  setPluginBusy(true);
  try {
    const ver = await invoke("plugin_install", { name, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress: pluginProgress("正在安装 " + name) });
    setActivity("插件 " + name + " 安装完成" + (ver ? "（v" + ver + "）" : "") + "，重启 DSH 后生效");
    input.value = "";
    resetPluginUpdates();
    await loadPlugins();
  } catch (e) { showError("安装失败", e); }
  finally { setPluginBusy(false); }
}
async function removePlugin(name) {
  if (pluginBusy) return;
  if (!(await uiConfirm({ title: "卸载插件", message: "卸载插件 " + name + "？\n将执行 dsh plugin --profile " + activePluginProfile + " remove " + name + "：\n从 profile 移除依赖并停用该插件。", okText: "卸载", danger: true }))) return;
  setPluginBusy(true);
  try {
    await invoke("plugin_remove", { name, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress: pluginProgress("正在卸载 " + name) });
    setActivity("插件 " + name + " 已卸载，重启 DSH 后生效");
    resetPluginUpdates();
    await loadPlugins();
  } catch (e) { showError("卸载失败", e); }
  finally { setPluginBusy(false); }
}
// 检查所有用户插件的 registry 最新版本（后端并行 npm view），
// 结果缓存到 updates 并在渲染时决定是否显示「更新」按钮；
// 「全部更新」按钮仅在发现有可更新插件后出现（带数量，紫色区别于行内按钮）。
async function checkUpdates(ignoreBusy = false) {
  if (pluginBusy && !ignoreBusy) return;
  setPluginBusy(true);
  setActivity("正在检查插件更新…");
  try {
    const res = await invoke("plugin_check_updates", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    updates = {};
    let updatable = 0, failed = 0, skipped = 0;
    for (const u of res) {
      updates[u.name] = u;
      if (u.latest && cmpVer(u.latest, u.installed) > 0) updatable++;
      else if (u.error) failed++;
      else if (!u.latest) skipped++;
    }
    const allBtn = $("pluginUpdateAllBtn");
    if (updatable > 0) {
      allBtn.hidden = false;
      allBtn.textContent = "全部更新（" + updatable + "）";
    } else {
      allBtn.hidden = true;
    }
    await loadPlugins();
    if (updatable > 0) {
      setActivity("发现 " + updatable + " 个插件可更新" + (failed ? "（" + failed + " 个检查失败）" : ""));
    } else if (failed > 0) {
      setActivity("检查完成：未发现可更新插件（" + failed + " 个插件检查失败）");
    } else if (skipped > 0) {
      setActivity("未发现可更新插件（" + skipped + " 个本地依赖未检查）");
    } else if (res.length === 0) {
      setActivity("当前 profile 没有可检查的用户插件");
    } else {
      setActivity("所有插件已是最新版本");
    }
  } catch (e) {
    $("pluginUpdateAllBtn").hidden = true;
    setActivity("检查更新失败：" + cleanMsg(e));
  }
  finally { setPluginBusy(false); }
}
async function updatePlugin(name) {
  if (pluginBusy) return;
  setPluginBusy(true);
  try {
    await invoke("plugin_update", { all: false, name, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress: pluginProgress("正在更新 " + name) });
    setActivity("插件 " + name + " 更新完成，重启 DSH 后生效");
    // 更新完成后自动复查，让「更新」按钮随最新状态消失/保留
    resetPluginUpdates();
    await loadPlugins();
    await checkUpdates(true);
  } catch (e) { showError("更新失败", e); }
  finally { setPluginBusy(false); }
}
async function updateAllPlugins() {
  if (pluginBusy) return;
  if (!(await uiConfirm({ title: "全部更新", message: "将所有已安装插件更新到最新版本？\n将执行 pnpm update --latest（在 " + activePluginProfile + " profile 目录）。", okText: "全部更新" }))) return;
  setPluginBusy(true);
  try {
    await invoke("plugin_update", { all: true, name: null, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress: pluginProgress("正在更新全部插件") });
    setActivity("全部插件更新完成，重启 DSH 后生效");
    resetPluginUpdates();
    await loadPlugins();
    await checkUpdates(true);
  } catch (e) { showError("更新失败", e); }
  finally { setPluginBusy(false); }
}

async function togglePlugin(name, enable) {
  if (pluginBusy) return;
  if (!enable && !(await uiConfirm({ title: "禁用插件", message: "禁用插件 " + name + "？\n将在 cordis.patch.yml 添加 disabled 条目（保留安装、暂时停用），重启 DSH 后生效。", okText: "禁用" }))) return;
  setPluginBusy(true);
  try {
    await invoke("plugin_toggle", { name, enable });
    setActivity("插件 " + name + (enable ? "已启用" : "已禁用") + "，重启 DSH 后生效");
    await loadPlugins();
  } catch (e) { showError((enable ? "启用" : "禁用") + "失败", e); }
  finally { setPluginBusy(false); }
}

$("pluginInstallBtn").addEventListener("click", installPlugin);
$("pluginNameInput").addEventListener("keydown", (e) => { if (e.key === "Enter") installPlugin(); });
$("pluginCheckBtn").addEventListener("click", checkUpdates);
$("pluginUpdateAllBtn").addEventListener("click", updateAllPlugins);
$("pluginList").addEventListener("click", (e) => {
  // 详情里的外链：用系统浏览器打开
  const link = e.target.closest("a[data-url]");
  if (link) {
    e.preventDefault();
    invoke("open_url", { url: link.dataset.url }).catch(() => {});
    return;
  }
  // 行内按钮
  const btn = e.target.closest("button[data-act]");
  if (btn) {
    if (pluginBusy) return;
    const name = btn.dataset.name;
    const act = btn.dataset.act;
    if (act === "update") updatePlugin(name);
    else if (act === "remove") removePlugin(name);
    else if (act === "toggle") togglePlugin(name, btn.dataset.enable === "1");
    else if (act === "toggle-detail") toggleDetail(btn);
    return;
  }
  // 整行点击（不含已展开的详情/操作区内部）展开或收起
  if (pluginBusy) return;
  if (e.target.closest(".pluginDetail, .pluginOpRow")) return;
  const row = e.target.closest(".pluginRow");
  if (row) toggleDetail(row);
});
// 键盘：详情按钮本身支持 Enter / Space；行内其它按钮不触发展开。
$("pluginList").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  if (!e.target.closest(".rowToggle")) return;
  const row = e.target.closest(".pluginRow");
  if (!row || pluginBusy) return;
  e.preventDefault();
  toggleDetail(row);
});

// ---------------- 初始化 ----------------
refreshChecks();
setView("home");              // 应用 data-view 显隐，初始为首页
setTimeout(pollStatus, 200);   // 首次状态轮询
// 启动后按设置决定是否静默检查 DSH 更新
invoke("get_settings").then((s) => {
  closeAction = s.closeAction;
}).catch(() => {});
if (cfg.autoCheckUpdate !== false) setTimeout(checkUpdate, 3500);
// Web 地址与版本号从后端读取（版本与 Cargo.toml 保持一致）
invoke("get_web_url").then(u => { if (u) webUrl = u; }).catch(() => {});
window.__TAURI__.app.getVersion().then(v => {
  $("version").textContent = "v" + v;
  $("title").dataset.ver = v; // 标题右侧的版本徽标
}).catch(() => {});
