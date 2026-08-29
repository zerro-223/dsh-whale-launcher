// util.js 纯函数工具层的单元测试（Node 内置 test runner，无第三方依赖）。
// 运行：node --test tauri/ui/test/
// util.js 是经典浏览器脚本（无模块导出），这里读源码用 new Function
// 包装执行后取出函数——它不依赖 window/document，因此可以在 Node 中安全求值。
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(join(here, "..", "util.js"), "utf8");
const util = new Function(
  src + "\nreturn { escapeHtml, escapeAttr, escapeMultiline, formatBytes, cmpVer };"
)();

test("escapeHtml 转义文本内容中的 HTML 特殊字符", () => {
  assert.equal(util.escapeHtml('<script>alert("x")</script>'), "&lt;script&gt;alert(\"x\")&lt;/script&gt;");
  assert.equal(util.escapeHtml("a&b"), "a&amp;b");
  assert.equal(util.escapeHtml(123), "123");
});

test("escapeAttr 转义双引号与单引号（属性插值安全）", () => {
  assert.equal(util.escapeAttr('"><script>'), "&quot;&gt;&lt;script&gt;");
  assert.equal(util.escapeAttr("it's"), "it&#39;s");
  assert.equal(util.escapeAttr("a&b"), "a&amp;b");
});

test("escapeMultiline 换行转 <br>", () => {
  assert.equal(util.escapeMultiline("a\nb"), "a<br>b");
  assert.equal(util.escapeMultiline("<b>\n"), "&lt;b&gt;<br>");
});

test("formatBytes 各量级格式化", () => {
  assert.equal(util.formatBytes(0), "0 B");
  assert.equal(util.formatBytes(1023), "1023 B");
  assert.equal(util.formatBytes(1024), "1.0 KB");
  assert.equal(util.formatBytes(1.5 * 1024 * 1024), "1.5 MB");
  assert.equal(util.formatBytes(1.2 * 1024 ** 3), "1.2 GB");
  assert.equal(util.formatBytes(undefined), "");
});

test("cmpVer 数字位比较不按字符串", () => {
  assert.equal(util.cmpVer("1.10.0", "1.9.9"), 1);
  assert.equal(util.cmpVer("0.12.2", "0.12.2"), 0);
  assert.equal(util.cmpVer("1.2.3", "1.2.4"), -1);
});

test("cmpVer 预发布版本低于正式版", () => {
  assert.equal(util.cmpVer("1.0.0-rc.1", "1.0.0"), -1);
  assert.equal(util.cmpVer("1.0.0", "1.0.0-alpha"), 1);
});

test("cmpVer 预发布标识按数字而非字典序", () => {
  assert.equal(util.cmpVer("1.0.0-2", "1.0.0-10"), -1);
  assert.equal(util.cmpVer("1.0.0-alpha", "1.0.0-beta"), -1);
  assert.equal(util.cmpVer("1.0.0-alpha.1", "1.0.0-alpha"), 1);
});

test("cmpVer build metadata 不参与优先级", () => {
  assert.equal(util.cmpVer("1.0.0+build.1", "1.0.0+build.2"), 0);
  assert.equal(util.cmpVer("1.0.0+build", "1.0.0"), 0);
});

test("cmpVer 无效版本走稳定的字符串兜底", () => {
  assert.equal(util.cmpVer("abc", "abc"), 0);
  assert.equal(util.cmpVer("abc", "abd"), -1);
  assert.equal(util.cmpVer("", ""), 0);
});
