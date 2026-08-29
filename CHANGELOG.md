# 更新日志

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 SemVer。历史版本说明见各次 Release。

## [1.3.0] - 2026-08-29

### 重构

- Rust 后端按职责拆分为模块（`settings` / `locate` / `npm` / `proxy` /
  `process` / `backup` / `plugins` / `launch` / `checks` / `update` /
  `tray` / `util`），`lib.rs` 只保留模块声明与应用入口
- 前端拆出纯函数工具层 `util.js`（转义 / formatBytes / cmpVer），
  新增 Node 内置 runner 的单元测试（`tauri/ui/test/`）

### 新增

- CI：新增 Release 工作流（tag 触发构建并上传 exe）与 cargo-audit
  依赖安全审计；前端 job 增加 `node --test`
- `update_check` 命令返回命名结构体（installed / latest / error），
  取代前端按位置解构的匿名三元组
- 启动操作互斥：`start_web` / `start_tui` / `start_headless` /
  `restart_dsh` 共用一把锁，与备份/恢复互斥同款实现（RAII guard）

### 修复

- 双击「启动 Web 界面 / 启动 TUI 终端」会并发拉起两个 DSH 进程、第二个
  绑定端口失败弹假错误：前端按钮 busy 防抖 + 后端启动操作互斥
- `run_npm` 轮询等待期间不排空子进程管道，输出超过管道缓冲会阻塞子进程
  导致假超时（与 `npm_view_version` 对齐）
- 进程身份核验改用大小写不敏感的精确子串匹配：原先 `-like '*bin*'` 会把
  路径中的 `[ ] * ?` 当通配符，安装路径含这些字符时 restart/stop 失效
- 解压备份按实际写入字节强制 8GB 上限（条目声明大小可伪造）
- spawn_web 启动慢（8 秒窗口内端口未就绪但进程存活）不再按失败报错，
  返回 `starting` 交由状态轮询确认，且不触发自动打开浏览器
- 设置页代理开关的提示被第二条 setActivity 覆盖：合并为一条
- 备份列表 / 设置页加载增加请求竞态防护（对齐插件列表的模式）
- `escapeAttr` 补单引号与 `>` 转义；修正「延迟 500ms 注册」过时注释
- 备份清单改用 serde_json 生成，删除手写 `json_escape`

### 变更

- `.gitignore` 增加 `backups/` 与 `web.log`（备份含 `.credentials.yaml`
  凭据，exe 放仓库根目录测试时防止误提交）
- `crate-type` 精简为 `rlib`（桌面应用不需要 staticlib/cdylib）
- localStorage key（`dshLauncher.v1`）单一来源化到 theme.js

## [1.2.0] - 2026-08-22

- 自定义标题栏（decorations 关闭 + 前端拖拽区）
- 严格 CSP（`script-src 'self'`，无内联脚本）
- 备份/恢复改用 zip crate（替代 PowerShell ZipArchive），修复恢复流程并
  新增 ZipSlip 防护；多处性能优化（时间戳 Win32 化、日志尾部 seek 读取）

## [1.1.0] - 2026-08-15

- DSH 安装位置自动识别（npm 全局 / npx 缓存 / 显式配置），不再要求
  "D 盘 + junction" 布局
- 深海风格界面改版；启动 Web 界面自动打开浏览器、新增关闭 Web 界面
