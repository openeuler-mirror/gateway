# 网关控制台麒麟系统弹窗卡顿修改方案

日期：2026-10-22
基线：主仓库 `gateway-gitcode`，提交 `0f3f558`
状态：本地源码已修改并完成开发环境验证，作为独立修复提交；麒麟部署后仍需现场验收。

## 1. 问题现象

在麒麟系统的浏览器中，打开网关控制台新增模型、新增团队时，操作明显缓慢、卡顿严重；Windows 环境下未出现同等程度的问题。

现场在浏览器开发者工具 Console 中临时关闭弹窗背景模糊和入场动画后，用户反馈操作明显变快。

## 2. 原因分析与依据

新增模型和新增团队使用公共弹窗函数 `showModal()`，共用 `.modal-overlay` 和 `.modal` 样式。

原实现对覆盖整个视口的遮罩启用 `backdrop-filter: blur(6px)`。该效果需要对背景内容执行模糊处理，增加渲染与合成开销。弹窗本身还带有缩放、位移和透明度入场动画。

现场关闭这两项效果后明显改善，说明弹窗视觉效果是本次卡顿的重要贡献因素，应优先简化公共弹窗样式。

麒麟上的浏览器版本、图形加速状态、显卡驱动、终端硬件和远程桌面环境可能放大这类开销，但目前没有现场性能录制或图形状态证据，不能认定为某个驱动或麒麟系统本身的缺陷。由于对照实验同时关闭了两项效果，也尚未量化二者各自的影响。

## 3. 修改方案

统一简化公共弹窗，适用于所有浏览器：

1. 删除全屏背景模糊，继续使用普通半透明遮罩区分前景与背景。
2. 删除弹窗入场动画以及不再使用的 `modalIn` 关键帧。
3. 保留浅色、深色主题遮罩、弹窗尺寸、滚动区域、圆角和阴影。

本次仅修改前端 CSS，不调整新增模型、团队的接口、数据库结构或业务逻辑；不引入按操作系统或浏览器识别的分支。

## 4. 修改文件与具体内容

修改文件：

```text
crates/boom-dashboard/src/frontend/style.css
```

修改位置：约第 1316 行，公共弹窗样式区域。

实际差异：

```diff
 .modal-overlay {
   position: fixed; inset: 0; background: rgba(0,0,0,.55);
   display: flex; align-items: center; justify-content: center; z-index: 100;
-  backdrop-filter: blur(6px);
+  /* Keep modal interaction responsive on software-rendered desktops. */
 }
 [data-theme="light"] .modal-overlay { background: rgba(0,0,0,.3); }
 .modal {
   background: var(--surface); border: 1px solid var(--border);
   border-radius: var(--radius); padding: 28px;
   width: 500px; max-width: 90vw; max-height: 80vh; overflow-y: auto;
   box-shadow: var(--shadow-lg);
-  animation: modalIn .25s cubic-bezier(.16,1,.3,1);
-}
-@keyframes modalIn {
-  from { opacity: 0; transform: scale(.96) translateY(8px); }
-  to   { opacity: 1; transform: scale(1) translateY(0); }
 }
```

关联代码：`frontend/app.js` 中的 `showNewModelModal()`、`showCreateTeamModal()` 均调用公共 `showModal()`。因此修改会同时覆盖新增模型、团队及其他共用该样式的弹窗。

视觉变化：弹窗直接显示，背景仅变暗、不再模糊。

## 5. 本地验证结果

本次已在 Windows 主机的 Docker 开发环境完成：

| 验证项 | 结果 |
| --- | --- |
| 重新构建并启动网关及业务 Hook | 成功 |
| 网关、PostgreSQL、MySQL 健康状态 | 均为 healthy |
| `/health/ready` | HTTP 200 |
| `/dashboard/style.css` 实际响应 | 已无背景 `blur(6px)` 和 `modalIn` 动画 |
| 现有业务 Hook 接口冒烟检查 | 9 项全部通过，包括鉴权、模型改写及预期拒绝场景 |
| `git diff --check` | 通过 |

麒麟现场已验证临时样式调整能明显改善操作；尚未在麒麟上部署本次重新构建的程序并复测。上述接口检查验证服务正常，不代表浏览器渲染性能的量化测试，也不包含新增模型、团队的真实写入验证。

移植版在隔离 PostgreSQL 的 Linux 容器中重新构建并启动，健康检查返回 HTTP 200。Windows Chrome 验证深色、浅色主题下新增模型和新增团队弹窗的打开、输入与关闭，并验证模型弹窗可滚动。浏览器实际收到的样式已移除公共弹窗的背景模糊和入场动画。

## 6. 麒麟构建与部署

### 6.1 生效原理

`crates/boom-dashboard/src/handlers_static.rs` 使用 `include_str!("frontend/style.css")`，在编译时将样式嵌入网关二进制。

因此必须重新构建并部署网关程序。仅修改服务器上的 CSS 源文件，或只重启旧二进制，不会带来新样式。

### 6.2 构建

在具备对应工具链和依赖的麒麟构建环境中，将修改应用到主仓库，再在主仓库根目录执行与现有部署一致的构建命令。例如 release 构建：

```bash
cargo build --release --locked -p boom-main --bin boom-gateway
```

产物为 `target/release/boom-gateway`。使用显式 `--target` 时，以实际目标架构目录中的产物为准。

若使用本项目已配置的麒麟离线工具链，可执行：

```bash
boom-cargo-offline build --release --frozen -p boom-main --bin boom-gateway
```

离线构建要求 vendor 与当前锁文件匹配。应使用与目标机 CPU 架构、运行库兼容的构建环境；本次没有生成可直接交付的麒麟二进制或镜像。

### 6.3 部署与刷新

1. 备份现用网关程序或镜像，记录版本及服务启动参数。
2. 按现场既有运维流程停止网关，替换新二进制后启动；容器部署则重新构建镜像并更新服务。
3. 保留现有业务配置、环境变量和 Hook 动态库；本次样式修改不要求变更数据库或重新制作 Hook。
4. 确认健康检查通过，再在浏览器按 `Ctrl+Shift+R` 强制刷新控制台。

服务名、部署目录及镜像名按现场实际配置执行。本次未在麒麟执行部署操作。

## 7. 现场验收

使用与问题复现时相同的麒麟终端、浏览器、网关地址、账号和数据：

- 确认未依赖此前 Console 注入的临时样式；刷新后直接测试。
- 在开发者工具 Network 中查看 `/dashboard/style.css` 响应，确认 `.modal-overlay` 不再启用背景模糊，且已删除 `modalIn`。
- 分别打开新增模型、新增团队，检查打开速度、输入、下拉选择及滚动是否流畅。
- 使用专用测试数据验证创建、保存及列表刷新，完成后清理测试记录。
- 检查浅色、深色主题及其他新增、编辑弹窗的遮罩、布局和关闭行为。
- 使用 Windows 浏览器回归相同操作。

若仍有卡顿，应分别记录“打开弹窗”“弹窗内操作”“保存等待”的耗时，再结合网络请求和浏览器性能录制继续定位，避免把后端等待与页面绘制混为一谈。

## 8. 回退

部署异常时，按原部署流程恢复备份的二进制或镜像，再启动并验证健康状态。回退不需要数据库变更。

源码回退仅恢复本文件的上述样式差异，避免覆盖其他本地改动；源码回退后同样需要重新构建、部署才能生效。
