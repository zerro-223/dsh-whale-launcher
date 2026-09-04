// 通用工具与展示基础（无业务依赖，先于 main.js 加载）。
// 挂到 window 上作为轻量命名空间（无打包链，跨文件共享的约定是
// DOM 偏好存 localStorage / 子进程行为存 settings.json，见 main.js 头注）。
// 纯函数（转义 / formatBytes / cmpVer）在 util.js 中定义，本文件只做导出。

// ---------------- Tauri IPC ----------------
// 唯一声明处：chrome.js / main.js 直接使用，避免跨脚本
// 顶层 const 重复声明导致整段脚本解析失败
const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

// ---------------- 配置（localStorage） ----------------
// key 的单一来源在 theme.js（它必须最先执行以避免闪屏），此处只读取
const CFG_KEY = window.DSH_CFG_KEY;
const CFG_DEFAULTS = { theme: "system", colorTheme: "teal", proxyEnabled: false, proxyAddr: "http://127.0.0.1:7890", autoCheckUpdate: true, autoOpenBrowser: true };
function loadCfg() {
  try {
    const raw = localStorage.getItem(CFG_KEY);
    if (raw) return Object.assign({}, CFG_DEFAULTS, JSON.parse(raw));
  } catch (e) { /* 损坏即回默认 */ }
  return Object.assign({}, CFG_DEFAULTS);
}
function saveCfg() {
  localStorage.setItem(CFG_KEY, JSON.stringify(window.cfg));
}

// ---------------- 主题（深色 / 浅色 / 跟随系统 三态循环） ----------------
const THEME_MODES = ["dark", "light", "system"];
function resolveTheme() {
  if (window.cfg.theme === "system") {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  return window.cfg.theme;
}
function themeLabel(mode) {
  return mode === "system" ? "跟随系统" : mode === "dark" ? "深色" : "浅色";
}
function applyTheme() {
  document.documentElement.dataset.theme = resolveTheme();
  const themeBtn = $("themeBtn");
  themeBtn.textContent = window.cfg.theme === "system" ? "跟随系统" : (window.cfg.theme === "dark" ? "深色模式" : "浅色模式");
  themeBtn.title = "点击切换主题（深色 → 浅色 → 跟随系统）";
}
function initTheme() {
  $("themeBtn").addEventListener("click", () => {
    window.cfg.theme = THEME_MODES[(THEME_MODES.indexOf(window.cfg.theme) + 1) % THEME_MODES.length];
    applyTheme(); saveCfg();
    window.setActivity("已切换为" + themeLabel(window.cfg.theme) + "模式");
  });
  // 系统主题变化时，跟随模式即时刷新
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => {
    if (window.cfg.theme === "system") applyTheme();
  });
}

// ---------------- 配色主题（强调色；与深浅模式正交） ----------------
const ACCENTS = ["teal", "blue", "violet", "amber", "rose"];
function applyAccent() {
  const a = ACCENTS.includes(window.cfg.colorTheme) ? window.cfg.colorTheme : "teal";
  document.documentElement.dataset.accent = a;
  document.querySelectorAll("#accentSwatches .swatch").forEach((b) => {
    b.classList.toggle("active", b.dataset.accent === a);
  });
}
function initAccent() {
  document.querySelectorAll("#accentSwatches .swatch").forEach((b) => {
    b.addEventListener("click", () => {
      if (window.cfg.colorTheme === b.dataset.accent) return;
      window.cfg.colorTheme = b.dataset.accent;
      applyAccent();
      saveCfg();
      window.setActivity("已切换配色主题：" + b.title);
    });
  });
}

// ---------------- 自定义确认对话框 ----------------
// 返回 Promise<boolean>；支持 danger 红色确认按钮
function uiConfirm({ title, message, okText = "确定", cancelText = "取消", danger = false }) {
  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modalOverlay";
    overlay.innerHTML = `
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modalTitle">${escapeHtml(title)}</div>
        <div class="modalMsg">${escapeMultiline(message)}</div>
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

// 导出到 window（无模块系统下的跨文件契约，main.js / chrome.js 按名取用）
Object.assign(window, {
  invoke,
  escapeHtml, escapeAttr, escapeMultiline, $, formatBytes, cmpVer,
  loadCfg, saveCfg, CFG_KEY, resolveTheme, themeLabel, applyTheme, initTheme,
  ACCENTS, applyAccent, initAccent, uiConfirm,
});
