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
    if (raw) return Object.assign({ theme: "system", proxyEnabled: false, proxyAddr: "http://127.0.0.1:7890", history: [], autoCheckUpdate: true, autoOpenBrowser: true }, JSON.parse(raw));
  } catch (e) { /* ignore */ }
  return { theme: "system", proxyEnabled: false, proxyAddr: "http://127.0.0.1:7890", history: [], autoCheckUpdate: true, autoOpenBrowser: true };
}
function saveCfg() {
  localStorage.setItem(CFG_KEY, JSON.stringify(cfg));
}

// ---------------- 主题（深色 / 浅色 / 跟随系统 三态循环） ----------------
const THEME_MODES = ["dark", "light", "system"];
function resolveTheme() {
  if (cfg.theme === "system") {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  return cfg.theme;
}
function themeLabel(mode) {
  return mode === "system" ? "跟随系统" : mode === "dark" ? "深色" : "浅色";
}
function applyTheme() {
  document.documentElement.dataset.theme = resolveTheme();
  // 按钮显示当前模式（点击循环切换：深色 → 浅色 → 跟随系统）
  themeBtn.textContent = cfg.theme === "system" ? "跟随系统" : (cfg.theme === "dark" ? "深色模式" : "浅色模式");
  themeBtn.title = "点击切换主题（深色 → 浅色 → 跟随系统）";
}
themeBtn.addEventListener("click", () => {
  cfg.theme = THEME_MODES[(THEME_MODES.indexOf(cfg.theme) + 1) % THEME_MODES.length];
  applyTheme(); saveCfg(); setActivity("已切换为" + themeLabel(cfg.theme) + "模式");
});
// 系统主题变化时，跟随模式即时刷新
window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
  if (cfg.theme === "system") applyTheme();
});

// ---------------- 工具 ----------------
const $ = (id) => document.getElementById(id);
const activity = $("activity");
function setActivity(msg) { activity.textContent = msg; }

// 全局日志面板：npm/pnpm 完整输出流式显示，或展示 web.log 内容
// 追加走 rAF 攒帧批量渲染：pnpm/npm 高频逐行输出时不再每行触发一次 DOM 写
const logPanel = {
  lines: 0,
  pending: [],
  rafId: 0,
  title(t) { $("logTitle").textContent = t; },
  open() { $("logPanel").hidden = false; },
  close() { $("logPanel").hidden = true; },
  clear() {
    this.pending.length = 0;
    if (this.rafId) { cancelAnimationFrame(this.rafId); this.rafId = 0; }
    $("logBody").textContent = "";
    this.lines = 0;
  },
  append(s) {
    this.pending.push(s);
    if (this.rafId) return;
    this.rafId = requestAnimationFrame(() => {
      this.rafId = 0;
      const el = $("logBody");
      const batch = this.pending.splice(0, this.pending.length);
      if (!batch.length) return;
      el.textContent += batch.join("\n") + "\n";
      this.lines += batch.length;
      if (this.lines > 400) { // 上限保护，只保留最近 300 行
        el.textContent = el.textContent.split("\n").slice(-300).join("\n");
        this.lines = 300;
      }
      el.scrollTop = el.scrollHeight;
    });
  },
  show(title, text) { this.title(title); this.clear(); this.append(text); this.open(); },
};
$("logClose").addEventListener("click", () => logPanel.close());
$("logLink").addEventListener("click", async () => {
  try {
    const [text, exists] = await invoke("read_web_log");
    if (!exists) { logPanel.show("web.log", "web.log 不存在（DSH Web 模式尚未启动过）"); return; }
    logPanel.show("web.log", text.trim() ? text : "（web.log 为空）");
  } catch (e) { logPanel.show("web.log", "读取失败：" + String(e)); }
});

// 自定义确认对话框（替代原生 confirm，与界面风格一致）
// 返回 Promise<boolean>；支持 danger 红色确认按钮
function uiConfirm({ title, message, okText = "确定", cancelText = "取消", danger = false }) {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modalOverlay";
    overlay.innerHTML = `
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modalTitle">${escapeHtml(title)}</div>
        <div class="modalMsg">${escapeHtml(message)}</div>
        <div class="modalBtns">
          <button class="btn modalCancel">${escapeHtml(cancelText)}</button>
          <button class="btn accent${danger ? " danger" : ""} modalOk">${escapeHtml(okText)}</button>
        </div>
      </div>`;
    document.body.appendChild(overlay);
    const cleanup = (val) => {
      overlay.remove();
      document.removeEventListener("keydown", onKey);
      resolve(val);
    };
    const onKey = (e) => {
      if (e.key === "Escape") cleanup(false);
      else if (e.key === "Enter") cleanup(true);
    };
    overlay.querySelector(".modalOk").addEventListener("click", () => cleanup(true));
    overlay.querySelector(".modalCancel").addEventListener("click", () => cleanup(false));
    overlay.addEventListener("click", (e) => { if (e.target === overlay) cleanup(false); });
    document.addEventListener("keydown", onKey);
  });
}

// ---------------- 窗口状态记忆（位置/大小存 localStorage，重启恢复） ----------------
const WIN_KEY = "dshLauncher.windowState";
let winStateTimer = null;
async function saveWindowState() {
  try {
    // 启动初期（WebView2 未布局完成）innerSize 可能返回异常值（如屏幕尺寸），
    // 3 秒内不保存，避免把错误尺寸写入状态
    if (Date.now() - bootTime < 3000) return;
    const win = window.__TAURI__.window.getCurrentWindow();
    if (await win.isMaximized()) return; // 最大化时不覆盖已保存的正常状态
    const sf = await win.scaleFactor();
    const size = await win.innerSize();
    // 用 outerPosition（外框位置）：setPosition 设置的也是外框，二者必须一致；
    // 若用 innerPosition（客户区）恢复，窗口每次重启都会上漂一个标题栏高度
    const pos = await win.outerPosition();
    // 统一存逻辑像素（除以缩放比），恢复时用 LogicalSize/LogicalPosition，
    // 高 DPI 屏上尺寸/位置才不会逐次缩水或漂移
    localStorage.setItem(WIN_KEY, JSON.stringify({
      w: Math.round(size.width / sf),
      h: Math.round(size.height / sf),
      x: Math.round(pos.x / sf),
      y: Math.round(pos.y / sf),
    }));
  } catch (e) { /* ignore */ }
}
async function restoreWindowState() {
  try {
    const raw = localStorage.getItem(WIN_KEY);
    if (!raw) return;
    const s = JSON.parse(raw);
    if (!(s.w >= 640 && s.h >= 480 && s.w <= 4000 && s.h <= 3000)) return; // 合理性校验
    // 必须传 LogicalSize/LogicalPosition 实例：裸 {width,height} 会被序列化成
    // {"undefined":{...}}，后端 dpi::Size 反序列化失败导致恢复静默失效
    const W = window.__TAURI__.window;
    const win = W.getCurrentWindow();
    await win.setSize(new W.LogicalSize(s.w, s.h));
    if (typeof s.x === "number" && typeof s.y === "number") {
      // 可见性校验：恢复位置须与任一显示器工作区相交，否则放弃位置保持居中，
      // 防止副屏拔掉后窗口恢复到屏幕外「丢失」
      let visible = true;
      try {
        const monitors = await W.availableMonitors();
        visible = monitors.some((m) => {
          const sf = m.scaleFactor || 1;
          const mx = m.workArea.position.x / sf, my = m.workArea.position.y / sf;
          const mw = m.workArea.size.width / sf, mh = m.workArea.size.height / sf;
          return s.x < mx + mw && s.x + s.w > mx && s.y < my + mh && s.y + s.h > my;
        });
      } catch (e) { visible = true; }
      if (visible) await win.setPosition(new W.LogicalPosition(s.x, s.y));
    }
  } catch (e) { /* ignore */ }
}
restoreWindowState();
const curWin = window.__TAURI__.window.getCurrentWindow();
curWin.onResized(() => {
  clearTimeout(winStateTimer);
  winStateTimer = setTimeout(saveWindowState, 300);
}).catch(() => {});
curWin.onMoved(() => {
  clearTimeout(winStateTimer);
  winStateTimer = setTimeout(saveWindowState, 300);
}).catch(() => {});

// 关闭窗口 = 隐藏到后台（托盘常驻）或直接退出（设置页 closeAction 配置）。
// 用前端 onCloseRequested（事件插件通道），避免 Rust 侧窗口事件注册的时序竞态。
// 注意：窗口创建初期 WebView2 可能误发一次 close-requested（会导致窗口刚启动
// 就被隐藏），因此启动后 3 秒内只阻止关闭、不执行隐藏。
const BOOT_PROTECT_MS = 3000;
const bootTime = Date.now();
let closeAction = "tray"; // 由 get_settings 初始化；quit = 放行关闭退出
curWin.onCloseRequested(async (event) => {
  // 「直接退出」模式：不阻止，让默认关闭流程销毁窗口并退出进程
  if (closeAction === "quit") return;
  event.preventDefault();
  if (Date.now() - bootTime < BOOT_PROTECT_MS) return;
  try {
    // 关键：外部 ShowWindow 恢复的窗口会使 tao 内部可见性 flags 与实际状态
    // 不同步，直接 hide() 会被判定为"无变化"而空操作；先 show() 同步 flags
    // 再 hide() 才能可靠隐藏。
    await curWin.show();
    await curWin.hide();
  } catch (e) { /* ignore */ }
}).catch(() => {});
// 固定窗口标题：tauri 会把 document.title 同步为窗口标题，若为空则单实例
// 的 FindWindow("DSH 启动器") 无法找到窗口、恢复显示会失效；同步可能晚于
// 启动或覆盖手动设置，因此定时守护标题。
curWin.setTitle("DSH 启动器").catch(() => {});
// 仅在启动竞态窗口内补设几次即停止（WebView2 加载完成后可能用 document.title
// 覆盖窗口标题）；不做常驻定时器，避免隐藏到托盘后仍每 3 秒产生 IPC 调用。
for (const delay of [1500, 4000, 8000]) {
  setTimeout(() => { curWin.setTitle("DSH 启动器").catch(() => {}); }, delay);
}

// 自定义标题栏按钮：最小化直接最小化；关闭走 close() 触发上方
// onCloseRequested 统一处理（tray = 隐藏到托盘 / quit = 放行退出）
$("tbMin").addEventListener("click", () => { curWin.minimize().catch(() => {}); });
$("tbClose").addEventListener("click", () => { curWin.close().catch(() => {}); });

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
// 关键操作失败：活动栏显示截断摘要，完整错误展开到日志面板
function showError(title, e) {
  setActivity(title + "：" + cleanMsg(e));
  logPanel.title(title);
  logPanel.clear();
  logPanel.append(String(e));
  logPanel.open();
}
// 字节数格式化：1024 → "1.0 KB"，1.5MB → "1.5 MB"
function formatBytes(n) {
  if (!n && n !== 0) return "";
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
  return (n / 1024 / 1024).toFixed(1) + " MB";
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
function escapeHtml(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/\n/g, "<br>");
}
function escapeAttr(s) {
  return String(s).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
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
// 偶发 IPC 失败保持上次状态（避免状态胶囊误闪「未运行」），连续 3 次失败才显示未知；
// 窗口隐藏（托盘）时胶囊不可见，降为 10 秒低频轮询，恢复显示时回到 2 秒
let pollFailures = 0;
async function pollStatus() {
  try {
    const running = await invoke("status");
    pollFailures = 0;
    setPill(running ? "DSH 正在运行" : "DSH 未运行", running);
  } catch (e) {
    pollFailures += 1;
    if (pollFailures >= 3) setPill("状态检测异常", "unknown");
  }
  setTimeout(pollStatus, document.hidden ? 10000 : 2000);
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
  } catch (e) { showError("启动失败", e); return false; }
}
// 「启动 Web 界面」：启动（已在运行则跳过）后按设置决定是否自动打开浏览器
$("webBtn").addEventListener("click", async () => {
  if (!(await startWeb(false))) return;
  if (cfg.autoOpenBrowser !== false) {
    setActivity("DSH 已就绪，即将打开浏览器…");
    setTimeout(() => invoke("open_browser"), 1500);
  } else {
    setActivity("DSH 已就绪：" + webUrl);
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
$("tuiBtn").addEventListener("click", async () => {
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
  if (list.length === 0) { box.innerHTML = '<span class="sub">🐋 暂无历史任务，执行过的会显示在这里，点击可重跑</span>'; return; }
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
  if (!(await uiConfirm({ title: "重启 DSH", message: "将终止当前 DSH（" + webUrl + "）并重新启动。\n当前 Web 界面（包括正在进行的会话）会中断，重启后恢复。", okText: "重启" }))) return;
  btn.disabled = true; btn.textContent = "重启中…";
  setActivity("正在重启 DSH…");
  try {
    const r = await invoke("restart_dsh", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    if (r === "ok") setActivity("DSH 已重启完成（Web 模式后台运行中）");
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
    const [enabled, server] = await invoke("get_system_proxy_cmd");
    if (enabled && server) {
      cfg.proxyAddr = server; $("setProxyInput").value = server;
      setActivity("已同步系统代理 " + server + "（与浏览器一致）");
    } else {
      setActivity("系统代理未启用，使用下方手动填写的地址");
    }
    setActivity("已启用代理；启动 DSH 时将注入 HTTP(S)_PROXY 与 NODE_USE_ENV_PROXY");
  } else {
    setActivity("已关闭代理；DSH 将直连网络");
  }
  saveCfg();
});
$("setProxyInput").addEventListener("change", () => { cfg.proxyAddr = $("setProxyInput").value.trim(); saveCfg(); });
$("setProxyImport").addEventListener("click", async () => {
  const [enabled, server] = await invoke("get_system_proxy_cmd");
  if (!enabled || !server) { setActivity("Windows 系统代理未启用"); return; }
  $("setProxyInput").value = server;
  cfg.proxyEnabled = true; cfg.proxyAddr = server;
  $("setProxySwitch").checked = true;
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
async function loadSettings() {
  try {
    const s = await invoke("get_settings");
    settingsState = s;
    // 窗口行为单选
    $("setCloseSeg").querySelectorAll(".segBtn").forEach((b) => {
      b.classList.toggle("on", b.dataset.val === s.closeAction);
    });
    // profile 下拉（保留当前选中，若列表中不存在则补一项）
    const profiles = await invoke("list_profiles");
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
    setActivity("设置加载失败：" + cleanMsg(e));
  }
  // 开机自启
  try { $("setAutoStart").checked = await invoke("get_autostart"); } catch (e) { /* ignore */ }
  // localStorage 配置
  $("setAutoCheck").checked = cfg.autoCheckUpdate !== false;
  $("setAutoBrowser").checked = cfg.autoOpenBrowser !== false;
  $("setHistoryInfo").textContent = "当前 " + cfg.history.length + " 条";
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
    updates = null; // 插件视图的更新缓存作废
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
// 清空 Headless 历史
$("setClearHistory").addEventListener("click", async () => {
  if (!(await uiConfirm({ title: "清空历史", message: "确定清空全部 Headless 历史记录？此操作不可恢复。", okText: "清空", danger: true }))) return;
  cfg.history = [];
  saveCfg();
  renderHistory();
  $("setHistoryInfo").textContent = "已清空";
  setActivity("Headless 历史已清空");
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
async function loadBackups() {
  const box = $("backupList");
  try {
    const list = await invoke("list_backups");
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
// 更新检查结果缓存：{ [name]: { installed, latest, error } }；null = 尚未检查
let updates = null;
function setPluginBusy(b) {
  pluginBusy = b;
  $("pluginList").classList.toggle("busy", b);
  $("pluginInstallBtn").disabled = b;
  $("pluginCheckBtn").disabled = b;
  $("pluginUpdateAllBtn").disabled = b;
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
  box.classList.remove("busy");
  $("profilePath").textContent = res.profileDir;
  if (!res.initialized) {
    box.innerHTML = '<div class="pluginEmpty"><div class="emptyIcon">🧩</div><div class="emptyTitle">web profile 尚未初始化</div><div class="sub">安装第一个插件时会自动创建（' + escapeHtml(res.profileDir) + '）</div></div>';
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
  // 键盘可达性：整行可聚焦，Enter/Space 展开/收起
  row.tabIndex = 0;
  row.setAttribute("role", "button");
  row.setAttribute("aria-expanded", "false");
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
  row.setAttribute("aria-expanded", String(expanded));
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
  try {
    const res = await invoke("plugin_list");
    renderPlugins(res);
  } catch (e) {
    const box = $("pluginList");
    box.classList.remove("busy");
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
    await loadPlugins();
  } catch (e) { showError("安装失败", e); }
  finally { setPluginBusy(false); }
}
async function removePlugin(name) {
  if (pluginBusy) return;
  if (!(await uiConfirm({ title: "卸载插件", message: "卸载插件 " + name + "？\n将执行 dsh plugin --profile web remove " + name + "：\n从 profile 移除依赖并停用该插件。", okText: "卸载", danger: true }))) return;
  setPluginBusy(true);
  try {
    await invoke("plugin_remove", { name, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress: pluginProgress("正在卸载 " + name) });
    setActivity("插件 " + name + " 已卸载，重启 DSH 后生效");
    await loadPlugins();
  } catch (e) { showError("卸载失败", e); }
  finally { setPluginBusy(false); }
}
// 检查所有用户插件的 registry 最新版本（后端并行 npm view），
// 结果缓存到 updates 并在渲染时决定是否显示「更新」按钮；
// 「全部更新」按钮仅在发现有可更新插件后出现（带数量，紫色区别于行内按钮）。
async function checkUpdates() {
  if (pluginBusy) return;
  setPluginBusy(true);
  setActivity("正在检查插件更新…");
  try {
    const res = await invoke("plugin_check_updates", { proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr });
    updates = {};
    let updatable = 0, failed = 0;
    for (const u of res) {
      updates[u.name] = u;
      if (u.latest && cmpVer(u.latest, u.installed) > 0) updatable++;
      else if (u.error) failed++;
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
      setActivity("所有插件已是最新（" + failed + " 个插件版本检查失败）");
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
    updates = null;
    await loadPlugins();
    await checkUpdates();
  } catch (e) { showError("更新失败", e); }
  finally { setPluginBusy(false); }
}
async function updateAllPlugins() {
  if (pluginBusy) return;
  if (!(await uiConfirm({ title: "全部更新", message: "将所有已安装插件更新到最新版本？\n将执行 pnpm update --latest（在 web profile 目录）。", okText: "全部更新" }))) return;
  setPluginBusy(true);
  try {
    await invoke("plugin_update", { all: true, name: null, proxyOn: cfg.proxyEnabled, proxyAddr: cfg.proxyAddr, progress: pluginProgress("正在更新全部插件") });
    setActivity("全部插件更新完成，重启 DSH 后生效");
    updates = null;
    await loadPlugins();
    await checkUpdates();
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
// 键盘：聚焦插件行时 Enter / Space 展开收起
$("pluginList").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" && e.key !== " ") return;
  const row = e.target.closest(".pluginRow");
  if (!row || pluginBusy) return;
  e.preventDefault();
  toggleDetail(row);
});

// ---------------- 初始化 ----------------
applyTheme();
renderHistory();
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
