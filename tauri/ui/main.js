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

// 把 webUrl 渲染到界面的端口与 URL 两处
function renderWebUrl() {
  const port = (() => { try { return new URL(webUrl).port; } catch (_) { return ""; } })();
  setEl("runtimePort", "textContent", port || "80");
  setEl("runtimeUrl", "textContent", webUrl);
}

// 从后端重新读取 Web 地址。改端口后必须调用——webUrl 只在启动时取过一次，
// 不刷新的话界面仍显示旧端口，用户会以为改动没生效。
async function refreshWebUrl() {
  try {
    const u = await invoke("get_web_url");
    if (u) webUrl = u;
  } catch (e) { /* 读取失败保留原值 */ }
  renderWebUrl();
}

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
$("recheckBtn")?.addEventListener("click", refreshChecks);

$("fixBtn")?.addEventListener("click", async () => {
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
  const runtimeLabel = $("runtimeLabel");
  const runtimeBadge = $("runtimeBadge");
  const runtimeIcon = $("runtimeStateIcon");
  const sideText = $("sideStatusText");
  if (runtimeLabel) runtimeLabel.textContent = state === "running" ? "DSH Web 服务运行中" : state === "foreign-port" ? "端口被其他程序占用" : state === "stopped" ? "DSH Web 服务未运行" : "DSH Web 服务状态未知";
  if (runtimeBadge) { runtimeBadge.textContent = state === "running" ? "正常" : state === "stopped" ? "未运行" : state === "foreign-port" ? "警告" : "检查中"; runtimeBadge.className = "badge" + (state === "running" ? "" : " info"); }
  if (runtimeIcon) runtimeIcon.textContent = state === "running" ? "✓" : state === "stopped" ? "–" : "!";
  if (sideText) sideText.textContent = state === "running" ? "DSH 运行中" : state === "stopped" ? "DSH 未运行" : "DSH 状态检查中";
}
// 端口占用处置面板：只在"进入 foreign-port"这个边沿查一次占用者，
// 而不是随 2 秒轮询反复起 PowerShell 查询
let lastPortState = "";
function applyStatus(state) {
  setPill(state);
  const box = $("portConflict");
  if (!box) return;
  if (state === "foreign-port") {
    box.hidden = false;
    if (lastPortState !== "foreign-port") refreshPortOwner();
  } else {
    box.hidden = true;
    // 复位"结束占用进程"的可见性：上一次可能是 DSH 自身把它隐藏了
    const killBtn = $("pcKillBtn");
    if (killBtn) killBtn.hidden = false;
  }
  lastPortState = state;
}
async function pollStatus() {
  try {
    const d = await invoke("status_detail");
    pollFailures = 0;
    applyStatus(
      d.state === "running" ? "running" : d.state === "foreign-port" ? "foreign-port" : "stopped"
    );
  } catch (e) {
    pollFailures += 1;
    if (pollFailures >= 3) setPill("unknown");
  }
  setTimeout(pollStatus, document.hidden ? 10000 : 2000);
}
// 手动立即刷新一次。刻意不复用 pollStatus：它末尾会自续期，
// 直接调用会再开一条永续轮询（双轮询陷阱）。
async function pollOnce() {
  try {
    const d = await invoke("status_detail");
    pollFailures = 0;
    applyStatus(
      d.state === "running" ? "running" : d.state === "foreign-port" ? "foreign-port" : "stopped"
    );
  } catch (e) { /* 手动刷新失败不改动胶囊 */ }
}

// ---------------- 端口占用处置 ----------------
/// 查询并展示当前占用 webPort 的进程（只提示"端口被占用"用户无从下手）
async function refreshPortOwner() {
  setEl("pcBody", "textContent", "正在查询占用者…");
  try {
    const o = await invoke("port_owner");
    const el = $("pcBody");
    if (!el) return;
    if (!o) {
      el.textContent = "当前未检测到监听进程（可能刚刚释放）。点「重新检测」刷新状态。";
      return;
    }
    const name = o.name || o.path || "未知进程";
    const isDsh = !!(o.isDsh || o.likelyDsh);
    let text = "占用者：" + name + "（PID " + o.pid + "）";
    if (o.isDsh) {
      text += "\n这是 DSH 自身——请用上方「关闭 Web 界面」停止它，不要强制结束。";
    } else if (o.likelyDsh) {
      text += "\n端口后面确实是 DSH web，但它以更高权限运行，启动器读不到它的进程信息。"
        + "\n请用上方「关闭 Web 界面」；若关闭失败，点「以管理员身份重启启动器」后重试。";
    } else if (o.path && o.name) {
      text += "\n路径：" + o.path;
    }
    el.textContent = text;
    // 是 DSH 就不提供"结束占用进程"——那个按钮是给无关程序用的
    const killBtn = $("pcKillBtn");
    if (killBtn) {
      killBtn.hidden = isDsh;
      killBtn.disabled = isDsh;
    }
    // 顺手给一个可用的建议端口（+1），省得用户自己想
    const input = $("pcPortInput");
    if (input && !input.value) {
      const cur = Number((String(webUrl).match(/:(\d+)/) || [])[1] || 0);
      if (cur > 0 && cur < 65535) input.placeholder = String(cur + 1);
    }
  } catch (e) {
    setEl("pcBody", "textContent", "占用者查询失败：" + cleanMsg(e));
  }
}
$("pcKillBtn")?.addEventListener("click", async () => {
  const btn = $("pcKillBtn");
  if (btn.disabled) return;
  let owner = null;
  try { owner = await invoke("port_owner"); } catch (e) { /* 下面按未取到处理 */ }
  if (!owner) { setActivity("端口当前未被占用，无需结束进程"); refreshPortOwner(); return; }
  if (owner.isDsh) { setActivity("占用端口的是 DSH 自身，请用「关闭 Web 界面」停止"); return; }
  const who = (owner.name || owner.path || "未知进程") + "（PID " + owner.pid + "）";
  if (!(await uiConfirm({
    title: "结束占用进程",
    message: "将强制结束 " + who + "，该进程会立即丢失未保存的数据。\n\n确认这个进程可以关闭再继续。",
    okText: "结束进程",
    danger: true,
  }))) return;
  btn.disabled = true; btn.textContent = "结束中…";
  try {
    setActivity(await invoke("kill_port_owner", { pid: owner.pid }));
    await pollOnce();
    refreshPortOwner();
  } catch (e) {
    showError("结束进程失败", e);
  } finally {
    btn.disabled = false; btn.textContent = "结束占用进程";
  }
});
$("pcRecheckBtn")?.addEventListener("click", async () => { await pollOnce(); refreshPortOwner(); });
$("pcPortApply")?.addEventListener("click", async () => {
  const input = $("pcPortInput");
  const v = parseInt(String(input.value).trim(), 10);
  if (!Number.isInteger(v) || v < 1 || v > 65535) {
    setActivity("端口需为 1–65535 之间的整数");
    return;
  }
  const btn = $("pcPortApply");
  btn.disabled = true; btn.textContent = "保存中…";
  try {
    await invoke("save_settings", { patch: { webPort: v } });
    await refreshWebUrl();
    setActivity("Web 端口已改为 " + v + "，正在启动 DSH…");
    await startWeb(false);
  } catch (e) {
    showError("修改端口失败", e);
  } finally {
    btn.disabled = false; btn.textContent = "保存并启动";
  }
});

// 操作因权限失败时的统一出口。
// DSH 以更高权限（管理员）运行时，启动器既读不到它的命令行、也结束不了它
// （taskkill 返回"拒绝访问"），唯一出路是以管理员身份重启启动器。
function isElevationNeeded(e) {
  return String(e).includes("以管理员身份");
}
async function offerElevation(title, message) {
  const ok = await uiConfirm({
    title,
    message: message + "\n\n是否以管理员身份重启启动器后重试？",
    okText: "以管理员身份重启",
  });
  if (!ok) return false;
  try {
    await invoke("relaunch_as_admin");
    setActivity("正在以管理员身份重启启动器，请在 UAC 窗口点「是」…");
  } catch (e) {
    showError("提权重启失败", e);
  }
  return true;
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
$("webBtn")?.addEventListener("click", async () => {
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
$("browserBtn")?.addEventListener("click", () => { invoke("open_browser"); setActivity("已调用浏览器打开 " + webUrl); });
// 「关闭 Web 界面」：终止 DSH Web 进程，不重启
$("webCloseBtn")?.addEventListener("click", async () => {
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
      await offerElevation("需要管理员权限", "DSH 以更高权限运行，非提权的启动器无法结束它。");
    } else if (isElevationNeeded(e)) {
      showError("关闭失败", e);
      await offerElevation("需要管理员权限", "DSH 以更高权限运行，非提权的启动器无法结束它。");
    } else { setActivity("关闭失败：" + cleanMsg(e)); }
  } finally {
    btn.disabled = false; btn.textContent = "关闭 Web 界面";
  }
});
// TUI 首次点击会自动安装插件（可能耗时数十秒），同样需要 busy 防抖
let tuiStartBusy = false;
$("tuiBtn")?.addEventListener("click", async () => {
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
$("dirBtn")?.addEventListener("click", async () => {
  try { setActivity(await invoke("open_install_dir")); }
  catch (e) { setActivity("打开安装目录失败：" + cleanMsg(e)); }
});
$("cmdBtn")?.addEventListener("click", async () => {
  try {
    const cmd = await invoke("get_web_cmd");
    await navigator.clipboard.writeText(cmd);
    setActivity("已复制 Web 启动命令到剪贴板");
  } catch (e) { setActivity("复制失败：" + cleanMsg(e)); }
});

// ---------------- 重启 DSH ----------------
// 用 status_detail（三态）而非 status（"tracked 或端口占用"二元口径）：
// webPort 被无关程序占用时 status 也返回 true，会把用户带进确认框、
// 再由后端以 foreign-port 拒绝，白跑一趟。这里提前分流。
$("restartBtn")?.addEventListener("click", async () => {
  const btn = $("restartBtn");
  if (btn.disabled) return;
  let state = "stopped", port = "";
  try {
    const d = await invoke("status_detail");
    state = d.state; port = d.port;
  } catch (e) { setActivity("状态检测失败：" + cleanMsg(e)); return; }
  if (state === "foreign-port") {
    setActivity("端口 " + port + " 被其他程序占用（非 DSH 进程），无法重启；请停止占用程序或修改 settings.json 的 webPort");
    return;
  }
  if (state !== "running") {
    await startWeb(false);
    setActivity("DSH 未在运行，已直接启动…");
    return;
  }
  if (!(await uiConfirm({ title: "重启 DSH", message: "将终止当前 DSH（" + webUrl + "）并重新启动。\n当前 Web 界面（包括正在进行的会话）会中断，重启后恢复。", okText: "重启" }))) return;
  btn.disabled = true; btn.textContent = "重启中…";
  setActivity("正在重启 DSH…");
  try {
    const r = await invoke("restart_dsh", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "ok") setActivity("DSH 已重启完成（Web 模式后台运行中）");
    else if (r === "starting") setActivity("DSH 重启中：进程已启动，等待端口就绪…");
  } catch (e) {
    if (String(e).includes("still-running")) {
      setActivity("未能停止现有 DSH 进程（可能权限不足），未重新启动");
      await offerElevation("需要管理员权限", "现有 DSH 以更高权限运行，非提权的启动器无法结束它，因此无法重启。");
    } else if (isElevationNeeded(e)) {
      showError("重启失败", e);
      await offerElevation("需要管理员权限", "现有 DSH 以更高权限运行，非提权的启动器无法结束它，因此无法重启。");
    } else { showError("重启失败", e); }
  } finally {
    btn.disabled = false; btn.textContent = "重启 DSH";
  }
});

// ---------------- 代理（设置页） ----------------
// 初值回填同样要守卫：元素缺失时直接赋值会抛异常，中断其后所有绑定
const proxySwitchEl = $("setProxySwitch");
const proxyInputEl = $("setProxyInput");
if (proxySwitchEl) proxySwitchEl.checked = cfg.proxyEnabled;
if (proxyInputEl) proxyInputEl.value = cfg.proxyAddr;
$("setProxySwitch")?.addEventListener("change", async (e) => {
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
$("setProxyInput")?.addEventListener("change", () => { cfg.proxyAddr = $("setProxyInput").value.trim(); saveCfg(); });
$("setProxyImport")?.addEventListener("click", async () => {
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
  if (!btn) return;
  btn.textContent = text;
  btn.classList.toggle("accent", !!accent);
}
// 横幅可见性 = bannerVisible（更新检查逻辑维护） && 当前在首页（视图切换维护），
// 二者解耦：离开首页不丢状态，返回首页按状态恢复；非首页时检查更新也不会误显示
let bannerVisible = false;
function applyBannerVisibility() {
  setEl("banner", "hidden", !(bannerVisible && currentView === "home"));
}
function showBanner(latest, installed) {
  setEl("bannerText", "textContent", "发现 DSH 新版本 v" + latest + "（当前 v" + installed + "），点击右侧「立即更新」手动升级");
  setEl("bannerBtn", "textContent", "立即更新");
  bannerVisible = true;
  applyBannerVisibility();
}
function showInstallBanner(latest) {
  setEl("bannerText", "textContent", "未检测到 DSH 程序，点击右侧「立即安装」（npm 全局安装）" + (latest ? "，将安装最新版 v" + latest : ""));
  setEl("bannerBtn", "textContent", "立即安装");
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

$("updateBtn")?.addEventListener("click", () => {
  if (updateState.busy) return;
  if (!updateState.installed) { confirmInstall(); }
  else if (updateState.available) { confirmUpdate(); }
  else { checkUpdate(); }
});
$("repoBtn")?.addEventListener("click", async () => {
  try {
    await invoke("open_url", { url: DSH_REPOSITORY_URL });
    setActivity("已在浏览器打开 DeepSeek Harness GitHub 仓库");
  } catch (e) {
    setActivity("打开 GitHub 仓库失败：" + cleanMsg(e));
  }
});
$("sideBrowserBtn")?.addEventListener("click", () => $("browserBtn").click());
$("sideRestartBtn")?.addEventListener("click", () => $("restartBtn").click());
$("sideLogsBtn")?.addEventListener("click", () => $("logLink").click());
$("runtimeLogBtn")?.addEventListener("click", () => $("logLink").click());
$("advancedLink")?.addEventListener("click", (e) => { e.preventDefault(); setView("settings"); });
$("bannerBtn")?.addEventListener("click", () => {
  if (updateState.busy) return;
  if (updateState.installed) { confirmUpdate(); } else { confirmInstall(); }
});

// ---------------- 更新 / 安装前置检查 ----------------
// 更新失败的两种常见原因都能提前判定，而解法完全不同：
//   ① 目标目录不可写（npm 全局目录常常只给普通用户「读+执行」）→ 需要提权重启启动器
//   ② DSH 正在运行（Windows 上 node 会占用待替换的文件）→ 需要先停止 DSH
// 与其让用户对着 npm 的 EPERM 或 EACCES 发呆，不如在这里分流。
async function ensureUpdatable() {
  let p;
  try {
    p = await invoke("update_preflight");
  } catch (e) {
    return true; // 拿不到前置信息就不拦，交给后端的强制检查兜底
  }
  if (p.dshRunning) {
    if (!(await uiConfirm({
      title: "先停止 DSH",
      message: "更新要替换 DSH 的程序文件，而 DSH 正在运行会占用这些文件。\n\n是否先停止 DSH 再继续？",
      okText: "停止并更新",
    }))) return false;
    try {
      await invoke("stop_web");
    } catch (e) {
      showError("停止 DSH 失败", e);
      return false;
    }
  }
  if (p.needsAdmin) {
    const ok = await uiConfirm({
      title: "需要管理员权限",
      message: "更新要写入下面的目录，但它对当前用户只开放「读取 + 执行」权限：\n" + p.targetDir
        + "\n\n以管理员身份重启启动器后即可完成更新。\n会弹出 Windows UAC 授权窗口，本窗口随后关闭。",
      okText: "以管理员身份重启",
    });
    if (!ok) return false;
    try {
      await invoke("relaunch_as_admin");
      setActivity("正在以管理员身份重启启动器，请在 UAC 窗口点「是」…");
    } catch (e) {
      showError("提权重启失败", e);
    }
    return false; // 本实例即将退出，不再继续
  }
  return true;
}

// 一键安装（npm install -g @deepseek-ai/dsh）
async function confirmInstall() {
  if (updateState.busy) return;
  if (!(await uiConfirm({ title: "安装 DSH", message: "未检测到 DSH 程序。\n将执行 npm install -g @deepseek-ai/dsh（全局安装），需联网下载，请稍候。", okText: "安装" }))) return;
  if (!(await ensureUpdatable())) return;
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
  if (!(await ensureUpdatable())) return;
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
  if (!["home", "plugins", "settings"].includes(view)) view = "home";
  currentView = view;
  setEl("viewHome", "hidden", view !== "home");
  setEl("viewPlugins", "hidden", view !== "plugins");
  setEl("viewSettings", "hidden", view !== "settings");
  document.querySelectorAll("[data-view]").forEach((el) => { el.hidden = el.dataset.view !== view; });
  document.querySelectorAll(".navItem").forEach((el) => el.classList.toggle("active", el.id === (view === "home" ? "homeBtn" : view === "plugins" ? "pluginMgrBtn" : "settingsBtn")));
  // 更新横幅显隐 = bannerVisible && 首页，状态不因切换视图丢失
  applyBannerVisibility();
  if (view === "plugins") {
    setEl("productKicker", "textContent", "PLUGINS / MANAGEMENT");
    setEl("title", "textContent", "插件管理");
    setEl("subtitle", "textContent", "管理已安装插件与 profile");
  loadPlugins(); // 每次进入插件页刷新列表
  } else if (view === "settings") {
    setEl("productKicker", "textContent", "SETTINGS / PREFERENCES");
    setEl("title", "textContent", "设置");
    setEl("subtitle", "textContent", "调整外观、启动行为、网络与高级功能");
    loadSettings(); // 每次进入设置页刷新配置
  } else {
    setEl("productKicker", "textContent", "CONTROL CENTER / WORKSPACE");
    setEl("title", "textContent", "DSH 工作台");
    setEl("subtitle", "textContent", "以 Web 服务为中心管理启动、插件和运行状态");
  }
}
$("homeBtn")?.addEventListener("click", () => setView("home"));
$("pluginMgrBtn")?.addEventListener("click", () => setView("plugins"));
$("settingsBtn")?.addEventListener("click", () => setView("settings"));

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
    // Web 服务端口
    setEl("setPortInput", "value", s.webPort == null ? "" : String(s.webPort));
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
$("setAutoCheck")?.addEventListener("change", (e) => {
  cfg.autoCheckUpdate = e.target.checked;
  saveCfg();
  setActivity(e.target.checked ? "已开启启动时自动检查更新" : "已关闭启动时自动检查更新");
});
$("setAutoBrowser")?.addEventListener("change", (e) => {
  cfg.autoOpenBrowser = e.target.checked;
  saveCfg();
  setActivity(e.target.checked ? "已开启自动打开浏览器" : "已关闭自动打开浏览器");
});
// 开机自启（注册表）
$("setAutoStart")?.addEventListener("change", async (e) => {
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
$("setCloseSeg")?.addEventListener("click", async (e) => {
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
$("setProfileSelect")?.addEventListener("change", async (e) => {
  try {
    const s = await invoke("save_settings", { patch: { pluginProfile: e.target.value } });
    settingsState = s;
    activePluginProfile = s.pluginProfile;
    resetPluginUpdates(); // 插件视图的更新缓存作废
    updateSideVersion();  // 侧栏版本行的 profile 名同步
    setActivity("插件管理 profile 已切换为 " + s.pluginProfile + "，进入插件视图生效");
  } catch (err) {
    setActivity("保存 profile 失败：" + cleanMsg(err));
    loadSettings();
  }
});
// registry 镜像
$("setRegistryInput")?.addEventListener("change", async (e) => {
  try {
    const s = await invoke("save_settings", { patch: { registry: e.target.value.trim() } });
    settingsState = s;
    setActivity(s.registry ? "registry 镜像已设为 " + s.registry : "已恢复官方 npm registry");
  } catch (err) {
    setActivity("保存 registry 失败：" + cleanMsg(err));
    $("setRegistryInput").value = settingsState.registry;
  }
});
// Web 服务端口（1–65535）。此前只能在 settings.json 里手改并重启启动器，
// 「端口被占用」的提示因此等于没有解法。
$("setPortInput")?.addEventListener("change", async (e) => {
  const raw = String(e.target.value).trim();
  const v = parseInt(raw, 10);
  if (!/^\d+$/.test(raw) || !Number.isInteger(v) || v < 1 || v > 65535) {
    setActivity("Web 端口需为 1–65535 之间的整数");
    e.target.value = settingsState.webPort == null ? "" : String(settingsState.webPort);
    return;
  }
  if (v === settingsState.webPort) return;
  try {
    const s = await invoke("save_settings", { patch: { webPort: v } });
    settingsState = s;
    await refreshWebUrl();
    setActivity("Web 端口已改为 " + s.webPort + "；若 DSH 正在运行，请点「重启 DSH」使其生效");
    refreshChecks();
  } catch (err) {
    setActivity("保存端口失败：" + cleanMsg(err));
    e.target.value = settingsState.webPort == null ? "" : String(settingsState.webPort);
  }
});
// ---------------- 数据备份 / 恢复 ----------------
// 互斥保护：备份与恢复不能同时进行（恢复含 pnpm 重建插件，可能耗时较长）
let backupBusy = false;
function setBackupBusy(b) {
  backupBusy = b;
  $("backupList")?.classList.toggle("busy", b); // busy 类由 style.css 提供置灰/禁用观感
  setEl("backupBtn", "disabled", b);
  setEl("backupDirBtn", "disabled", b);
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

// 「备份 DSH 数据」：确认后打包配置/对话历史/插件清单到 exe 旁 backups/（后端自动保留 5 份）。
// 打包大 $DSH_HOME 可能耗时数分钟，进度经 Channel 落到日志面板与活动栏。
$("backupBtn")?.addEventListener("click", async () => {
  if (backupBusy) return;
  if (!(await uiConfirm({ title: "备份 DSH 数据", message: "将打包 DSH 配置、对话历史与插件清单到备份目录。\n\n备份文件包含 API 密钥等敏感凭据，请妥善保管。\n（DSH 运行中不可备份，请先停止）", okText: "备份" }))) return;
  setBackupBusy(true);
  try {
    const progress = new window.__TAURI__.core.Channel();
    progress.onmessage = (line) => {
      const s = String(line).trim();
      if (!s) return;
      setActivity(s.length > 90 ? s.slice(0, 87) + "…" : s);
      logPanel.append(s);
    };
    logPanel.title("备份 DSH 数据");
    logPanel.clear();
    logPanel.open();
    const r = await invoke("backup_dsh", { progress });
    setActivity("备份完成：" + r.fileName);
    await loadBackups();
  } catch (e) { showError("备份失败", e); }
  finally { setBackupBusy(false); }
});

// 「打开备份目录」：在资源管理器中定位 backups/
$("backupDirBtn")?.addEventListener("click", async () => {
  try {
    setActivity(await invoke("open_backups_dir"));
  } catch (e) { setActivity("打开备份目录失败：" + cleanMsg(e)); }
});

// 备份列表事件委托：点击行内「恢复」按钮进入恢复流程
$("backupList")?.addEventListener("click", async (e) => {
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
// 更新检查结果缓存：{ [name]: { installed, latest, error, publishedAt, ageMinutes } }；null = 尚未检查
let updates = null;
// 发布年龄阈值（分钟）。依据是真实案例：某插件发布后 17 秒就被"检查更新"查到，
// 用户点更新即失败——registry 元数据与包文件在本地缓存和 CDN 的传播需要时间。
// <60 分钟视为"刚发布"：只展示、不给一键更新入口（详情里仍可强制更新）；
// <24 小时给出年龄提示但仍可一键更新。
const FRESH_MINUTES = 60;
const FRESH_HOURS = 24;
// 把分钟数写成"3 分钟 / 2 小时 / 1 天"
function ageText(min) {
  if (min < 1) return "不到 1 分钟";
  if (min < 60) return min + " 分钟";
  if (min < 1440) return Math.floor(min / 60) + " 小时";
  return Math.floor(min / 1440) + " 天";
}
// 取某插件的发布年龄（分钟）；未知返回 null
function pluginAgeMinutes(name) {
  const u = updates && updates[name];
  return u && typeof u.ageMinutes === "number" ? u.ageMinutes : null;
}
function setPluginBusy(b) {
  pluginBusy = b;
  $("pluginList")?.classList.toggle("busy", b);
  setEl("pluginInstallBtn", "disabled", b);
  setEl("pluginCheckBtn", "disabled", b);
  setEl("pluginUpdateAllBtn", "disabled", b);
}
function resetPluginUpdates() {
  updates = null;
  setEl("pluginUpdateAllBtn", "hidden", true);
  setEl("pluginUpdateAllBtn", "textContent", "全部更新");
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
  setEl("profilePath", "textContent", res.profileDir);
  if (!res.initialized) {
    box.innerHTML = '<div class="pluginEmpty"><div class="emptyIcon">🧩</div><div class="emptyTitle">' + escapeHtml(activePluginProfile) + ' profile 尚未初始化</div><div class="sub">安装第一个插件时会自动创建（' + escapeHtml(res.profileDir) + '）</div></div>';
    return;
  }
  // 内置插件（随 DSH 安装）不展示，仅管理用户插件
  const userPlugins = res.plugins.filter((p) => !p.isBuiltin);
  const summary = $("pluginSummaryText");
  if (summary) summary.innerHTML = '<strong>已安装插件</strong> · ' + escapeHtml(activePluginProfile) + ' profile · ' + userPlugins.length + ' 个用户插件';
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
  // 发布年龄徽标：让"刚发布"这一事实在界面上可见，而不是等更新失败了才知道
  const ageMin = hasUpdate && typeof up.ageMinutes === "number" ? up.ageMinutes : null;
  const tooFresh = ageMin !== null && ageMin < FRESH_MINUTES;
  if (ageMin !== null && !tooFresh) {
    badges.push('<span class="badge INFO">' + escapeHtml("发布 " + ageText(ageMin)) + "</span>");
  } else if (tooFresh) {
    badges.push('<span class="badge WARN">' + escapeHtml("刚发布 " + ageText(ageMin)) + "</span>");
  }
  const ver = p.version ? '<span class="pluginVer">v' + escapeHtml(p.version) + '</span>' : "";
  const newVer = hasUpdate ? '<span class="pluginNew">→ v' + escapeHtml(up.latest) + '</span>' : "";
  // 摘要行右侧：仅在有更新且不是"刚发布"时给一键更新入口。
  // 刚发布的版本默认只展示——避免用户点一次失败一次（详情面板里仍可强制更新）。
  const quick = hasUpdate && !tooFresh
    ? `<button class="btn mini accent" data-act="update" data-name="${escapeAttr(p.name)}">更新 v${escapeAttr(up.latest)}</button>`
    : "";
  // 详情面板操作（展开后可见）
  const ops = [];
  if (p.isBundle && p.entryIds && p.entryIds.length) {
    ops.push(`<button class="btn mini" data-act="toggle" data-name="${escapeAttr(p.name)}" data-enable="${p.patchDisabled ? "1" : "0"}">${p.patchDisabled ? "启用" : "禁用"}</button>`);
  }
  if (hasUpdate) {
    const label = tooFresh ? `仍要更新 v${escapeAttr(up.latest)}` : `更新 v${escapeAttr(up.latest)}`;
    ops.push(`<button class="btn mini accent" data-act="update" data-name="${escapeAttr(p.name)}">${label}</button>`);
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
    let updatable = 0, failed = 0, skipped = 0, fresh = 0;
    for (const u of res) {
      updates[u.name] = u;
      if (u.latest && cmpVer(u.latest, u.installed) > 0) {
        updatable++;
        if (typeof u.ageMinutes === "number" && u.ageMinutes < FRESH_MINUTES) fresh++;
      } else if (u.error) failed++;
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
      setActivity(
        "发现 " + updatable + " 个插件可更新"
        + (fresh ? "（" + fresh + " 个刚发布，建议稍后再更新）" : "")
        + (failed ? "（" + failed + " 个检查失败）" : "")
      );
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
  // 「刚发布」的版本：默认只展示不给一键入口，走到这里说明用户是主动要更，
  // 但仍有必要把风险说清——失败原因不是本地环境问题，等一会儿就好
  const ageMin = pluginAgeMinutes(name);
  if (ageMin !== null && ageMin < FRESH_MINUTES) {
    if (!(await uiConfirm({
      title: "该版本刚发布",
      message: name + " 的最新版本发布于 " + ageText(ageMin) + "前。\n\n"
        + "刚发布的版本可能尚未同步到本地缓存与 registry 的 CDN 边缘，更新容易失败。\n"
        + "建议等 5–30 分钟后再试。\n\n仍然现在更新？",
      okText: "仍要更新",
      danger: true,
    }))) return;
  }
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
  // 列出"刚发布"的插件：全部更新会连带更新它们，很可能失败
  const fresh = Object.keys(updates || {}).filter((n) => {
    const a = pluginAgeMinutes(n);
    return a !== null && a < FRESH_MINUTES;
  });
  const freshNote = fresh.length
    ? "\n\n注意：" + fresh.join("、") + " 刚发布不久，可能因尚未同步完成而失败（失败会自动重试一次）。"
    : "";
  if (!(await uiConfirm({ title: "全部更新", message: "将所有已安装插件更新到最新版本？\n将执行 pnpm update --latest（在 " + activePluginProfile + " profile 目录）。" + freshNote, okText: "全部更新" }))) return;
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

$("pluginInstallBtn")?.addEventListener("click", installPlugin);
$("pluginNameInput")?.addEventListener("keydown", (e) => { if (e.key === "Enter") installPlugin(); });
$("pluginCheckBtn")?.addEventListener("click", checkUpdates);
$("pluginUpdateAllBtn")?.addEventListener("click", updateAllPlugins);
$("pluginList")?.addEventListener("click", (e) => {
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
$("pluginList")?.addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  if (!e.target.closest(".rowToggle")) return;
  const row = e.target.closest(".pluginRow");
  if (!row || pluginBusy) return;
  e.preventDefault();
  toggleDetail(row);
});

// ---------------- 侧栏版本行 ----------------
// 版本号来自 app.getVersion()（与 Cargo.toml 一致），profile 名来自 settings.json。
// 两者都是异步读取，先到者先渲染——不再硬编码版本号/profile，避免与实际漂移。
let appVersion = "";
function updateSideVersion() {
  const el = $("sideVersion");
  if (!el) return;
  const parts = [];
  if (appVersion) parts.push("v" + appVersion);
  parts.push(activePluginProfile + " profile");
  el.textContent = parts.join(" · ");
}

// ---------------- 初始化 ----------------
// 先做元素契约自检：缺失的 id 对应的功能会静默失效（绑定处已用 ?. 兜底，
// 不会中断后续初始化），这里把缺失清单一次性报出来便于定位。
assertRequiredIds();
refreshChecks();
setView("home");              // 应用 data-view 显隐，初始为首页
setTimeout(pollStatus, 200);   // 首次状态轮询
// 启动后按设置决定是否静默检查 DSH 更新
invoke("get_settings").then((s) => {
  closeAction = s.closeAction;
  activePluginProfile = s.pluginProfile || activePluginProfile;
  updateSideVersion();
}).catch(() => {});
if (cfg.autoCheckUpdate !== false) setTimeout(checkUpdate, 3500);
// Web 地址与版本号从后端读取（版本与 Cargo.toml 保持一致）
refreshWebUrl();
window.__TAURI__.app.getVersion().then(v => {
  appVersion = v;
  const verEl = $("version");
  if (verEl) verEl.textContent = "v" + v;
  const titleEl = $("title");
  if (titleEl) titleEl.dataset.ver = v; // 标题右侧的版本徽标
  updateSideVersion();
}).catch(() => {});
