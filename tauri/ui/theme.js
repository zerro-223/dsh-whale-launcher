// 首帧前定主题：读取 localStorage 里的主题配置写入 data-theme，
// 避免深色用户启动瞬间先闪一帧浅色（main.js 在 body 末尾才执行）。
// 独立文件在 head 中同步加载（渲染阻塞，效果等同内联脚本），
// 以便启用严格 CSP（script-src 'self'，禁止内联脚本）。
(function () {
  try {
    var c = JSON.parse(localStorage.getItem("dshLauncher.v1") || "{}");
    var t = c.theme || "system";
    if (t === "system") t = window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
    document.documentElement.dataset.theme = t;
  } catch (e) { /* 默认浅色变量兜底 */ }
})();
