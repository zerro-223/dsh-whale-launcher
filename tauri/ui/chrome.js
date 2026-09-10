// 窗口外观层：窗口状态记忆、关闭行为、自定义标题栏、日志面板与活动栏。
// 依赖 core.js（$ / invoke / loadCfg / saveCfg），先于 main.js 加载。

// ---------------- 窗口状态记忆（位置/大小存 localStorage，重启恢复） ----------------
const WIN_KEY = "dshLauncher.windowState";
let winStateTimer = null;
const bootTime = Date.now();
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
    // 合理性校验（略宽于窗口 minWidth/minHeight，陈旧的过小值直接放弃）
    if (!(s.w >= 500 && s.h >= 600 && s.w <= 4000 && s.h <= 3000)) return;
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
let closeAction = "tray"; // 由 main.js 初始化；quit = 放行关闭退出
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
// 启动或覆盖手动设置，启动竞态窗口内补设几次即停止。
curWin.setTitle("DSH 启动器").catch(() => {});
for (const delay of [1500, 4000, 8000]) {
  setTimeout(() => { curWin.setTitle("DSH 启动器").catch(() => {}); }, delay);
}

// 自定义标题栏按钮：最小化直接最小化；关闭走 close() 触发上方
// onCloseRequested 统一处理（tray = 隐藏到托盘 / quit = 放行退出）
$("tbMin")?.addEventListener("click", () => { curWin.minimize().catch(() => {}); });
$("tbClose")?.addEventListener("click", () => { curWin.close().catch(() => {}); });

// ---------------- 活动栏与全局日志面板 ----------------
function setActivity(msg) { setEl("activity", "textContent", msg); }
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

// 全局日志面板：npm/pnpm 完整输出流式显示，或展示 web.log 内容
// 追加走 rAF 攒帧批量渲染：pnpm/npm 高频逐行输出时不再每行触发一次 DOM 写
// 所有 DOM 访问经 setEl 守卫：本对象在 catch 分支（showError）里也会被调用，
// 自身抛异常会把原始错误一起吞掉。
const logPanel = {
  lines: 0,
  pending: [],
  rafId: 0,
  title(t) { setEl("logTitle", "textContent", t); },
  open() { setEl("logPanel", "hidden", false); },
  close() { setEl("logPanel", "hidden", true); },
  clear() {
    this.pending.length = 0;
    if (this.rafId) { cancelAnimationFrame(this.rafId); this.rafId = 0; }
    setEl("logBody", "textContent", "");
    this.lines = 0;
  },
  append(s) {
    this.pending.push(s);
    if (this.rafId) return;
    this.rafId = requestAnimationFrame(() => {
      this.rafId = 0;
      const el = $("logBody");
      const batch = this.pending.splice(0, this.pending.length);
      if (!el || !batch.length) return;
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
$("logClose")?.addEventListener("click", () => logPanel.close());
$("logLink")?.addEventListener("click", async () => {
  try {
    const [text, exists] = await invoke("read_web_log");
    if (!exists) { logPanel.show("web.log", "web.log 不存在（DSH Web 模式尚未启动过）"); return; }
    logPanel.show("web.log", text.trim() ? text : "（web.log 为空）");
  } catch (e) { logPanel.show("web.log", "读取失败：" + String(e)); }
});
// 启动器自身日志：npm / pnpm / dsh plugin 的完整输出落盘于此，
// 更新或插件操作失败后这是唯一能事后复查原始报错的地方
$("launcherLogLink")?.addEventListener("click", async () => {
  try {
    const [text, exists] = await invoke("read_launcher_log");
    if (!exists) {
      logPanel.show("launcher.log", "launcher.log 不存在（尚未执行过安装/更新/插件操作）");
      return;
    }
    logPanel.show("launcher.log", text.trim() ? text : "（launcher.log 为空）");
  } catch (e) { logPanel.show("launcher.log", "读取失败：" + String(e)); }
});

// ---------------- 页面可见性（极光动画暂停，见 style.css 同名规则） ----------------
// 托盘化/最小化时暂停背景装饰动画，避免后台常驻期间 GPU/CPU 持续重绘
document.addEventListener("visibilitychange", () => {
  document.documentElement.classList.toggle("page-hidden", document.hidden);
});

Object.assign(window, {
  curWin, bootTime, setActivity, cleanMsg, showError, logPanel,
  get closeAction() { return closeAction; },
  set closeAction(v) { closeAction = v; },
});
