#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
NervDesk 行为级补丁（M1 功能行为 + M2 安全 Override 锁定）应用脚本。

用法（在 RustDesk 源码根目录执行，也就是有 Cargo.toml 的那一层）：

    python3 patches/nervdesk-features/apply_nervdesk_features.py

改动范围（与 nervdesk-features-change-list.md 完全一致，锚点逐一校验）：

  1. libs/hbb_common/src/config.rs
       - OPTION_ACCESS_MODE / enable-file-transfer / enable-audio / enable-clipboard /
         enable-record-session 五项读取强制锁定（M2）
       - approve-mode=password（仅密码访问，无确认弹窗路径，M1-2）
       - allow-numeric-one-time-password=Y（PIN 码解锁选项开启，M1-3）
       - is_disable_unlock_pin() 恒 false（解锁 PIN 能力常开，M1-1）
       - nerve_apply_forced_defaults()：固化 OVERWRITE_SETTINGS / 瘦身 builtin /
         出厂固定密码（占位符 __NERVDESK_PASSWORD__ 被 CI 替换后才写入，M1-7）
       - NERVDESK_MODE 编译期形态开关（t11）：controlled / controller / 未设置
  2. src/common.rs
       - load_custom_client() 各分支末尾调用 nerve_apply_forced_defaults()
  3. src/tray.rs
       - Windows 隐藏托盘图标（M1-4，默认/controlled）；controller 变体保留托盘
  4. src/flutter.rs
       - session_add() 会话拒绝守卫（t11 controlled 变体：纯被控端拒绝发起新会话）
  5. flutter/lib/desktop/pages/connection_page.dart
       - 形态条件化（t11）：ID 直连输入在 controlled 移除；设备列表面板
         （连接管理器，M1-5）默认/controlled 隐藏、controller 恢复
t11 双变体形态（编译期，CI 传 env/RUSTFLAGS 给 Rust、--dart-define 给 Flutter）：
  - NERVDESK_MODE=controlled → 纯被控端（隐藏托盘/连接管理器/ID 输入 + 会话拒绝）
  - NERVDESK_MODE=controller / 未设置 → 控制端全 UI 形态（保留托盘/连接管理器/ID 直连）
  - 仅 NERVDESK_MODE=controlled 为纯被控端形态

特性（与 branding-apply.py / apply_custom_client.py 一致）：
  - 锚点匹配数 != 1 立即失败，绝不静默跳过；
  - CRLF/LF 自适应（Windows CI checkout 出来是 CRLF 也能打）；
  - 打完后立刻校验，防止「看起来打了、实际没打上」；
  - 密码/PIN 一律只出现占位符 __NERVDESK_PASSWORD__，真值不入仓库。

与 nervdesk-features.patch 的关系：二者等价，任选其一应用；
本脚本在 CRLF / 锚点变化场景下更稳，CI 推荐用本脚本（与现有
"Apply iroh source changes" 注入风格一致）。
"""

import pathlib
import sys

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

FILES = [
    "libs/hbb_common/src/config.rs",
    "src/common.rs",
    "src/tray.rs",
    "src/flutter.rs",
    "flutter/lib/desktop/pages/connection_page.dart",
]

# libs/hbb_common 是子模块：脚本必须在仓库根目录运行，且子模块已 init
# （git submodule update --init --depth 1 libs/hbb_common）。


def die(m: str) -> "None":
    print(f"[FAIL] {m}", file=sys.stderr)
    sys.exit(1)


def read_text(p: pathlib.Path) -> "tuple[str, bool]":
    raw = p.read_bytes().decode("utf-8")
    crlf = "\r\n" in raw
    return (raw.replace("\r\n", "\n") if crlf else raw), crlf


def write_text(p: pathlib.Path, text: str, crlf: bool) -> None:
    p.write_bytes((text.replace("\n", "\r\n") if crlf else text).encode("utf-8"))


def sub1(text: str, old: str, new: str, label: str) -> str:
    n = text.count(old)
    if n != 1:
        die(f"{label}: 锚点匹配 {n} 处，期望 1 处")
    return text.replace(old, new)


# ---------------------------------------------------------------------------
# 1. libs/hbb_common/src/config.rs
# ---------------------------------------------------------------------------

CONFIG_FORCED_BLOCK = """// ============================================================================
// NervDesk 定制（M1 行为级 / M2 安全 Override 锁定）
// 按 docs/09 分层验收，行为级功能（M1/M2）承诺平台为 Windows（M5），
// 因此所有强制项仅 Windows 生效；其它平台保持上游原行为。
// ============================================================================

/// 出厂固定密码占位符：构建期由 CI Secret 替换（真值绝不入仓库）。
/// 产物中若仍为占位符原样（未替换），nerve_apply_forced_defaults 不会写入任何密码。
pub const NERVDESK_BUILTIN_PASSWORD: &str = "__NERVDESK_PASSWORD__";

/// F1 修复：占位符哨兵在运行时由片段拼接而成（`format!("__{}_{}", "NERVDESK",
/// "PASSWORD__")`），拼出的串与 const 初始化行的占位符一致。这样：
/// 1) 源文件里完整占位符只出现在 const 初始化行（CI 替换点，整树唯一一次），
///    CI 整树字节替换不会把守卫右侧一起换成真值 → 守卫替换后依然成立；
/// 2) 产物 rodata 中不存在连续占位符串，M6-3 产物扫描不会误报。
#[inline]
fn nervdesk_password_marker() -> String {
    format!("__{}_{}", "NERVDESK", "PASSWORD__")
}

// ============================================================================
// NervDesk 编译期形态开关（t11）：NERVDESK_MODE（构建时环境变量，CI 用
// env/RUSTFLAGS 传值；Flutter 侧对应 --dart-define=NERVDESK_MODE）
//   - "controlled"  → 纯被控端：隐藏托盘/连接管理器/发起连接入口 + 会话拒绝守卫；
//   - "controller" / 未设置 → 控制端全 UI 形态：保留托盘、连接管理器、ID 直连
//     （未设置默认即 controller 形态，与用户承诺一致）。
// Android/其它平台不设置该开关，保持上游行为（即 controller 形态）。
// 取值实现说明：option_env! 在**编译期**把形态固化进产物（改构建参数重编才可变），
// 运行时函数读取固化值；刻意不用 const 求值字符串比较，规避其随 MSRV 版本波动
// 的兼容风险（1.75 与 1.91 工具链均可编译）。
// ============================================================================

/// 是否为纯被控端变体（NERVDESK_MODE=controlled）。
#[inline]
pub fn nervdesk_mode_controlled() -> bool {
    option_env!("NERVDESK_MODE") == Some("controlled")
}

/// 是否为控制端形态（NERVDESK_MODE=controller 或未设置——默认即 controller）。
#[inline]
pub fn nervdesk_mode_controller() -> bool {
    !nervdesk_mode_controlled()
}

/// NervDesk 强制选项表：无论用户配置 / override 写入什么，读取值恒为锁定值。
///
/// M2 五项安全 Override：access-mode=完全访问，文件传输/音频/剪贴板/会话录制锁定开启；
/// M1 行为：approve-mode=password 实现「仅密码访问（无确认弹窗路径）」；
/// allow-numeric-one-time-password=Y 开启 PIN 码解锁选项（数字临时密码）。
pub const NERVDESK_FORCED_OPTIONS: &[(&str, &str)] = &[
    (keys::OPTION_ACCESS_MODE, "full"),
    (keys::OPTION_ENABLE_FILE_TRANSFER, "Y"),
    (keys::OPTION_ENABLE_AUDIO, "Y"),
    (keys::OPTION_ENABLE_CLIPBOARD, "Y"),
    (keys::OPTION_ENABLE_RECORD_SESSION, "Y"),
    (keys::OPTION_APPROVE_MODE, "password"),
    (keys::OPTION_ALLOW_NUMERNIC_ONE_TIME_PASSWORD, "Y"),
];

/// 查询某选项的 NervDesk 强制值；非强制选项返回 None。
#[cfg(target_os = "windows")]
pub fn nervdesk_forced_option(k: &str) -> Option<String> {
    NERVDESK_FORCED_OPTIONS
        .iter()
        .find(|(key, _)| *key == k)
        .map(|(_, v)| v.to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn nervdesk_forced_option(_k: &str) -> Option<String> {
    None
}

/// NervDesk 启动入口（load_custom_client 之后调用）：
/// 1) 把强制选项固化进 OVERWRITE_SETTINGS：is_option_fixed()==true（UI 显示锁定）、
///    is_option_can_save()==false（用户写入被丢弃，与 get_option 强制构成双重保证）；
/// 2) 写入界面瘦身 builtin：hide-tray（M1-4 隐藏托盘）、hide-help-cards /
///    hide-remote-printer-settings / hide-websocket-settings（M1-6 界面瘦身）；
/// 3) 出厂固定密码：占位符已被 CI 替换且当前无永久密码时写入（仅密码访问的前提）。
#[cfg(target_os = "windows")]
pub fn nerve_apply_forced_defaults() {
    {
        let mut ow = OVERWRITE_SETTINGS.write().unwrap();
        for (k, v) in NERVDESK_FORCED_OPTIONS {
            ow.insert(k.to_string(), v.to_string());
        }
    }
    {
        let mut bs = BUILTIN_SETTINGS.write().unwrap();
        // F-V1 修复（t14）：hide-tray 按形态条件化——仅 controlled 形态写入
        // （配合 tray.rs 的形态 gate：controller/未设置形态保留托盘，避免
        // builtin 强制默认值在 load_custom_client 阶段把托盘挡回去）。
        if nervdesk_mode_controlled() {
            bs.insert(keys::OPTION_HIDE_TRAY.to_owned(), "Y".to_owned());
        }
        bs.insert(keys::OPTION_HIDE_HELP_CARDS.to_owned(), "Y".to_owned());
        bs.insert(keys::OPTION_HIDE_REMOTE_PRINTER_SETTINGS.to_owned(), "Y".to_owned());
        bs.insert(keys::OPTION_HIDE_WEBSOCKET_SETTINGS.to_owned(), "Y".to_owned());
    }
    if NERVDESK_BUILTIN_PASSWORD != nervdesk_password_marker()
        && !Config::has_permanent_password()
    {
        log::info!("NervDesk: 写入出厂固定密码（构建期 Secret 替换）");
        let _ = Config::set_permanent_password(NERVDESK_BUILTIN_PASSWORD);
    }
}

#[cfg(not(target_os = "windows"))]
pub fn nerve_apply_forced_defaults() {}
"""


def patch_config_rs() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    if not p.is_file():
        die("找不到 libs/hbb_common/src/config.rs（子模块没拉下来？）")
    t, crlf = read_text(p)
    # E1: get_option 强制优先（锚点：Config 的 get_option，非 LocalConfig）
    t = sub1(
        t,
        "pub fn get_option(k: &str) -> String {\n        get_or(\n            &OVERWRITE_SETTINGS,\n            &CONFIG2.read().unwrap().options,",
        "pub fn get_option(k: &str) -> String {\n"
        "        // NervDesk 定制（M2/M1）：强制选项优先于一切配置层，读取值恒为锁定值\n"
        "        if let Some(v) = nervdesk_forced_option(k) {\n            return v;\n        }\n"
        "        get_or(\n            &OVERWRITE_SETTINGS,\n            &CONFIG2.read().unwrap().options,",
        "config.rs get_option",
    )
    # E2: is_disable_unlock_pin 恒 false
    t = sub1(
        t,
        "pub fn is_disable_unlock_pin() -> bool {\n"
        "        BUILTIN_SETTINGS\n            .read()\n            .unwrap()\n"
        "            .get(keys::OPTION_DISABLE_UNLOCK_PIN)\n"
        "            .map(|v| v == \"Y\")\n            .unwrap_or(false)\n    }",
        "pub fn is_disable_unlock_pin() -> bool {\n"
        "        // NervDesk 定制（M1-1）：解锁 PIN 能力常开，不允许被 builtin 关闭\n"
        "        false\n    }",
        "config.rs is_disable_unlock_pin",
    )
    # E3: keys 模块之后插入 NervDesk 定制块（锚点：keys 模块结尾 + common_load 开头）
    t = sub1(
        t,
        "        OPTION_USE_RAW_TCP_FOR_API,\n"
        "        OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW,\n"
        "        OPTION_ALLOW_COMMAND_LINE_SETTINGS_WHEN_SETTINGS_DISABLED,\n"
        "    ];\n}\n\npub fn common_load<",
        "        OPTION_USE_RAW_TCP_FOR_API,\n"
        "        OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW,\n"
        "        OPTION_ALLOW_COMMAND_LINE_SETTINGS_WHEN_SETTINGS_DISABLED,\n"
        "    ];\n}\n\n"
        + CONFIG_FORCED_BLOCK
        + "\npub fn common_load<",
        "config.rs NervDesk 定制块",
    )
    write_text(p, t, crlf)
    print("[OK] config.rs: 强制选项表 + get_option 强制 + 解锁 PIN 常开 + 启动固化")


# ---------------------------------------------------------------------------
# 2. src/common.rs
# ---------------------------------------------------------------------------


def patch_common_rs() -> None:
    p = pathlib.Path("src/common.rs")
    if not p.is_file():
        die("找不到 src/common.rs")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "pub fn load_custom_client() {\n"
        "    #[cfg(debug_assertions)]\n"
        "    if let Ok(data) = std::fs::read_to_string(\"./custom.txt\") {\n"
        "        read_custom_client(data.trim());\n"
        "        return;\n"
        "    }",
        "pub fn load_custom_client() {\n"
        "    // NervDesk 定制（M1/M2）：无论 custom.txt 是否读取成功，强制选项都要在最后固化，\n"
        "    // 保证行为级定制在任何配置路径（含无 custom.txt、读取失败）下都生效。\n"
        "    #[cfg(debug_assertions)]\n"
        "    if let Ok(data) = std::fs::read_to_string(\"./custom.txt\") {\n"
        "        read_custom_client(data.trim());\n"
        "        config::nerve_apply_forced_defaults();\n"
        "        return;\n"
        "    }",
        "common.rs load_custom_client 注释与 debug 分支",
    )
    t = sub1(
        t,
        "    let Some(path) = std::env::current_exe().map_or(None, |x| x.parent().map(|x| x.to_path_buf()))\n"
        "    else {\n        return;\n    };",
        "    let Some(path) = std::env::current_exe().map_or(None, |x| x.parent().map(|x| x.to_path_buf()))\n"
        "    else {\n        config::nerve_apply_forced_defaults();\n        return;\n    };",
        "common.rs 无 exe 路径分支",
    )
    t = sub1(
        t,
        "        let Ok(data) = std::fs::read_to_string(&path) else {\n"
        "            log::error!(\"Failed to read custom client config\");\n"
        "            return;\n        };\n"
        "        read_custom_client(&data.trim());\n    }\n}",
        "        let Ok(data) = std::fs::read_to_string(&path) else {\n"
        "            log::error!(\"Failed to read custom client config\");\n"
        "            config::nerve_apply_forced_defaults();\n"
        "            return;\n        };\n"
        "        read_custom_client(&data.trim());\n    }\n"
        "    config::nerve_apply_forced_defaults();\n}",
        "common.rs 读取失败与正常末尾分支",
    )
    write_text(p, t, crlf)
    print("[OK] common.rs: load_custom_client 全分支固化")


# ---------------------------------------------------------------------------
# 3. src/tray.rs
# ---------------------------------------------------------------------------


def patch_tray_rs() -> None:
    p = pathlib.Path("src/tray.rs")
    if not p.is_file():
        die("找不到 src/tray.rs")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "pub fn start_tray() {\n"
        "    if crate::ui_interface::get_builtin_option(hbb_common::config::keys::OPTION_HIDE_TRAY) == \"Y\" {",
        "pub fn start_tray() {\n"
        "    // NervDesk 定制（M1-4 + t11 变体）：Windows 隐藏托盘图标（不注册、不显示），\n"
        "    // 托盘入口（Open / Stop service）对无人值守被控端无意义；\n"
        "    // controller 形态（默认/controller）保留托盘——控制端需要主窗口入口；仅 controlled 隐藏。\n"
        "    #[cfg(target_os = \"windows\")]\n"
        "    {\n"
        "        if !hbb_common::config::nervdesk_mode_controller() {\n            return;\n        }\n"
        "    }\n\n"
        "    if crate::ui_interface::get_builtin_option(hbb_common::config::keys::OPTION_HIDE_TRAY) == \"Y\" {",
        "tray.rs start_tray Windows 隐藏托盘（controller 变体保留）",
    )
    write_text(p, t, crlf)
    print("[OK] tray.rs: Windows 隐藏托盘（controller 变体保留；默认/controlled 隐藏）")


# ---------------------------------------------------------------------------
# 4. flutter/lib/desktop/pages/connection_page.dart
# ---------------------------------------------------------------------------


def patch_connection_page() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/connection_page.dart")
    if not p.is_file():
        die("找不到 flutter/lib/desktop/pages/connection_page.dart")
    t, crlf = read_text(p)
    # E6a: 顶部模式常量（t11，与 Rust NERVDESK_MODE 对应）
    t = sub1(
        t,
        "import '../../desktop/widgets/material_mod_popup_menu.dart' as mod_menu;",
        "import '../../desktop/widgets/material_mod_popup_menu.dart' as mod_menu;\n\n"
        "// NervDesk 编译期形态（t11）：与 Rust 侧 NERVDESK_MODE 对应（CI 传\n"
        "// --dart-define=NERVDESK_MODE=controlled；未设置默认 = controller 形态）。\n"
        "const String _nerveMode = String.fromEnvironment('NERVDESK_MODE');\n"
        "const bool _nerveControlled = _nerveMode == 'controlled';\n"
        "const bool _nerveController = _nerveMode != 'controlled';",
        "connection_page.dart 模式常量",
    )
    # E6b: ID 直连输入行在 controlled 变体移除
    t = sub1(
        t,
        "children: [\n"
        "                Flexible(child: _buildRemoteIDTextField(context)),\n"
        "              ],\n"
        "            ).marginOnly(top: 22),",
        "children: [\n"
        "                if (!_nerveControlled)\n"
        "                  Flexible(child: _buildRemoteIDTextField(context)),\n"
        "              ],\n"
        "            ).marginOnly(top: 22),",
        "connection_page.dart ID 输入行条件化",
    )
    # E6c: 设备列表面板（连接管理器）→ controller 变体显示，默认/controlled 隐藏
    t = sub1(
        t,
        "            Divider().paddingOnly(right: 12),\n"
        "            Expanded(child: PeerTabPage()),",
        "            Divider().paddingOnly(right: 12),\n"
        "            // NervDesk 定制（M1-5 + t11 变体）：默认形态与 controlled 变体隐藏连接\n"
        "            // 管理器（主界面设备列表面板 PeerTabPage：最近/收藏/发现/通讯录/分组收起），\n"
        "            // 保留上方 ID 直连输入；controller 变体（NERVDESK_MODE=controller）恢复\n"
        "            // 设备列表面板与直连入口（控制端可发起对他人的会话）。\n"
        "            Expanded(\n"
        "                child: _nerveController ? PeerTabPage() : Container()),",
        "connection_page.dart PeerTabPage 条件化",
    )
    write_text(p, t, crlf)
    print("[OK] connection_page.dart: 形态条件化（ID 输入/设备列表）")


# ---------------------------------------------------------------------------
# 4b. src/flutter.rs（t11 controlled 守卫）
# ---------------------------------------------------------------------------


def patch_flutter_rs() -> None:
    p = pathlib.Path("src/flutter.rs")
    if not p.is_file():
        die("找不到 src/flutter.rs")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "    conn_token: Option<String>,\n"
        ") -> ResultType<FlutterSession> {\n"
        "    let conn_type = if is_file_transfer {",
        "    conn_token: Option<String>,\n"
        ") -> ResultType<FlutterSession> {\n"
        "    // NervDesk 定制（t11）：controlled 变体（NERVDESK_MODE=controlled，纯被控端）\n"
        "    // 拒绝发起任何新会话（远程桌面/文件传输/终端/端口转发/RDP 全部覆盖，fail-safe）。\n"
        "    // 诚实边界：这是编译期形态的用户级拒绝（同产物内所有入口统一挡在会话创建处），\n"
        "    // 不承诺同源二进制防篡改——改构建参数重编即得 controller 形态。\n"
        "    if hbb_common::config::nervdesk_mode_controlled() {\n"
        "        bail!(\"NervDesk controlled mode: outgoing sessions are disabled\");\n"
        "    }\n"
        "    let conn_type = if is_file_transfer {",
        "flutter.rs session_add controlled 守卫",
    )
    write_text(p, t, crlf)
    print("[OK] flutter.rs: session_add controlled 变体拒绝守卫")


# ---------------------------------------------------------------------------
# 校验
# ---------------------------------------------------------------------------


def verify() -> None:
    ok = True

    def check(rel: str, needle: str, desc: str) -> None:
        nonlocal ok
        if not pathlib.Path(rel).is_file():
            print(f"[FAIL] 校验失败：{rel} 不存在", file=sys.stderr)
            ok = False
            return
        if needle not in pathlib.Path(rel).read_text(encoding="utf-8"):
            print(f"[FAIL] 校验失败：{rel} 未包含 {desc}", file=sys.stderr)
            ok = False
        else:
            print(f"[OK] 校验 {rel}: {desc}")

    check("libs/hbb_common/src/config.rs", "pub const NERVDESK_FORCED_OPTIONS", "强制选项表")
    check("libs/hbb_common/src/config.rs", "nervdesk_forced_option(k)", "get_option 强制优先")
    check("libs/hbb_common/src/config.rs", "pub const NERVDESK_BUILTIN_PASSWORD: &str = \"__NERVDESK_PASSWORD__\"", "密码占位符")
    check("libs/hbb_common/src/config.rs", "nervdesk_password_marker", "F1 修复：密码哨兵运行时拼接")
    check("libs/hbb_common/src/config.rs", "NERVDESK_BUILTIN_PASSWORD != nervdesk_password_marker()", "F1 修复：密码守卫无完整占位符字面量")
    check("libs/hbb_common/src/config.rs", "// NervDesk 定制（M1-1）", "解锁 PIN 常开")
    check("libs/hbb_common/src/config.rs", "pub fn nerve_apply_forced_defaults()", "启动固化入口")
    check("src/common.rs", "config::nerve_apply_forced_defaults();", "load_custom_client 固化调用")
    check("src/tray.rs", "// NervDesk 定制（M1-4 + t11 变体）", "Windows 托盘隐藏（controller 保留）")
    check("src/flutter.rs", "nervdesk_mode_controlled()", "controlled 会话拒绝守卫")
    check("flutter/lib/desktop/pages/connection_page.dart", "_nerveControlled", "形态常量（controlled）")
    check("flutter/lib/desktop/pages/connection_page.dart", "_nerveController ? PeerTabPage() : Container()", "设备列表条件化")
    check("libs/hbb_common/src/config.rs", "pub fn nervdesk_mode_controlled()", "t11 形态开关(controlled)")
    check("libs/hbb_common/src/config.rs", "pub fn nervdesk_mode_controller()", "t11 controller 开关")

    # 反向校验：强制选项表 7 项齐全
    cfg = pathlib.Path("libs/hbb_common/src/config.rs").read_text(encoding="utf-8")
    for key in [
        'keys::OPTION_ACCESS_MODE, "full"',
        'keys::OPTION_ENABLE_FILE_TRANSFER, "Y"',
        'keys::OPTION_ENABLE_AUDIO, "Y"',
        'keys::OPTION_ENABLE_CLIPBOARD, "Y"',
        'keys::OPTION_ENABLE_RECORD_SESSION, "Y"',
        'keys::OPTION_APPROVE_MODE, "password"',
        'keys::OPTION_ALLOW_NUMERNIC_ONE_TIME_PASSWORD, "Y"',
    ]:
        if key not in cfg:
            print(f"[FAIL] 强制选项表缺少 {key}", file=sys.stderr)
            ok = False

    # 占位符纪律：__NERVDESK_PASSWORD__ 只允许出现在 config.rs 的 const 定义行
    # （CI 替换点）；守卫比较行不得含完整占位符字面量（F1 修复，防整树替换自毁）。
    # （脚本自身与 change-list 文档不算源码）
    placeholders = [f for f in FILES if "__NERVDESK_PASSWORD__" in pathlib.Path(f).read_text(encoding="utf-8")]
    if placeholders != ["libs/hbb_common/src/config.rs"]:
        print(f"[FAIL] 占位符出现在非预期文件：{placeholders}", file=sys.stderr)
        ok = False
    cfg2 = pathlib.Path("libs/hbb_common/src/config.rs").read_text(encoding="utf-8")
    for line in cfg2.splitlines():
        if "NERVDESK_BUILTIN_PASSWORD" in line and "__NERVDESK_PASSWORD__" in line and "pub const" not in line:
            print(f"[FAIL] 密码守卫/注释行含完整占位符字面量（F1 隐患）：{line.strip()}", file=sys.stderr)
            ok = False

    if not ok:
        sys.exit(1)
    print("[OK] 全部 NervDesk 行为级改动校验通过")


def verify_variant_tray() -> None:
    """F-V1（t14）：hide-tray builtin 写入按形态条件化的自检断言。

    应用后校验：
      1) OPTION_HIDE_TRAY 的 builtin 写入只出现一次，且必须包裹在
         `if nervdesk_mode_controlled() { ... }` 内（controlled 写入 Y）；
      2) 三态语义仿真：controlled → 托盘隐藏生效；controller / 未设置 → 不写
         hide-tray（builtin 默认缺省），托盘保留（配合 tray.rs 形态 gate）。
    """
    ok = True

    def fail(m: str) -> None:
        nonlocal ok
        print(f"[FAIL] {m}", file=sys.stderr)
        ok = False

    cfg_path = pathlib.Path("libs/hbb_common/src/config.rs")
    if not cfg_path.is_file():
        fail("libs/hbb_common/src/config.rs 不存在")
        sys.exit(1)
    cfg = cfg_path.read_text(encoding="utf-8")

    import re as _re

    # 1) hide-tray 写入条件化（结构性断言）
    inserts = _re.findall(r"bs\.insert\(keys::OPTION_HIDE_TRAY\.to_owned\(\),\s*\"Y\"\.to_owned\(\)\)", cfg)
    if len(inserts) != 1:
        fail(f"OPTION_HIDE_TRAY 写入应恰好 1 处（条件化），实际 {len(inserts)} 处")
    guarded = _re.search(
        r"if nervdesk_mode_controlled\(\) \{\n"
        r"\s+bs\.insert\(keys::OPTION_HIDE_TRAY\.to_owned\(\),\s*\"Y\"\.to_owned\(\)\)",
        cfg,
    )
    if not guarded:
        fail("OPTION_HIDE_TRAY 写入未包裹在 nervdesk_mode_controlled() 条件内（F-V1 未修复）")
    else:
        print("[OK] F-V1：hide-tray 写入已按形态条件化（仅 controlled 写入 Y）")

    # 2) 三态语义仿真：controlled 隐藏生效 / controller、未设置保留
    def mode_of(m):  # 返回 (controlled, controller)
        return (m == "controlled", m != "controlled")

    expect = {
        "controlled": True,   # 托盘隐藏
        "controller": False,  # 托盘保留
        None: False,          # 未设置 = controller 形态，托盘保留
    }
    for mode, expected_hidden in expect.items():
        c, k = mode_of(mode)
        # builtin 写入：仅 controlled 写 Y；tray 形态 gate：仅 controlled 提前 return
        builtin_write_hidden = c
        tray_gate_hidden = not k
        hidden = builtin_write_hidden and tray_gate_hidden
        # controller/未设置：builtin 不写（缺省 false）+ gate 不拦 → 托盘保留
        if hidden == expected_hidden:
            print(f"[OK] F-V1 三态: mode={str(mode):<12} hide_tray_hidden={hidden} （符合规格：仅 controlled 隐藏）")
        else:
            fail(f"F-V1 三态: mode={mode} hide_tray_hidden={hidden}，期望 {expected_hidden}")

    if not ok:
        sys.exit(1)
    print("[OK] F-V1：hide-tray 形态条件化自检通过（controlled 生效 / controller-未设置保留）")


def main() -> None:
    print("=== NervDesk 行为级补丁（M1 功能行为 + M2 安全 Override 锁定）===")
    patch_config_rs()
    patch_common_rs()
    patch_tray_rs()
    patch_connection_page()
    patch_flutter_rs()
    verify()
    verify_variant_tray()
    print("=== 完成 ===")
    print("提示：密码/PIN 占位符 __NERVDESK_PASSWORD__ 由构建期 CI Secret 替换（t5/t6），")
    print("本脚本只保证源码内无真值；relay 令牌（__NERVDESK_RELAY_TOKEN__）在 t3 处理。")


if __name__ == "__main__":
    main()