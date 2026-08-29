// 通用工具与展示基础（无业务依赖，先于 main.js 加载）。
// 挂到 window 上作为轻量命名空间（无打包链，跨文件共享的约定是
// DOM 偏好存 localStorage / 子进程行为存 settings.json，见 main.js 头注）。

// ---------------- DOM / 转义 ----------------
// escapeHtml：用于文本插值（innerHTML 中的内容片段）；
// escapeAttr：用于属性插值（title/data-*），额外转义引号；
// escapeMultiline：文本 + 换行转 <br>（多行错误信息）。
// 三者各司其职，避免"一个函数顺带处理换行"被误用于属性位置。
// Tauri IPC（唯一声明处：chrome.js / main.js 直接使用，避免跨脚本
// 顶层 const 重复声明导致整段脚本解析失败）
const { invoke } = window.__TAURI__.core;

function escapeHtml(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
function escapeAttr(s) {
  return String(s).replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}
function escapeMultiline(s) {
  return escapeHtml(s).replace(/\n/g, "<br>");
}
const $ = (id) => document.getElementById(id);

// ---------------- 字节数格式化 ----------------
// 1024 → "1.0 KB"，1.5MB → "1.5 MB"，1.2GB → "1.2 GB"
function formatBytes(n) {
  if (!n && n !== 0) return "";
  if (n < 1024) return n + " B";
  if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
  if (n < 1024 * 1024 * 1024) return (n / 1024 / 1024).toFixed(1) + " MB";
  return (n / 1024 / 1024 / 1024).toFixed(1) + " GB";
}

// ---------------- 版本比较 ----------------
// 处理 npm 常见的 SemVer：预发布标识按数字/字符串规则比较，build
// metadata 不参与优先级；无效版本保留稳定的字符串兜底顺序。
function cmpVer(a, b) {
  const parse = (value) => {
    const s = String(value).trim();
    const m = s.match(/^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/);
    if (!m) return null;
    return { nums: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ? m[4].split(".") : null };
  };
  const pa = parse(a), pb = parse(b);
  if (!pa || !pb) {
    const sa = String(a), sb = String(b);
    return sa === sb ? 0 : (sa > sb ? 1 : -1);
  }
  for (let i = 0; i < 3; i++) {
    if (pa.nums[i] !== pb.nums[i]) return pa.nums[i] > pb.nums[i] ? 1 : -1;
  }
  if (!pa.pre && !pb.pre) return 0;
  if (!pa.pre) return 1;
  if (!pb.pre) return -1;
  for (let i = 0; i < Math.max(pa.pre.length, pb.pre.length); i++) {
    if (i >= pa.pre.length) return -1;
    if (i >= pb.pre.length) return 1;
    const xa = pa.pre[i], xb = pb.pre[i];
    if (xa === xb) continue;
    const na = /^\d+$/.test(xa), nb = /^\d+$/.test(xb);
    if (na && nb) return Number(xa) > Number(xb) ? 1 : -1;
    if (na !== nb) return na ? -1 : 1;
    return xa > xb ? 1 : -1;
  }
  return 0;
}

// ---------------- 配置（localStorage） ----------------
const CFG_KEY = "dshLauncher.v1";
const CFG_DEFAULTS = { theme: "system", colorTheme: "teal", proxyEnabled: false, proxyAddr: "http://127.0.0.1:7890", history: [], autoCheckUpdate: true, autoOpenBrowser: true };
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
