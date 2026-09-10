# 更新日志

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 SemVer。历史版本说明见各次 Release。

## [未发布]

### 修复（用户反馈的三个使用问题）

- **更新 DSH 报权限错误**：`npm install -g` 的写入目标（即 npm 全局前缀，
  如 `<Node 安装目录>\node_global`）在部分机器上对普通用户只有「读+执行」权限
  （`Users:(RX)`），更新必然 EPERM/EACCES。现在更新/安装前会探测目标目录可写性
  与当前权限级别，给出具体目录与三条出路，并提供「以管理员身份重启启动器」
  （`ShellExecuteW(runas)`；提权实例以 `--elevated` 豁免单实例互斥，
  否则会被已有实例判定为重复启动而立即退出）；自检面板新增「运行权限」
  「更新权限」两项，让这件事在界面上可见
- 更新/安装前强制要求 DSH 已停止：Windows 上运行中的 node 会占用待替换的
  文件，覆盖会失败——此前确认框里只是「建议先停止」
- **端口被占用只有结论没有解法**：现在展示占用者（PID / 进程名 / 路径），
  可在首页「结束占用进程」（二次确认；终止前复核该 PID 仍在监听目标端口以
  防 PID 复用误杀；拒绝结束 DSH 自身与系统进程），并可直接在界面内修改
  Web 端口——此前 `SettingsPatch` 不含 `webPort`，只能手改 settings.json 再重启
- **插件更新遇到刚发布的版本会报错**：检查更新现在附带该版本的发布年龄
  （`npm view <pkg> time --json`，且只在确实存在可更新版本时才查一次，不给常规
  检查增加开销）；小于 1 小时的版本标注「刚发布」并默认不提供一键更新入口；
  更新失败若命中 `ERR_PNPM_NO_MATCHING_VERSION` / `ERR_PNPM_FETCH_404` /
  `ETARGET` 等特征，自动刷新后重试一次，仍失败则说明「版本刚发布、尚未同步
  完成，建议等 5–30 分钟」——检查更新走 npm、实际安装走 pnpm，两者缓存独立，
  "看得见装不上"是必然存在的时间窗（本机实测：某插件发布后 17 秒即被查到）
- 前端手写刷新状态不再复用 `pollStatus`（它末尾自续期，直接调用会再开一条
  永续轮询），新增不参与轮询链的 `pollOnce`
- **修正在"以管理员权限运行的 DSH"上的两处误判**（上述功能实现后于本机实测发现）：
  DSH 以管理员权限运行时，非提权查询读不到它的 `CommandLine`（实测为 `null`），
  基于命令行的身份核验会 fail closed，于是把自家 DSH 报成「端口被其他程序占用」，
  还会给出误导性的「结束占用进程」。现在：
  - `dsh_state` / `stop_web` / `restart_dsh` / `kill_port_owner` 在命令行核验失败时
    用 **HTTP 协议探针**兜底（DSH web 对未授权请求返回 401 +
    `dsh web authentication required`），不依赖进程元数据、不受权限影响
  - `kill_tree` 如实返回 taskkill 的失败原因，前端据此提示「以管理员身份重启
    启动器」并提供入口——此前错误被 `let _ =` 吞掉，只能猜「可能权限不足」
  - 确认为 DSH 自身时不再提供「结束占用进程」，改为引导使用「关闭 Web 界面」

### 新增（日志落盘与时间工具）

- 子进程完整输出落盘到 exe 旁 `launcher.log`（超过 2MB 时保留尾部 512KB），
  页脚新增「启动器日志」入口。此前 npm / pnpm / dsh plugin 的输出只推给 UI，
  操作失败后**无从回溯**——pnpm 的报错会彻底丢失
- `util.rs` 新增 `cmp_ver`（与前端 `cmpVer` 同一套 SemVer 规则）与
  `parse_rfc3339_utc_secs`（解析 registry 发布时间，不解时区偏移以免把年龄算偏），
  均带单元测试

### 变更（npm 查询逻辑去重）

- `npm_view_version` 与新增的 `npm_view_publish_time` 共用 `run_npm_query`，
  消除重复的「管道排空 + 超时 + 进程树终止」实现

### 修复（前端健壮性与子进程终止，同日较早一批）

- 前端顶层事件绑定补 `?.` 守卫，共享状态函数改用 `setEl` 安全赋值，并在
  启动时用 `assertRequiredIds` 一次性报出缺失元素：此前任一元素 id 漂移都会
  抛异常中断其后全部绑定（表现为大片功能一起静默失效）
- 「重启 DSH」改用三态 `status_detail` 判定：webPort 被无关程序占用时
  不再走完确认框才被后端拒绝，而是直接给出可行动提示
- 子进程超时/失败终止改为 `taskkill /T` 终止整棵进程树：npm/pnpm 在 Windows
  上经 cmd.exe 包装，原先只 kill 直接子进程会留下真正干活的 node 继续在后台
  写 node_modules，启动器却已报失败
- `which` 的未命中结果缓存加 60 秒 TTL：运行期间新装 pnpm/node 不再需要
  手工点一次「重新检查」才能被识别
- 「备份 DSH 数据」补进度上报（阶段 + 条目数 + 原始体积 + 产物体积）：
  此前大 `$DSH_HOME` 打包期间界面只有一个置灰按钮，观感像卡死
- 侧栏版本行不再硬编码版本号与 profile（版本取 `app.getVersion()`，
  profile 取 settings.json 并随切换同步），消除文档/界面漂移

### 新增（同日较早一批）

- 前端元素契约测试 `tauri/ui/test/element-contract.test.mjs`：校验
  index.html 的 id、JS 中 `$("…")` 引用、`REQUIRED_IDS` 清单三者一致，
  让 id 漂移在 CI 阶段暴露而不是运行时静默失效

### 变更（同日较早一批）

- README 修正 exe 体积（7.6MB → 约 9.9MB，实测）与 Release 产物命名说明
- CI 的 Rust job 设 `CARGO_INCREMENTAL=0`：rustc 1.98.0 在「增量编译 +
  clippy + 已存在 test 产物」时会 ICE（`rmeta/encoder.rs: no entry found for
  key`），与源码无关（已在未改动的提交上复现），会造成随机 CI 失败

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
- 启动操作互斥：`start_web` / `start_tui` / `restart_dsh` 共用一把锁，
  与备份/恢复互斥同款实现（RAII guard）

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
