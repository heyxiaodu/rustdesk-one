# tray-dark-mode —— 托盘图标「深色/浅色两套」单色剪影资产

> **本副本说明（task-40 入库）**：本目录是这些资产的**入仓副本**，位置在软件仓库 `repos/rustdesk` 内，因为它同时是构建输入 —— 两个 ICO 由 `src/tray.rs` 用 `include_bytes!("../brand/tray-dark-mode/ico/…")` 内嵌进二进制。生成器与测量脚本仍在 NERVDesk 工作树的 `analysis/brand/`；canonical 另一份在 `brand-assets/tray-dark-mode/`（两边的资产文件逐字节相同，仅本 README 比那份多了本说明与 §6）。
>
> 下文（§3、§5）里以 `../` 开头的路径按 **NERVDesk 工作树**解读：`../svg/`、`../png/`、`../ico/`、`../white-variants.sha256` 指 `brand-assets/` 下的同级目录 —— 本目录的兄弟目录里并没有 `svg/`、`png/logo-source.png`、`ico/`。§4 的校验命令在本目录执行即可（`cd repos/rustdesk/brand/tray-dark-mode && sha256sum -c sha256.txt`）。§5 的「未接入运行时」是 task-36 时点的状态，已被 task-40 取代，见 §6。

派生自 **task-36**（用户裁决：**深色任务栏用白、浅色任务栏用深墨（如 `#1A1A1A`），自动切换** + **单色剪影一套贯通**）。

本目录是**派生素材**，不是原始设计稿；原始稿见 `../svg/`、`../png/logo-source.png`、`../png/icon-1024.png`。

## 1. 产物清单（16 个资产文件 + 1 个校验清单 + 本 README）

| 文件 | 尺寸 | 字节 | 内容 |
|---|---|---|---|
| `png/tray-16-white.png` | 16×16 | 400 | 白 `#FFFFFF`，alpha = 形状覆盖度 |
| `png/tray-24-white.png` | 24×24 | 573 | 同上 |
| `png/tray-32-white.png` | 32×32 | 727 | 同上 |
| `png/tray-48-white.png` | 48×48 | 1091 | 同上 |
| `png/tray-64-white.png` | 64×64 | 1464 | 同上 |
| `png/tray-128-white.png` | 128×128 | 3214 | 同上（主路径规格用） |
| `png/tray-256-white.png` | 256×256 | 7663 | 同上（与运行期主源 256×256 同规格） |
| `ico/tray-white.ico` | 5 帧 16/24/32/48/64 | 4341 | 同上，多尺寸容器（PNG 载荷） |
| `png/tray-16-dark-ink.png` | 16×16 | 398 | 深墨 `#1A1A1A`，alpha 与白色套**逐像素相同** |
| `png/tray-24-dark-ink.png` | 24×24 | 573 | 同上 |
| `png/tray-32-dark-ink.png` | 32×32 | 735 | 同上 |
| `png/tray-48-dark-ink.png` | 48×48 | 1087 | 同上 |
| `png/tray-64-dark-ink.png` | 64×64 | 1437 | 同上 |
| `png/tray-128-dark-ink.png` | 128×128 | 3125 | 同上（主路径规格用） |
| `png/tray-256-dark-ink.png` | 256×256 | 7355 | 同上 |
| `ico/tray-dark-ink.ico` | 5 帧 16/24/32/48/64 | 4316 | 同上，多尺寸容器（PNG 载荷） |
| `sha256.txt` | — | 1430 | 16 条 sha256；`sha256sum -c sha256.txt` 校验 |

## 2. 为什么是两套（不是审美选择）

| 套 | 目标底色 | 对比度（主色口径） | 在相反底色上 |
|---|---|---|---|
| `-white` | 深色任务栏（`#0A0A0A` / `#000000` / `#1C1C1C`） | 19.80:1 / 21.00:1 / 17.04:1 | `#FFFFFF` 上 **1.00:1** ⇒ 不可见 |
| `-dark-ink` | 浅色任务栏（`#FFFFFF` / `#F3F3F3`） | 17.40:1 / 15.68:1 | `#0A0A0A` 上 **1.14:1** ⇒ 不可见 |

门槛 = 非文本 UI 图形 ≥ **3:1**（WCAG 2.x 1.4.11）。**任一套都无法单独满足两侧** ⇒ 必须按任务栏深浅切换（两套 + 自动切换）。

现状基线（同一测量口径）：运行期主源 `repos/rustdesk/flutter/assets/icon.png` 在 `#0A0A0A` 上的对比度 = **1.14:1**，托盘回归源 `repos/rustdesk/res/tray-icon.ico` 帧 0 = **1.13:1** ⇒ 今天的托盘图标在 NERV 深色底上几乎不可见。

## 3. 生成方法与 provenance（可复现）

- **形状来源**：`../png/icon-1024.png` 的 **alpha 通道**（192 个不同 alpha 取值 ⇒ 是抗锯齿覆盖度，不是二值掩膜）。该 mask 缩放到 32×32 后与运行期真实主源 `../png/icon-256.png`（= `repos/rustdesk/flutter/assets/icon.png`，字节相同）的 alpha 平面平均绝对差 **0.0037/1.0** ⇒ 同族，接入后不会改变主路径取景。
- **不用的来源**：`../svg/nervdesk-icon-mono.svg` 内嵌 base64 PNG，本机 ImageMagick 6 的内置渲染器无法光栅化（`convert` 报 `unable to open image 'image/png;base64,…'` / `no images defined`），且本机无 `rsvg-convert`/`inkscape`/`magick` 委托。
- **步骤**：`convert icon-1024.png -alpha extract -filter Mitchell -resize NxN -depth 8 gray:-` 取覆盖度 → 纯 stdlib 写 8 bit RGBA PNG（**RGB 恒为单色常量**，A = 覆盖度）→ 纯 stdlib 组装多尺寸 ICO（PNG 载荷）。
- **脚本**：生成器 `analysis/brand/gen-tray-dark-mode.py`（幂等、`--dry-run`）；测量 `analysis/brand/measure-tray-dark-mode.py`（只读）；原始测量输出 `analysis/brand/tray-dark-mode-measure-raw.txt`；完整方案 `analysis/brand/tray-icon-dark-mode-plan.md`。
- **ICO 合法性**：`reserved=0 type=1 count=5`，帧 16/24/32/48/64、`bpp=32`、每帧 PNG 签名正确；与本仓既有约定一致（`repos/rustdesk/res/tray-icon.ico`、`repos/rustdesk/flutter/assets/icon.ico` 同为 PNG 载荷），且 `image` crate（本仓 `Cargo.toml:203 image = "0.24"`）的 `best_entry()` 按 `(bpp, 宽×高)` 会选中 64×64 帧、`is_png()` 支持 PNG 帧（内嵌 PNG 须为 32 BPP RGBA —— 本目录全部满足）。

## 4. 校验

```bash
cd brand-assets/tray-dark-mode
sha256sum -c sha256.txt            # 期望 16/16 OK

# 复核两套形状相同（alpha 平面逐像素）
for s in 16 24 32 48 64 128 256; do
  convert png/tray-$s-white.png    -alpha extract -depth 8 gray:- > /tmp/a.raw
  convert png/tray-$s-dark-ink.png -alpha extract -depth 8 gray:- > /tmp/b.raw
  cmp -s /tmp/a.raw /tmp/b.raw && echo "$s px IDENTICAL" || echo "$s px DIFFERS"
done

# 复核主色（不透明像素应 100% 为单色）
convert png/tray-32-white.png    -depth 8 -format %c histogram:info:- | grep -v ',0)' | sort -rn | head -3
convert png/tray-32-dark-ink.png -depth 8 -format %c histogram:info:- | grep -v ',0)' | sort -rn | head -3
```

## 5. 状态与边界

- **未接入运行时** ⇒ **本轮零用户可见效果**。`repos/rustdesk/**` 代码本轮一个字节未改；`repos/rustdesk/flutter/assets/icon.png` 与 `repos/rustdesk/res/tray-icon.ico` 保持原字节。
- **未覆盖** `../ico/tray-white.ico`、`../png/tray-{16,32}-white.png`、`../png/icon-32@2x-{bw,white}.png` 等 task-19 产物（它们登记在 `../white-variants.sha256`，而 NERVDesk 根目录不是 git 仓库 ⇒ 覆盖无历史可回滚）。task-19 的 `ico/tray-white.ico` 已复核为**合法单色 AA 剪影**（100 个半透明像素 RGB 全 255、102 个 alpha 级），差异只有：字形切割不同（Variant B vs 本目录的 Variant A）、帧集只有 16/32、同样未接入运行时。
- **真机验证**（Windows 任务栏渲染、主题切换、DPI 缩放、高对比度）一律 **NOT TESTED**。
- 证据等级：**静态 PASS + 运行 NOT TESTED**。所有数字可用上文命令复算。

## 6. 运行期接入（task-40 起）

- **判据**：`SystemUsesLightTheme`（Windows 任务栏颜色，`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`）。**不是** `AppsUseLightTheme`（应用主题，也是 Flutter `platformBrightness` 与 tao uxtheme 的来源）—— 两者可以不同。
- **映射**：任务栏浅 ⇒ `-dark-ink`；任务栏深 ⇒ `-white`；读不到 / 读取失败 / 值非 `0`/`1` ⇒ **保持现状**（不猜颜色、不 panic）。
- **两条加载路径都接入**：① 主源 `src/tray.rs` 的 `load_icon_from_asset()`（Flutter 资源目录；本目录 256 px 两件已复制进 `flutter/assets/`，名 `tray-256-white.png` / `tray-256-dark-ink.png`）；② 内嵌回落（本目录两个 ICO，`include_bytes!`）—— 交叉构建产物没有 `data/flutter_assets/`，本环境真正生效的是 ②。
- **切换**：既有 100 ms 事件循环里每 2 s 复查一次，变化时调 `tray-icon::TrayIcon::set_icon()`（`tray-icon 0.21.3`）；托盘进程没有窗口，收不到 tao 的 `WM_WININICHANGE`，只能轮询。
- 实现与逐条证据等级见 NERVDesk 工作树中的 `analysis/brand/tray-dark-mode-impl.md`（不在本仓内）。
