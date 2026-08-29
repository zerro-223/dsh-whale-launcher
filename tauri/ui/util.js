// 纯函数工具层：不依赖 window / document / Tauri，可被 Node 直接加载做
// 单元测试（tauri/ui/test/util.test.mjs 用 new Function 包装执行）。
// 经典脚本加载后函数声明自动挂到 window，core.js 负责在导出清单中显式列出。

// ---------------- DOM / 转义 ----------------
// escapeHtml：用于文本插值（innerHTML 中的内容片段）；
// escapeAttr：用于属性插值（title/data-*），额外转义引号（含单引号，
// 防未来有人用单引号包属性时被逃逸）；
// escapeMultiline：文本 + 换行转 <br>（多行错误信息）。
// 三者各司其职，避免"一个函数顺带处理换行"被误用于属性位置。
function escapeHtml(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
function escapeAttr(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}
function escapeMultiline(s) {
  return escapeHtml(s).replace(/\n/g, "<br>");
}

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
