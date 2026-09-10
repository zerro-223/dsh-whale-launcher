// 前端"元素契约"测试：index.html 与 JS 里的 id 必须一一对得上。
//
// 为什么需要它：前端无构建链，`$("x")` 取不到元素不会报错，只会让对应功能
// 静默失效（绑定处用 ?. 兜底后连异常都没有）。id 漂移是纯人为事故，靠肉眼
// 审查不可靠，这里把它变成 CI 上的硬约束。
//
// 运行：node --test tauri/ui/test/
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const readUi = (f) => readFileSync(join(here, "..", f), "utf8");

const html = readUi("index.html");
// `\sid="` 而非 `id="`：避免命中 data-id 之类以 id= 结尾的属性
const htmlIdList = [...html.matchAll(/\sid="([^"]+)"/g)].map((m) => m[1]);
const htmlIds = new Set(htmlIdList);

const JS_FILES = ["core.js", "chrome.js", "main.js"];
const jsSources = JS_FILES.map((f) => [f, readUi(f)]);

const REF_RE = /\$\("([^"]+)"\)/g;

/// 剥离注释后的代码文本。
/// 不做完整 JS 词法分析：这些文件没有跨行块注释，且行尾注释不含 $("…")
/// 形态（由下面的"行尾注释"用例守住），因此"整行注释过滤 + 行内块注释剥离"
/// 已足够，且规则简单可验证。
function stripComments(src) {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, " ")
    .split("\n")
    .filter((line) => !line.trim().startsWith("//"))
    .join("\n");
}

/// 收集 JS 中 $("id") 引用的 id（当前全部为字符串字面量）
function referencedIds(src) {
  return [...stripComments(src).matchAll(REF_RE)].map((m) => m[1]);
}

/// 从 core.js 源码中取出 REQUIRED_IDS 数组里的字符串字面量
function requiredIds() {
  const core = readUi("core.js");
  const block = core.match(/const REQUIRED_IDS = \[([\s\S]*?)\n\];/);
  assert.ok(block, "未能从 core.js 解析出 REQUIRED_IDS 数组（格式变了？）");
  const ids = [...stripComments(block[1]).matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  assert.ok(ids.length > 50, `REQUIRED_IDS 解析结果仅 ${ids.length} 项，可能解析失败`);
  return ids;
}

test("index.html 中不存在重复 id", () => {
  const dup = [...new Set(htmlIdList.filter((id, i) => htmlIdList.indexOf(id) !== i))];
  assert.deepEqual(dup, [], "重复 id 会让 getElementById 取到非预期元素");
});

test("JS 里 $() 引用的 id 都存在于 index.html", () => {
  const missing = [];
  for (const [file, src] of jsSources) {
    for (const id of referencedIds(src)) {
      if (!htmlIds.has(id)) missing.push(`${file}: $("${id}")`);
    }
  }
  assert.deepEqual(missing, [], "以下引用在 index.html 中不存在，对应功能会静默失效");
});

test("REQUIRED_IDS 清单里的 id 都存在于 index.html", () => {
  const missing = requiredIds().filter((id) => !htmlIds.has(id));
  assert.deepEqual(missing, [], "启动自检会为这些不存在的 id 报错，属笔误");
});

test("JS 引用的 id 都已纳入 REQUIRED_IDS（自检不得漏项）", () => {
  const required = new Set(requiredIds());
  const omitted = [];
  for (const [file, src] of jsSources) {
    for (const id of new Set(referencedIds(src))) {
      if (!required.has(id)) omitted.push(`${file}: ${id}`);
    }
  }
  assert.deepEqual(omitted, [], "这些 id 被 JS 引用但不在自检清单中，缺失时不会被报出来");
});

test("REQUIRED_IDS 无重复项", () => {
  const ids = requiredIds();
  const dup = [...new Set(ids.filter((id, i) => ids.indexOf(id) !== i))];
  assert.deepEqual(dup, [], "清单存在重复项");
});

test("$() 的参数全部是字符串字面量（契约校验的前提）", () => {
  const bad = [];
  for (const [file, src] of jsSources) {
    for (const m of stripComments(src).matchAll(/\$\(([^)]*)\)/g)) {
      const arg = m[1].trim();
      if (arg === "id") continue; // core.js 内 const $ = (id) => ...
      if (!/^"[^"]*"$/.test(arg)) bad.push(`${file}: $(${arg})`);
    }
  }
  assert.deepEqual(
    bad,
    [],
    "出现动态 id 取值：契约测试只覆盖字面量，请改为显式字面量或扩展本测试"
  );
});

test('行尾注释中不得出现 $("…") 形态（否则解析会把注释当代码）', () => {
  // 注意用非全局正则：/g 的 lastIndex 会在多次 test() 间残留
  const refTest = /\$\("[^"]+"\)/;
  const bad = [];
  for (const [file, src] of jsSources) {
    src.split("\n").forEach((line, i) => {
      if (line.trim().startsWith("//")) return; // 整行注释已由 stripComments 过滤
      const idx = line.indexOf("//");
      if (idx >= 0 && refTest.test(line.slice(idx))) bad.push(`${file}:${i + 1}`);
    });
  }
  assert.deepEqual(
    bad,
    [],
    '行尾注释里出现 $("…")：请改写注释，或扩展本测试的注释剥离逻辑'
  );
});
