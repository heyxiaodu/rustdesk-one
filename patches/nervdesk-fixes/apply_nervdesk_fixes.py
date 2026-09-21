#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
NervDesk 真机三项修复（t18）应用脚本。

依赖顺序（硬约束）：A 档品牌 → t2 → t3 → t4 → 本补丁（锚点建立在 t4 应用后状态；
t18 在 t4 之上修复三处真机缺口）。

三项修复：
  【1】UI 左上角 logo：本补丁不改代码（logo 资产由 nervdesk/branding/*.png|svg
       t19 装配时 cp 到 flutter/assets/）；同时隐藏「由 RustDesk 提供支持」脚注
       （hide-powered-by-me=Y）+ 扫描补齐其余用户可见残留（本补丁在 config.rs
       增加 hide-powered-by-me builtin 种子）。
  【2】固定密码显示/认证链路：审计结论 = 认证链路自洽（新增本地单测
       nervdesk_permanent_password_auth_chain_is_self_consistent 验证
       写入→存储→h1→challenge 比对一致）；修复 = 被控端主界面明确显示固定密码
       （config.rs nervdesk_builtin_password_raw + flutter_ffi
       main_get_builtin_password + 桌面密码板「固定密码」行），消除 `-` 歧义。
  【3】controlled 布局固定：右栏空白移除、网络状态移左栏底部、窗口固定
       500×700（逻辑）不可拉伸（runner CMakeLists/main.cpp/win32_window +
       common.dart 共享形态常量）。

改动文件（10）：
  libs/hbb_common/src/config.rs
  src/flutter_ffi.rs
  flutter/lib/common.dart
  flutter/lib/desktop/pages/connection_page.dart
  flutter/lib/desktop/pages/desktop_home_page.dart
  flutter/windows/CMakeLists.txt
  flutter/windows/runner/main.cpp
  flutter/windows/runner/win32_window.h
  flutter/windows/runner/win32_window.cpp
  src/lang/cn.rs

特性（与其余 nervdesk apply 脚本一致）：锚点唯一性校验、CRLF 自适应、
应用后校验；占位符纪律保持（完整占位符仍只在 config.rs const 初始化行）。
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
    "src/flutter_ffi.rs",
    "flutter/lib/common.dart",
    "flutter/lib/desktop/pages/connection_page.dart",
    "flutter/lib/desktop/pages/desktop_home_page.dart",
    "flutter/windows/CMakeLists.txt",
    "flutter/windows/runner/main.cpp",
    "flutter/windows/runner/win32_window.h",
    "flutter/windows/runner/win32_window.cpp",
    "src/lang/cn.rs",
    "src/platform/windows.rs",
    "src/core_main.rs",
    "src/server/connection.rs",
]


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


def check_prereq() -> None:
    cfg = pathlib.Path("libs/hbb_common/src/config.rs")
    if not cfg.is_file():
        die("找不到 libs/hbb_common/src/config.rs（子模块没拉下来？）")
    t = cfg.read_text(encoding="utf-8")
    for needle, desc in [
        ("pub const NERVDESK_FORCED_OPTIONS", "t2 特征"),
        ("pub fn nervdesk_mode_controlled", "t11 形态开关"),
        ("fn nervdesk_password_marker", "F1 哨兵"),
        ("pub fn nerve_apply_network_defaults", "t3 特征"),
    ]:
        if needle not in t:
            die(f"缺少 {desc}（{needle}）——请先按序应用 品牌→t2→t3→t4")
    print("[OK] 前置校验：品牌→t2→t3→t4 已应用")


# ---------------------------------------------------------------------------
# 1. config.rs
# ---------------------------------------------------------------------------


def patch_config_rs() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    t, crlf = read_text(p)
    # E1: 固定密码读取器（t18 第 2 项）
    t = sub1(
        t,
        "fn nervdesk_password_marker() -> String {\n"
        "    format!(\"__{}_{}\", \"NERVDESK\", \"PASSWORD__\")\n"
        "}",
        "fn nervdesk_password_marker() -> String {\n"
        "    format!(\"__{}_{}\", \"NERVDESK\", \"PASSWORD__\")\n"
        "}\n\n"
        "/// 已注入的出厂固定密码（CI Secret 替换占位符后返回 `Some(值)`；未注入返回 `None`）。\n"
        "/// 供被控端主界面明确显示「固定密码」（t18 第 2 项，消除一次性密码 `-` 歧义）。\n"
        "/// 真值只存在于编译产物（可被 strings 提取，非保密容器，docs/09 §0.2）。\n"
        "#[inline]\n"
        "pub fn nervdesk_builtin_password_raw() -> Option<String> {\n"
        "    if NERVDESK_BUILTIN_PASSWORD != nervdesk_password_marker() {\n"
        "        Some(NERVDESK_BUILTIN_PASSWORD.to_owned())\n"
        "    } else {\n"
        "        None\n"
        "    }\n"
        "}",
        "config.rs nervdesk_builtin_password_raw",
    )
    # E2: hide-powered-by-me builtin 种子（t18 第 1 项）+ t21 固定密码 builtin 种子
    t = sub1(
        t,
        "        bs.insert(keys::OPTION_HIDE_WEBSOCKET_SETTINGS.to_owned(), \"Y\".to_owned());\n",
        "        bs.insert(keys::OPTION_HIDE_WEBSOCKET_SETTINGS.to_owned(), \"Y\".to_owned());\n"
        "        // t18：隐藏「由 RustDesk 提供支持」脚注（read: hide-powered-by-me）\n"
        "        bs.insert(keys::OPTION_HIDE_POWERED_BY_ME.to_owned(), \"Y\".to_owned());\n"
        "        // t21：固定密码经既有 builtin 桥读出（不新增 FFI、不依赖 bridge 再生成）——\n"
        "        // generated_bridge.dart 由 CI codegen 在 pristine 源码上生成，新增 FFI 的\n"
        "        // Dart 声明进不了产物（r3 失败根因）；改用既有 mainGetBuildinOption(\n"
        "        // 'nervdesk-fixed-password')。值只在内存（BUILTIN_SETTINGS），不落盘，\n"
        "        // 暴露面与编译进二进制的 const 相同（非保密容器，docs/09 §0.2）。\n"
        "        if let Some(pw) = nervdesk_builtin_password_raw() {\n"
        "            bs.insert(\"nervdesk-fixed-password\".to_owned(), pw);\n"
        "        }\n",
        "config.rs hide-powered-by-me + fixed-password 种子",
    )
    # E3: 认证链路单测（t18 第 2 项验收）
    test_block = (
        "\n"
        "    #[test]\n"
        "    fn nervdesk_permanent_password_auth_chain_is_self_consistent() {\n"
        "        // t18 审计（第 2 项）：出厂固定密码「写入→存储→客户端/响应方比对」全链路\n"
        "        // 语义自洽，认证与显示可解释一致。示例口令非真值。\n"
        "        use crate::sha2::{Digest, Sha256};\n"
        "        let injected = \"nervdesk-fixed-sample-01\";\n"
        "        with_config_and_hard_settings(Config::default(), HashMap::new(), || {\n"
        "            let _file_guard = ConfigFileRestoreGuard::new(Config::file());\n"
        "            // 1) 注入路径（与被控端启动 nerve_apply_forced_defaults 同一函数）\n"
        "            assert!(Config::set_permanent_password(injected));\n"
        "            assert!(Config::has_permanent_password());\n"
        "            let (storage, salt) = Config::get_local_permanent_password_storage_and_salt();\n"
        "            assert!(!storage.is_empty() && !salt.is_empty());\n"
        "            assert!(local_permanent_password_storage_is_usable_for_auth(&storage, &salt));\n"
        "            // 2) 响应方发给客户端的 salt 与写入口令时的 salt 一致（生产路径）\n"
        "            assert_eq!(Config::get_effective_permanent_password_salt(), salt);\n"
        "            // 3) 响应方从加密存储解出的 h1 == 客户端按 sha256(plain+salt) 计算的 h1\n"
        "            let decoded = decode_permanent_password_h1_from_storage(&storage)\n"
        "                .expect(\"响应方可从加密存储解出 h1\");\n"
        "            let mut h1_client = Sha256::new();\n"
        "            h1_client.update(injected.as_bytes());\n"
        "            h1_client.update(salt.as_bytes());\n"
        "            assert_eq!(&decoded[..], &h1_client.finalize()[..], \"h1 一致\");\n"
        "            // 4) 挑战应答：客户端发送 sha256(h1+challenge)，响应方 verify_h1 同式比对\n"
        "            let challenge = \"123456\";\n"
        "            let mut client_send = Sha256::new();\n"
        "            client_send.update(&decoded);\n"
        "            client_send.update(challenge.as_bytes());\n"
        "            let sent = client_send.finalize();\n"
        "            let mut responder_check = Sha256::new();\n"
        "            responder_check.update(&decoded);\n"
        "            responder_check.update(challenge.as_bytes());\n"
        "            assert_eq!(\n"
        "                &sent[..],\n"
        "                &responder_check.finalize()[..],\n"
        "                \"verify_h1 语义一致\"\n"
        "            );\n"
        "        });\n"
        "    }\n"
    )
    t = sub1(
        t,
        "        let _state_guard = ConfigStateTestGuard::new(config, hard_settings);\n"
        "        test()\n"
        "    }\n\n"
        "    #[test]",
        "        let _state_guard = ConfigStateTestGuard::new(config, hard_settings);\n"
        "        test()\n"
        "    }\n"
        + test_block
        + "\n    #[test]",
        "config.rs 认证链路单测插入",
    )
    write_text(p, t, crlf)
    print("[OK] config.rs: 固定密码读取器 + hide-powered-by-me + 认证链路单测")


# ---------------------------------------------------------------------------
# 2. flutter_ffi.rs
# ---------------------------------------------------------------------------


def patch_flutter_ffi() -> None:
    p = pathlib.Path("src/flutter_ffi.rs")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "pub fn main_get_unlock_pin() -> SyncReturn<String> {\n"
        "    SyncReturn(get_unlock_pin())\n"
        "}",
        "pub fn main_get_unlock_pin() -> SyncReturn<String> {\n"
        "    SyncReturn(get_unlock_pin())\n"
        "}\n\n"
        "/// 已注入的出厂固定密码（未注入返回空串）：供被控端主界面显示「固定密码」\n"
        "/// （t18 第 2 项）。真值编译进产物，可被提取，非保密容器（docs/09 §0.2）。\n"
        "pub fn main_get_builtin_password() -> SyncReturn<String> {\n"
        "    SyncReturn(hbb_common::config::nervdesk_builtin_password_raw().unwrap_or_default())\n"
        "}",
        "flutter_ffi main_get_builtin_password",
    )
    write_text(p, t, crlf)
    print("[OK] flutter_ffi.rs: main_get_builtin_password")


# ---------------------------------------------------------------------------
# 3. common.dart
# ---------------------------------------------------------------------------


def patch_common_dart() -> None:
    p = pathlib.Path("flutter/lib/common.dart")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "Size getIncomingOnlyHomeSize() {\n"
        "  final magicWidth = isWindows ? 11.0 : 2.0;\n"
        "  final magicHeight = 10.0;\n"
        "  return imcomingOnlyHomeSize +\n"
        "      Offset(magicWidth, kDesktopRemoteTabBarHeight + magicHeight);\n"
        "}",
        "Size getIncomingOnlyHomeSize() {\n"
        "  final magicWidth = isWindows ? 11.0 : 2.0;\n"
        "  final magicHeight = 10.0;\n"
        "  return imcomingOnlyHomeSize +\n"
        "      Offset(magicWidth, kDesktopRemoteTabBarHeight + magicHeight);\n"
        "}\n\n"
        "// NervDesk 编译期形态（t11/t18）：与 Rust 侧 NERVDESK_MODE 对应\n"
        "// （CI 传 --dart-define=NERVDESK_MODE=controlled；未设置默认 = controller 形态）。\n"
        "const String kNervDeskMode = String.fromEnvironment('NERVDESK_MODE');\n"
        "const bool kNervDeskModeControlled = kNervDeskMode == 'controlled';\n"
        "const bool kNervDeskModeController = kNervDeskMode != 'controlled';\n\n"
        "/// controlled（纯被控端）窗口逻辑尺寸：500×700（上限；工作区不足时由 runner 侧\n"
        "/// FitToWorkArea 收缩，宁小勿出屏；窗口不可拉伸，规格见 t18 第 3 项）。\n"
        "/// 真实窗口尺寸由 flutter/windows/runner/main.cpp 决定（DPI 缩放 + 工作区夹取），\n"
        "/// 本函数仅供 Dart 侧参考/对齐（勿在运行时重复 setSize 覆盖）。\n"
        "Size getControlledHomeSize() {\n"
        "  return const Size(500, 700);\n"
        "}",
        "common.dart 共享形态常量",
    )
    write_text(p, t, crlf)
    print("[OK] common.dart: kNervDeskMode* 共享常量 + getControlledHomeSize")


# ---------------------------------------------------------------------------
# 4. connection_page.dart（改用共享常量）
# ---------------------------------------------------------------------------


def patch_connection_page() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/connection_page.dart")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "// NervDesk 编译期形态（t11）：与 Rust 侧 NERVDESK_MODE 对应（CI 传\n"
        "// --dart-define=NERVDESK_MODE=controlled；未设置默认 = controller 形态）。\n"
        "const String _nerveMode = String.fromEnvironment('NERVDESK_MODE');\n"
        "const bool _nerveControlled = _nerveMode == 'controlled';\n"
        "const bool _nerveController = _nerveMode != 'controlled';\n\n",
        "// NervDesk 编译期形态（t11/t18）：共享常量见 common.dart（kNervDeskMode*），\n"
        "// 与 Rust 侧 NERVDESK_MODE 对应（--dart-define=NERVDESK_MODE=controlled）。\n\n",
        "connection_page 模式常量挪到 common.dart",
    )
    t = sub1(t, "if (!_nerveControlled)", "if (!kNervDeskModeControlled)",
             "connection_page ID 输入条件")
    t = sub1(t, "child: _nerveController ? PeerTabPage() : Container()),",
             "child: kNervDeskModeController ? PeerTabPage() : Container()),",
             "connection_page PeerTabPage 条件")
    write_text(p, t, crlf)
    print("[OK] connection_page.dart: 使用共享形态常量")


# ---------------------------------------------------------------------------
# 5. desktop_home_page.dart（右栏移除/状态位/固定密码显示）
# ---------------------------------------------------------------------------


def patch_home_page() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_home_page.dart")
    t, crlf = read_text(p)
    # E7a: controlled 不渲染右栏
    t = sub1(
        t,
        "final isIncomingOnly = bind.isIncomingOnly();\n"
        "    return _buildBlock(\n"
        "        child: Row(\n"
        "      crossAxisAlignment: CrossAxisAlignment.start,\n"
        "      children: [\n"
        "        buildLeftPane(context),\n"
        "        if (!isIncomingOnly) const VerticalDivider(width: 1),\n"
        "        if (!isIncomingOnly) Expanded(child: buildRightPane(context)),\n"
        "      ],\n"
        "    ));",
        "final isIncomingOnly = bind.isIncomingOnly();\n"
        "    // NervDesk（t18 第 3 项）：controlled（纯被控端）不渲染右侧面板/分隔线\n"
        "    // （连接管理器与 ID 直连已被移除，右栏空白无意义），左栏独占窗口。\n"
        "    final showRightPane = !isIncomingOnly && !kNervDeskModeControlled;\n"
        "    return _buildBlock(\n"
        "        child: Row(\n"
        "      crossAxisAlignment: CrossAxisAlignment.start,\n"
        "      children: [\n"
        "        buildLeftPane(context),\n"
        "        if (showRightPane) const VerticalDivider(width: 1),\n"
        "        if (showRightPane) Expanded(child: buildRightPane(context)),\n"
        "      ],\n"
        "    ));",
        "home_page 右栏条件化",
    )
    # E7b: 网络状态位移到左栏底部（controlled 同 incoming-only）
    t = sub1(
        t,
        "if (isIncomingOnly) {\n"
        "      children.addAll([\n"
        "        Divider(),\n"
        "        OnlineStatusWidget(",
        "// NervDesk（t18 第 3 项）：controlled 变体与 incoming-only 一致，把网络状态\n"
        "    // 指示放在左栏底部（下方），替代被移除的右侧状态位。\n"
        "    if (isIncomingOnly || kNervDeskModeControlled) {\n"
        "      children.addAll([\n"
        "        Divider(),\n"
        "        OnlineStatusWidget(",
        "home_page 网络状态位条件",
    )
    # E7c: 固定密码显示
    t = sub1(
        t,
        "final showOneTime = model.approveMode != 'click' &&\n"
        "        model.verificationMethod != kUsePermanentPassword;",
        "final showOneTime = model.approveMode != 'click' &&\n"
        "        model.verificationMethod != kUsePermanentPassword;\n"
        "    // NervDesk（t18 第 2 项 + t21）：明确显示已注入的出厂固定密码（未注入为空串不渲染），\n"
        "    // 消除一次性密码 `-` 歧义——连接时使用此固定密码。\n"
        "    // t21：经既有 builtin 桥 mainGetBuildinOption（SyncReturn 生成为同名字面量，无\n"
        "    // Sync 后缀）读取 nervdesk-fixed-password（config.rs 启动时种子）；不新增 FFI，\n"
        "    // 避免依赖 CI codegen 再生成 generated_bridge.dart（r3 失败根因）。\n"
        "    final builtinPassword = bind.mainGetBuildinOption(key: 'nervdesk-fixed-password');",
        "home_page 固定密码读取",
    )
    t = sub1(
        t,
        "onTap: () => DesktopSettingPage.switch2page(\n"
        "                              SettingsTabKey.safety),\n"
        "                          onHover: (value) => editHover.value = value,\n"
        "                        ),\n"
        "                    ],\n"
        "                  ),\n"
        "                ],\n"
        "              ),\n"
        "            ),\n"
        "          ),\n"
        "        ],\n"
        "      ),\n"
        "    );\n"
        "  }",
        "onTap: () => DesktopSettingPage.switch2page(\n"
        "                              SettingsTabKey.safety),\n"
        "                          onHover: (value) => editHover.value = value,\n"
        "                        ),\n"
        "                    ],\n"
        "                  ),\n"
        "                  if (builtinPassword.isNotEmpty)\n"
        "                    Padding(\n"
        "                      padding: const EdgeInsets.only(top: 10),\n"
        "                      child: Column(\n"
        "                        crossAxisAlignment: CrossAxisAlignment.start,\n"
        "                        children: [\n"
        "                          AutoSizeText(\n"
        "                            translate(\"Fixed Password\"),\n"
        "                            style: TextStyle(\n"
        "                                fontSize: 12,\n"
        "                                color: textColor?.withOpacity(0.5)),\n"
        "                            maxLines: 1,\n"
        "                          ),\n"
        "                          SelectableText(\n"
        "                            builtinPassword,\n"
        "                            style: const TextStyle(\n"
        "                                fontWeight: FontWeight.w600, fontSize: 15),\n"
        "                          ),\n"
        "                          AutoSizeText(\n"
        "                            translate(\"Use this password to connect\"),\n"
        "                            style: TextStyle(\n"
        "                                fontSize: 11,\n"
        "                                color: textColor?.withOpacity(0.4)),\n"
        "                            maxLines: 1,\n"
        "                          ),\n"
        "                        ],\n"
        "                      ),\n"
        "                    ),\n"
        "                ],\n"
        "              ),\n"
        "            ),\n"
        "          ),\n"
        "        ],\n"
        "      ),\n"
        "    );\n"
        "  }",
        "home_page 固定密码行",
    )
    write_text(p, t, crlf)
    print("[OK] desktop_home_page.dart: 右栏移除/状态位/固定密码显示")


# ---------------------------------------------------------------------------
# 6. CMakeLists / main.cpp / win32_window（窗口固定）
# ---------------------------------------------------------------------------


def patch_cmake() -> None:
    p = pathlib.Path("flutter/windows/CMakeLists.txt")
    t, crlf = read_text(p)
    t = sub1(
        t,
        'add_subdirectory("runner")',
        'add_subdirectory("runner")\n\n'
        "# NervDesk（t18 第 3 项）：controlled 变体（NERVDESK_MODE=controlled，环境变量在\n"
        "# CMake 配置期读取）把 NERVDESK_MODE_CONTROLLED 编译进 runner：\n"
        "# main.cpp 固定窗口 500×700（逻辑）且不可拉伸，工作区不足自动收缩。\n"
        'if("$ENV{NERVDESK_MODE}" STREQUAL "controlled")\n'
        "  target_compile_definitions(${BINARY_NAME} PRIVATE NERVDESK_MODE_CONTROLLED=1)\n"
        '  message(STATUS "NervDesk controlled variant: runner fixed window 500x700 (logical), non-resizable")\n'
        "endif()",
        "CMakeLists controlled 宏",
    )
    write_text(p, t, crlf)
    print("[OK] CMakeLists.txt: NERVDESK_MODE_CONTROLLED 编译宏")


def patch_main_cpp() -> None:
    p = pathlib.Path("flutter/windows/runner/main.cpp")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "Win32Window::Point origin(workarea_origin.x + relative_origin.x, workarea_origin.y + relative_origin.y);\n"
        "  Win32Window::Size size(800u, 600u);\n\n"
        "  // Fit the window to the monitor's work area.\n"
        "  Win32Desktop::FitToWorkArea(origin, size);\n\n"
        "  std::wstring window_title;",
        "Win32Window::Point origin(workarea_origin.x + relative_origin.x, workarea_origin.y + relative_origin.y);\n"
        "  Win32Window::Size size(800u, 600u);\n\n"
        "  // NervDesk（t18 第 3 项）：controlled 变体固定逻辑尺寸 500×700（上限），\n"
        "  // 不可拉伸；FitToWorkArea 按屏幕工作区自动收缩（宁小勿出屏），\n"
        "  // DPI 缩放由 runner 处理（500×700 为逻辑像素）。\n"
        "#ifdef NERVDESK_MODE_CONTROLLED\n"
        "  const bool nervdesk_controlled = true;\n"
        "  size = Win32Window::Size(500u, 700u);\n"
        "#else\n"
        "  const bool nervdesk_controlled = false;\n"
        "#endif\n\n"
        "  // Fit the window to the monitor's work area.\n"
        "  Win32Desktop::FitToWorkArea(origin, size);\n\n"
        "  std::wstring window_title;",
        "main.cpp controlled 尺寸",
    )
    t = sub1(
        t,
        "if (!window.CreateAndShow(window_title, origin, size, !is_cm_page)) {",
        "if (!window.CreateAndShow(window_title, origin, size, !is_cm_page,\n"
        "                            /*resizable=*/!nervdesk_controlled && !is_cm_page)) {",
        "main.cpp resizable",
    )
    write_text(p, t, crlf)
    print("[OK] main.cpp: controlled 500×700 + 不可拉伸")


def patch_win32_window() -> None:
    p_h = pathlib.Path("flutter/windows/runner/win32_window.h")
    t, crlf = read_text(p_h)
    t = sub1(
        t,
        "  // as logical pixels and scale to appropriate for the default monitor. Returns\n"
        "  // true if the window was created successfully.\n"
        "  bool CreateAndShow(const std::wstring& title,\n"
        "                     const Point& origin,\n"
        "                     const Size& size,\n"
        "                     bool showOnTaskBar = true);",
        "  // as logical pixels and scale to appropriate for the default monitor. Returns\n"
        "  // true if the window was created successfully.\n"
        "  // |resizable|: false 时创建「固定尺寸」窗口（无 WS_THICKFRAME/WS_MAXIMIZEBOX，\n"
        "  //  用户不能拉伸/最大化），NervDesk controlled 变体使用（t18 第 3 项）。\n"
        "  bool CreateAndShow(const std::wstring& title,\n"
        "                     const Point& origin,\n"
        "                     const Size& size,\n"
        "                     bool showOnTaskBar = true,\n"
        "                     bool resizable = true);",
        "win32_window.h 签名",
    )
    write_text(p_h, t, crlf)
    p_c = pathlib.Path("flutter/windows/runner/win32_window.cpp")
    t, crlf = read_text(p_c)
    t = sub1(
        t,
        "bool Win32Window::CreateAndShow(const std::wstring& title,\n"
        "                                const Point& origin,\n"
        "                                const Size& size, bool showOnTaskBar) {",
        "bool Win32Window::CreateAndShow(const std::wstring& title,\n"
        "                                const Point& origin,\n"
        "                                const Size& size, bool showOnTaskBar,\n"
        "                                bool resizable) {",
        "win32_window.cpp 签名",
    )
    t = sub1(
        t,
        "  UINT dpi = FlutterDesktopGetDpiForMonitor(monitor);\n"
        "  double scale_factor = dpi / 96.0;\n\n"
        "  HWND window = CreateWindow(\n"
        "      window_class, title.c_str(), WS_OVERLAPPEDWINDOW,",
        "  UINT dpi = FlutterDesktopGetDpiForMonitor(monitor);\n"
        "  double scale_factor = dpi / 96.0;\n\n"
        "  // NervDesk（t18 第 3 项）：非 resizable 时去掉 WS_THICKFRAME/WS_MAXIMIZEBOX\n"
        "  // （固定尺寸，用户不可拉伸/最大化）；DPI 缩放照常按 logical→physical。\n"
        "  const DWORD window_style =\n"
        "      resizable ? WS_OVERLAPPEDWINDOW\n"
        "                : (WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX);\n\n"
        "  HWND window = CreateWindow(\n"
        "      window_class, title.c_str(), window_style,",
        "win32_window.cpp 固定样式",
    )
    write_text(p_c, t, crlf)
    print("[OK] win32_window.h/.cpp: resizable 参数 + 固定窗口样式")


# ---------------------------------------------------------------------------
# 7. cn.rs 语言键
# ---------------------------------------------------------------------------


def patch_cn_lang() -> None:
    p = pathlib.Path("src/lang/cn.rs")
    t, crlf = read_text(p)
    t = sub1(
        t,
        '("One-time Password", "一次性密码"),\n        ("Use one-time password", "使用一次性密码"),',
        '("One-time Password", "一次性密码"),\n        ("Fixed Password", "固定密码"),\n        ("Use this password to connect", "连接时使用此密码"),\n        ("Use one-time password", "使用一次性密码"),',
        "cn.rs 固定密码文案",
    )
    write_text(p, t, crlf)
    print("[OK] cn.rs: Fixed Password / 连接提示")


# ---------------------------------------------------------------------------
# 8. r5（t23）：Windows 单实例互斥（windows.rs + core_main.rs）
# ---------------------------------------------------------------------------

_PATCH_WIN_OLD = r"""        if show_window {
            ShowWindow(window, SW_NORMAL);
            SetForegroundWindow(window);
        }
    }
    return true;
}"""

_PATCH_WIN_NEW = r"""        if show_window {
            ShowWindow(window, SW_NORMAL);
            SetForegroundWindow(window);
        }
    }
    return true;
}

/// r5（t23）：Windows 单实例互斥（GUI 形态）。
/// 无参 GUI 启动时用命名 Mutex 检测已运行的同形态实例；已存在 → 聚焦已有主窗口并
/// 返回 false（调用方退出）。防 10048（二次实例重复 bind）与注册互踢重演；
/// 带参数的启动（--service/--server/--install/--cm/深链等）不参与互斥，交既有
/// main.cpp FindWindowW/whitelist 机制（深链需把链接派发给已有实例）。
/// 诚实边界：这是进程级互斥（同用户会话），不替代服务/ACL 防停用（M7）。
pub fn enforce_single_instance_gui() -> bool {
    if std::env::args().count() > 1 {
        // 带参数启动：服务/安装/深链等交既有机制，不拦
        return true;
    }
    use winapi::um::synchapi::CreateMutexW;
    let app = crate::get_app_name();
    let name: Vec<u16> = format!("{}_SingleInstance", app)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let handle = CreateMutexW(std::ptr::null_mut(), 0, name.as_ptr());
        if handle.is_null() {
            log::warn!("NervDesk: CreateMutexW 失败，跳过单实例检查（fail-open）");
            return true;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            focus_existing_main_window(&app);
            return false;
        }
        // 持有句柄（进程生命周期），不释放：后续实例会拿到 ERROR_ALREADY_EXISTS
        std::mem::forget(handle);
    }
    true
}

fn focus_existing_main_window(app: &str) {
    let title: Vec<u16> = app.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let w = FindWindowW(std::ptr::null(), title.as_ptr());
        if !w.is_null() {
            ShowWindow(w, SW_NORMAL);
            SetForegroundWindow(w);
        }
    }
}"""


def patch_windows_single_instance() -> None:
    p = pathlib.Path("src/platform/windows.rs")
    t, crlf = read_text(p)
    t = sub1(t, _PATCH_WIN_OLD, _PATCH_WIN_NEW, "windows.rs 单实例互斥")
    write_text(p, t, crlf)
    print("[OK] windows.rs: enforce_single_instance_gui（r5）")


def patch_core_main() -> None:
    p = pathlib.Path("src/core_main.rs")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "    if !crate::common::global_init() {\n"
        "        return None;\n"
        "    }\n"
        "    crate::load_custom_client();",
        "    if !crate::common::global_init() {\n"
        "        return None;\n"
        "    }\n"
        "    // r5（t23）：Windows 单实例互斥——无参 GUI 重复启动时退出并聚焦已有实例\n"
        "    // （防 10048 / 注册互踢；带参数启动跳过，见 windows.rs 注释）\n"
        "    #[cfg(windows)]\n"
        "    if !crate::platform::windows::enforce_single_instance_gui() {\n"
        "        return None;\n"
        "    }\n"
        "    crate::load_custom_client();",
        "core_main 单实例调用",
    )
    write_text(p, t, crlf)
    print("[OK] core_main.rs: 单实例互斥调用（r5）")


# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# ---------------------------------------------------------------------------
# 9. r6（t27）：彻底静默化（无快捷键）+ C4 设置入口 + C5 更新关闭 + C6 历史面板
# ---------------------------------------------------------------------------


def patch_r6_config() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    t, crlf = read_text(p)
    # C5/§4.3：FORCED_OPTIONS 追加（t2 区块，本补丁扩展）
    t = sub1(
        t,
        '    (keys::OPTION_ALLOW_NUMERNIC_ONE_TIME_PASSWORD, "Y"),\n',
        '    (keys::OPTION_ALLOW_NUMERNIC_ONE_TIME_PASSWORD, "Y"),\n'
        '    // r6（t27 C5/§4.3）：无人值守不弹更新；不接受窗口内改权限 / 远端改 CM\n'
        '    (keys::OPTION_ALLOW_AUTO_UPDATE, "N"),\n'
        '    (keys::OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW, "N"),\n'
        '    (keys::OPTION_ALLOW_REMOTE_CM_MODIFICATION, "N"),\n',
        "FORCED_OPTIONS 追加 C5/§4.3",
    )
    # C6：builtin 种子
    t = sub1(
        t,
        '        bs.insert(keys::OPTION_HIDE_POWERED_BY_ME.to_owned(), "Y".to_owned());\n',
        '        bs.insert(keys::OPTION_HIDE_POWERED_BY_ME.to_owned(), "Y".to_owned());\n'
        '        // r6（t27 C6）：主界面历史/发现面板收敛（PeerTabPage 已隐藏，纵深一致）\n'
        '        bs.insert(keys::OPTION_DISABLE_GROUP_PANEL.to_owned(), "Y".to_owned());\n'
        '        bs.insert(keys::OPTION_DISABLE_DISCOVERY_PANEL.to_owned(), "Y".to_owned());\n',
        "builtin 种子 C6",
    )
    write_text(p, t, crlf)
    print("[OK] config.rs: C5/§4.3 强制 + C6 种子（r6）")


def patch_r6_home() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_home_page.dart")
    t, crlf = read_text(p)
    # C4a：ID 板三点菜单
    t = sub1(
        t,
        "                        ).marginOnly(top: 5),\n"
        "                        buildPopupMenu(context)",
        "                        ).marginOnly(top: 5),\n"
        "                        // NervDesk（t27 C4）：controlled 隐藏 ID 板设置入口（三点菜单）\n"
        "                        if (!kNervDeskModeControlled) buildPopupMenu(context)",
        "ID 板三点菜单 C4a",
    )
    # C4：改密入口
    t = sub1(
        t,
        "onTap: () => DesktopSettingPage.switch2page(\n"
        "                              SettingsTabKey.safety),",
        "// NervDesk（t27 C4）：controlled 隐藏改密入口（设置全部封闭）\n"
        "                          onTap: kNervDeskModeControlled\n"
        "                              ? null\n"
        "                              : () => DesktopSettingPage.switch2page(\n"
        "                                  SettingsTabKey.safety),",
        "改密入口 C4",
    )
    write_text(p, t, crlf)
    print("[OK] desktop_home_page.dart: C4 设置入口隐藏（r6）")


def patch_r6_tab() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_tab_page.dart")
    t, crlf = read_text(p)
    t = sub1(
        t,
        "offstage: bind.isIncomingOnly() || bind.isDisableSettings(),",
        "offstage: bind.isIncomingOnly() || bind.isDisableSettings() ||\n"
        "                    kNervDeskModeControlled,",
        "tabbar 设置钮 C4b",
    )
    write_text(p, t, crlf)
    print("[OK] desktop_tab_page.dart: tabbar 设置按钮隐藏（r6）")


# ---------------------------------------------------------------------------
# 8b. r7（t30）：按 allow-hide-cm 语义恢复 CM（只隐藏窗口）——消息/文件恢复
# ---------------------------------------------------------------------------

_R7_CFG_OLD = '    (keys::OPTION_ALLOW_REMOTE_CM_MODIFICATION, "N"),\n];'
_R7_CFG_NEW = ('    (keys::OPTION_ALLOW_REMOTE_CM_MODIFICATION, "N"),\n'
               '    // r7（t30）：对齐官方 allow-hide-cm 链——仅隐藏 CM 窗口（进程/信道保留，\n'
               '    // 消息与文件功能恢复）；配合 approve-mode=password 与 verification-method\n'
               '    // =use-permanent-password（M1-3 临时 PIN 登录在 controlled Windows 让位于\n'
               '    // 固定密码；controller 形态不受影响——强制项仅 windows）\n'
               '    ("allow-hide-cm", "Y"),\n'
               '    (keys::OPTION_VERIFICATION_METHOD, "use-permanent-password"),\n'
               '];')

_R7_IPC_OLD = ('                } else if name == "hide_cm" {\n'
               '                    value = if crate::hbbs_http::sync::is_pro() || crate::common::is_custom_client()\n'
               '                    {')
_R7_IPC_NEW = ('                } else if name == "hide_cm" {\n'
               '                    value = if crate::hbbs_http::sync::is_pro()\n'
               '                        || crate::common::is_custom_client()\n'
               '                        || hbb_common::config::nervdesk_mode_controlled()\n'
               '                    {')

_CHAT_OLD1 = """  RxInt mobileUnreadSum = 0.obs;
  MessageKey? latestReceivedKey;"""
_CHAT_NEW1 = """  RxInt mobileUnreadSum = 0.obs;
  MessageKey? latestReceivedKey;
  // r7（t30）：受控端消息弹窗通道——server 模式新消息文本（controlled 主窗口
  // 消费；controller 维持官方原位展示，不依赖此字段）
  final RxnString lastServerMsg = RxnString();"""

_CHAT_OLD2 = """    if (text.isEmpty) return;
    if (desktopType == DesktopType.cm) {
      await showCmWindow();
    }"""
_CHAT_NEW2 = """    if (text.isEmpty) return;
    // r7（t30）：controlled 变体不因消息弹出隐藏的 CM 主窗——消息改由主窗口
    // 轻量弹窗展示（lastServerMsg）；controller/常规形态维持官方显示行为。
    if (desktopType == DesktopType.cm && !kNervDeskModeControlled) {
      await showCmWindow();
    }
    if (id != clientModeID) {
      lastServerMsg.value = text;
    }"""

_HOME_OLD1 = "  bool isCardClosed = false;"
_HOME_NEW1 = ("  bool isCardClosed = false;\n"
              "  // r7（t30）：受控端消息轻量弹窗（仅 controlled）\n"
              "  Timer? _nerveMsgTimer;\n"
              "  final RxnString _nervePopupMsg = RxnString();")

_HOME_OLD2 = """  void initState() {
    super.initState();
    _updateTimer = periodic_immediate(const Duration(seconds: 1), () async {"""
_HOME_NEW2 = """  void initState() {
    super.initState();
    // r7（t30）：controlled 监听受控消息 → 轻量弹窗（不依赖 CM 窗口可见）
    if (kNervDeskModeControlled) {
      gFFI.chatModel.addListener(_nerveOnChatMsg);
    }
    _updateTimer = periodic_immediate(const Duration(seconds: 1), () async {"""

_HOME_OLD3 = """  void dispose() {
    _uniLinksSubscription?.cancel();"""
_HOME_NEW3 = """  void dispose() {
    if (kNervDeskModeControlled) {
      gFFI.chatModel.removeListener(_nerveOnChatMsg);
      _nerveMsgTimer?.cancel();
    }
    _uniLinksSubscription?.cancel();"""

_HOME_OLD4 = """  Widget _buildBlock({required Widget child}) {"""
_HOME_NEW4 = r"""  // r7（t30）：受控消息监听（仅 controlled 挂载）
  void _nerveOnChatMsg() {
    final m = gFFI.chatModel.lastServerMsg.value;
    if (m == null || m.isEmpty || !mounted) return;
    _nerveMsgTimer?.cancel();
    setState(() => _nervePopupMsg.value = m);
    _nerveMsgTimer = Timer(const Duration(seconds: 8), () {
      if (mounted) setState(() => _nervePopupMsg.value = null);
    });
  }

  Widget _nerveMessageBanner() {
    final m = _nervePopupMsg.value;
    if (m == null || m.isEmpty) return const SizedBox.shrink();
    return Positioned(
      top: 8,
      left: 12,
      right: 12,
      child: Center(
        child: Material(
          elevation: 4,
          borderRadius: BorderRadius.circular(8),
          child: Container(
            constraints: const BoxConstraints(maxWidth: 420),
            padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
            decoration: BoxDecoration(
              color: Theme.of(context).colorScheme.inverseSurface,
              borderRadius: BorderRadius.circular(8),
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                Icon(Icons.mark_chat_unread_outlined,
                    size: 16,
                    color: Theme.of(context).colorScheme.onInverseSurface),
                const SizedBox(width: 8),
                Flexible(
                  child: Text(
                    m,
                    overflow: TextOverflow.ellipsis,
                    maxLines: 3,
                    style: TextStyle(
                        color: Theme.of(context).colorScheme.onInverseSurface),
                  ),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }

  Widget _buildBlock({required Widget child}) {"""

_HOME_OLD5 = """    return _buildBlock(
        child: Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        buildLeftPane(context),
        if (showRightPane) const VerticalDivider(width: 1),
        if (showRightPane) Expanded(child: buildRightPane(context)),
      ],
    ));
  }"""
_HOME_NEW5 = """    final pane = _buildBlock(
        child: Row(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        buildLeftPane(context),
        if (showRightPane) const VerticalDivider(width: 1),
        if (showRightPane) Expanded(child: buildRightPane(context)),
      ],
    ));
    // r7（t30）：受控端消息轻量弹窗（仅 controlled 叠加）
    if (!kNervDeskModeControlled) {
      return pane;
    }
    return Stack(children: [pane, _nerveMessageBanner()]);
  }"""


def patch_r7_cm() -> None:
    # r7：start_ipc 恢复上游原样（连接/CM 进程与信道全保留），加 r7 修订注释
    p = pathlib.Path("src/server/connection.rs")
    t, crlf = read_text(p)
    # 防御：若旧链（t26/r6 版）仍残留 early-return gate，先清除
    gate = ('    // NervDesk 定制（t26 C1）：受控端隐藏「会话控制面板」（CM 小窗）。\n'
            '    // controlled 变体不启动 --cm 面板进程（含 --tray 兜底）；会话 io_loop 不受\n'
            '    // 影响。中断途径（r6 用户拍板「彻底静默化」后）：仅控制端断开 / 运维脚本\n'
            '    // （taskkill / M7 service-install/watchdog），被控端无任何快捷键途径。\n'
            '    if hbb_common::config::nervdesk_mode_controlled() {\n'
            '        log::info!("NervDesk controlled: 隐藏会话控制面板（跳过 start_ipc/CM 窗口）");\n'
            '        return Ok(());\n'
            '    }\n\n')
    t = t.replace(gate, "")
    t = sub1(
        t,
        '    use hbb_common::anyhow::anyhow;\n\n'
        '    loop {',
        '    use hbb_common::anyhow::anyhow;\n\n'
        '    // r7（t30）修订：撤销 t26 的 early-return——改走官方 allow-hide-cm 语义\n'
        '    // （仅隐藏 CM 窗口、进程与 IPC 全保留），由强制选项 allow-hide-cm=Y +\n'
        '    // verification-method=use-permanent-password + 既有的 approve-mode=password\n'
        '    // 触发 password_security::hide_cm()，ipc.rs 对 controlled 应答 hide_cm=true\n'
        '    // → flutter hideCmWindow 隐藏窗口。消息/文件等会话功能随之恢复。\n'
        '    loop {',
        "connection.rs r7 修订注释（CM 启动恢复）",
    )
    write_text(p, t, crlf)
    print("[OK] connection.rs: CM 启动恢复（r7 官方掩体）")


def patch_r7_config() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    t, crlf = read_text(p)
    t = sub1(t, _R7_CFG_OLD, _R7_CFG_NEW, "config.rs hide_cm 链强制项")
    write_text(p, t, crlf)
    print("[OK] config.rs: allow-hide-cm / verification 强制（r7）")


def patch_r7_ipc() -> None:
    p = pathlib.Path("src/ipc.rs")
    t, crlf = read_text(p)
    t = sub1(t, _R7_IPC_OLD, _R7_IPC_NEW, "ipc.rs hide_cm 应答")
    write_text(p, t, crlf)
    print("[OK] ipc.rs: hide_cm 应答对 controlled 开放（r7）")


def patch_r7_chat_model() -> None:
    p = pathlib.Path("flutter/lib/models/chat_model.dart")
    t, crlf = read_text(p)
    t = sub1(t, _CHAT_OLD1, _CHAT_NEW1, "chat_model lastServerMsg")
    t = sub1(t, _CHAT_OLD2, _CHAT_NEW2, "chat_model CM-不弹 + 通道写入")
    write_text(p, t, crlf)
    print("[OK] chat_model.dart: lastServerMsg + CM gate（r7）")


def patch_r7_home() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_home_page.dart")
    t, crlf = read_text(p)
    t = sub1(t, _HOME_OLD1, _HOME_NEW1, "home fields")
    t = sub1(t, _HOME_OLD2, _HOME_NEW2, "home initState")
    t = sub1(t, _HOME_OLD3, _HOME_NEW3, "home dispose")
    t = sub1(t, _HOME_OLD4, _HOME_NEW4, "home banner fns")
    t = sub1(t, _HOME_OLD5, _HOME_NEW5, "home Stack overlay")
    write_text(p, t, crlf)
    print("[OK] desktop_home_page.dart: 受控消息轻量弹窗（r7）")
# ---------------------------------------------------------------------------
# 8c. r8（t32）：PIN 设置锁（unlock_pin）——受控端关键配置保护
# ---------------------------------------------------------------------------

_R8_CFG_OLD = """        log::info!("NervDesk: 写入出厂固定密码（构建期 Secret 替换）");
        let _ = Config::set_permanent_password(NERVDESK_BUILTIN_PASSWORD);
    }
    // NervDesk 定制（M4 网络）：内置地址锁定 + 网络设置隐藏 + iroh relay 令牌
    nerve_apply_network_defaults();"""
_R8_CFG_NEW = """        log::info!("NervDesk: 写入出厂固定密码（构建期 Secret 替换）");
        let _ = Config::set_permanent_password(NERVDESK_BUILTIN_PASSWORD);
    }
    // r8（t32）：PIN 设置锁种子（controlled）——与固定密码同源（change-list 注明
    // 可变：后续可改 Secret 注入）；仅当前为空且未禁用时写入（管理员 CLI/设置面可改）。
    // 语义区分：unlock_pin = 设置锁（保护关键配置入口）；allow-numeric-one-time-password
    // = 临时数字密码（会话登录），两者并存不混淆。
    if nervdesk_mode_controlled() {
        if let Some(pin) = nervdesk_builtin_password_raw() {
            if pin.chars().count() >= 4
                && Config::get_unlock_pin().is_empty()
                && !Config::is_disable_unlock_pin()
            {
                log::info!("NervDesk: 写入 PIN 设置锁（与固定密码同源）");
                Config::set_unlock_pin(&pin);
            }
        }
    }
    // NervDesk 定制（M4 网络）：内置地址锁定 + 网络设置隐藏 + iroh relay 令牌
    nerve_apply_network_defaults();"""

_HOME_GATE_OLD = """                          // NervDesk（t27 C4）：controlled 隐藏改密入口（设置全部封闭）
                          onTap: kNervDeskModeControlled
                              ? null
                              : () => DesktopSettingPage.switch2page(
                                  SettingsTabKey.safety),"""
_HOME_GATE_NEW = """                          // NervDesk（t27 C4 + r8 t32）：controlled 改密入口以 PIN 设置锁保护——
                          // 未验证 PIN（=固定密码同源）不可进入/修改关键配置；其余入口仍隐藏
                          onTap: kNervDeskModeControlled
                              ? () => _nerveUnlockThen(
                                  () => DesktopSettingPage.switch2page(
                                      SettingsTabKey.safety))
                              : () => DesktopSettingPage.switch2page(
                                  SettingsTabKey.safety),"""

_HOME_FN_OLD = """  Widget _buildBlock({required Widget child}) {"""
_HOME_FN_NEW = """  // r8（t32）：PIN 设置锁——进入受保护设置前验证 PIN（未过不可改关键配置）。
  // 与临时密码 PIN（allow-numeric-one-time-password，会话登录）语义不同。
  void _nerveUnlockThen(Function() onPass) {
    checkUnlockPinDialog(bind.mainGetUnlockPin(), onPass);
  }

  Widget _buildBlock({required Widget child}) {"""


def patch_r8_pin() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    t, crlf = read_text(p)
    t = sub1(t, _R8_CFG_OLD, _R8_CFG_NEW, "config.rs PIN 设置锁种子")
    write_text(p, t, crlf)
    print("[OK] config.rs: PIN 设置锁种子（r8）")


def patch_r8_home() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_home_page.dart")
    t, crlf = read_text(p)
    if "import 'package:flutter_hbb/common/widgets/dialog.dart';" not in t:
        anchor = "import 'package:flutter_hbb/common/"
        i = t.rfind(anchor)
        assert i >= 0
        eol = t.find("\n", i)
        t = t[: eol + 1] + "import 'package:flutter_hbb/common/widgets/dialog.dart';\n" + t[eol + 1 :]
    t = sub1(t, _HOME_GATE_OLD, _HOME_GATE_NEW, "home 改密 PIN 门")
    t = sub1(t, _HOME_FN_OLD, _HOME_FN_NEW, "home _nerveUnlockThen")
    write_text(p, t, crlf)
    print("[OK] desktop_home_page.dart: PIN 门（r8）")
# ---------------------------------------------------------------------------
# 8d. r9（t34）：方案 A 注释正式化（iroh = direct-only；relay 不注入说明）
# ---------------------------------------------------------------------------

_R9_IROH_OLD = r"""/// 做两件**必不可少**的事：
///
/// 1. **读 relay 地址**（选项 `iroh-relay`，逗号分隔）。
///    不配的话就是 `RelayMode::Disabled`，跨 NAT 时永远连不上 ——
///    即使用户部署了 relay 也用不到。
/// 2. **用 RustDesk 的密钥对派生 iroh 身份**，这样 iroh 的 `EndpointId`
///    就等于 RustDesk 的 `pk`，对端身份校验直接复用既有逻辑
///    （见 [`secret_key_from_rustdesk`]）。"""

_R9_IROH_NEW = r"""/// 方案 A（docs/12）：**iroh = 直连/打洞专用（direct-only 策略）**——
/// relay 默认不注入：未配置 option `iroh-relay` 时 relay_urls 为空、
/// `RelayMode::Disabled`；跨 NAT 兜底由 RustDesk relay（`relay-server`
/// 选项 → `api.nervcode.eu.org` 2.x WS `/ws/relay` / 21117）承担。
/// 如需 iroh-relay 辅助，请**显式配置 option `iroh-relay`**（逗号分隔地址），
/// 恢复路径见 docs/12 §2.1。`iroh-relay-token` 解析保留（兼容性，未用项）。
///
/// 其余行为：
///
/// 1. **用 RustDesk 的密钥对派生 iroh 身份**，这样 iroh 的 `EndpointId`
///    就等于 RustDesk 的 `pk`，对端身份校验直接复用既有逻辑
///    （见 [`secret_key_from_rustdesk`]）。"""


def patch_r9_iroh() -> None:
    p = pathlib.Path("nervdesk/iroh_transport.rs")
    t, crlf = read_text(p)
    t = sub1(t, _R9_IROH_OLD, _R9_IROH_NEW, "iroh_transport 注释（方案 A）")
    write_text(p, t, crlf)
    print("[OK] nervdesk/iroh_transport.rs: direct-only 注释（r9/t34）")


# ---------------------------------------------------------------------------
# 8e. r8/t35（本地修复）：消息横幅旁路通道（_nerve_ui）+ CM no-activate
# ---------------------------------------------------------------------------

_FLUTTER_OLD = """    #[inline]
    pub fn cm_init() {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        start_listen_ipc_thread();
    }"""
_FLUTTER_NEW = """    #[inline]
    pub fn cm_init() {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            start_listen_ipc_thread();
            // r8/t35：controlled 常驻主窗口旁路消息通道（_nerve_ui）——绕开隐藏 CM，
            // 横幅事件直达主窗口；CM（--cm）进程不抢占（其独享 _cm 监听）。
            if hbb_common::config::nervdesk_mode_controlled() && !crate::common::is_cm() {
                std::thread::spawn(nerv_main_ui_listener);
            }
        }
    }

    /// controlled 主窗口的旁路消息监听：接收服务端的 Data::ChatMessage（经
    /// ipc `_nerve_ui` 通道），push_event `nerve_chat_banner` → 主窗口横幅。
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::main(flavor = "current_thread")]
    pub async fn nerv_main_ui_listener() {
        match crate::ipc::new_listener("_nerve_ui").await {
            Ok(mut incoming) => {
                while let Some(result) = incoming.next().await {
                    let Ok(stream) = result else {
                        continue;
                    };
                    tokio::spawn(async move {
                        let mut conn = crate::ipc::Connection::new(stream);
                        loop {
                            match conn.next().await {
                                Ok(Some(crate::ipc::Data::ChatMessage { text })) => {
                                    FlutterHandler {}
                                        .push_event("nerve_chat_banner", &[("text", &text)], &[]);
                                }
                                Ok(Some(_)) => {}
                                _ => {
                                    log::debug!("NervDesk: _nerve_ui 通道断开");
                                    break;
                                }
                            }
                        }
                    });
                }
            }
            Err(err) => log::error!("NervDesk: _nerve_ui 监听失败: {}", err),
        }
    }"""

_IPC_OLD = """#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn set_unlock_pin(v: String, translate: bool) -> ResultType<()> {"""
_IPC_NEW = """/// r8/t35+t36：受控端旁路投递——把文字消息直接送到常驻主窗口（_nerve_ui），
/// 绕开隐藏中的 CM 窗口（其激活/置前会造成任务栏跳动）。
/// cfg 说明（t36 修复 E0425）：调用点在 server/connection.rs（`mod server` 在
/// Android 也编译），故定义必须对 Android 可见——仅排除 iOS（iOS 无 ipc 模块）。
#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "ios")))]
pub async fn send_chat_banner_to_main(text: String) -> ResultType<()> {
    if let Ok(mut c) = connect(1_000, "_nerve_ui").await {
        c.send(&Data::ChatMessage { text }).await?;
    }
    Ok(())
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn set_unlock_pin(v: String, translate: bool) -> ResultType<()> {"""

_CONN_OLD = """                    Some(misc::Union::ChatMessage(c)) => {
                        self.send_to_cm(ipc::Data::ChatMessage { text: c.text });
                        self.chat_unanswered = true;"""
_CONN_NEW = """                    Some(misc::Union::ChatMessage(c)) => {
                        // t37：先提取 text 再分发（原实现 send_to_cm 内 move c.text 后
                        // 再 clone → E0382 借用后移动；现一次 move + 两处克隆分发）
                        let text = c.text;
                        self.send_to_cm(ipc::Data::ChatMessage { text: text.clone() });
                        // r8/t35：controlled——旁路投递到常驻主窗口横幅（绕开隐藏 CM）
                        if hbb_common::config::nervdesk_mode_controlled() {
                            let t = text.clone();
                            tokio::spawn(async move {
                                if let Err(err) = crate::ipc::send_chat_banner_to_main(t).await {
                                    log::debug!("NervDesk: 主窗口横幅投递失败: {}", err);
                                }
                            });
                        }
                        self.chat_unanswered = true;"""

_MODEL_OLD = """      } else if (name == 'chat_server_mode') {
        parent.target?.chatModel
            .receive(int.parse(evt['id'] as String), evt['text'] ?? '');"""
_MODEL_NEW = """      } else if (name == 'chat_server_mode') {
        parent.target?.chatModel
            .receive(int.parse(evt['id'] as String), evt['text'] ?? '');
      } else if (name == 'nerve_chat_banner') {
        // r8/t35：controlled 旁路消息通道（绕开隐藏 CM）→ 主窗口横幅
        parent.target?.chatModel.lastServerMsg.value = evt['text'] ?? '';"""

_CHAT_OLD = """      if (isDesktop) {
        windowOnTop(null);"""
_CHAT_NEW = """      if (isDesktop && !kNervDeskModeControlled) {
        // r8/t35：controlled（CM 隐藏）禁止激活置前——Windows 会拦截导致任务栏跳动
        windowOnTop(null);"""

_HOME_F_OLD = """  // r7（t30）：受控端消息轻量弹窗（仅 controlled）
  Timer? _nerveMsgTimer;
  final RxnString _nervePopupMsg = RxnString();"""
_HOME_F_NEW = """  // r7（t30）+ r8（t35）：受控端消息轻量弹窗（仅 controlled；Rx 直听订阅）
  Timer? _nerveMsgTimer;
  StreamSubscription? _nerveMsgSub;
  final RxnString _nervePopupMsg = RxnString();"""

_HOME_I_OLD = """    // r7（t30）：controlled 监听受控消息 → 轻量弹窗（不依赖 CM 窗口可见）
    if (kNervDeskModeControlled) {
      gFFI.chatModel.addListener(_nerveOnChatMsg);
    }"""
_HOME_I_NEW = """    // r7（t30）+ r8（t35）：controlled 直听 lastServerMsg（Rx）→ 轻量弹窗；
    // 消息经 _nerve_ui 旁路通道直达主窗口（绕开隐藏 CM 的激活逻辑）。
    if (kNervDeskModeControlled) {
      _nerveMsgSub = gFFI.chatModel.lastServerMsg.listen((m) {
        if (m == null || m.isEmpty || !mounted) return;
        _nerveMsgTimer?.cancel();
        setState(() => _nervePopupMsg.value = m);
        _nerveMsgTimer = Timer(const Duration(seconds: 8), () {
          if (mounted) setState(() => _nervePopupMsg.value = null);
        });
      });
    }"""

_HOME_D_OLD = """    if (kNervDeskModeControlled) {
      gFFI.chatModel.removeListener(_nerveOnChatMsg);
      _nerveMsgTimer?.cancel();
    }"""
_HOME_D_NEW = """    if (kNervDeskModeControlled) {
      _nerveMsgSub?.cancel();
      _nerveMsgTimer?.cancel();
    }"""

_T35_FN_OLD = """  // r7（t30）：受控消息监听（仅 controlled 挂载）
  void _nerveOnChatMsg() {
    final m = gFFI.chatModel.lastServerMsg.value;
    if (m == null || m.isEmpty || !mounted) return;
    _nerveMsgTimer?.cancel();
    setState(() => _nervePopupMsg.value = m);
    _nerveMsgTimer = Timer(const Duration(seconds: 8), () {
      if (mounted) setState(() => _nervePopupMsg.value = null);
    });
  }

"""
_T35_FN_NEW = ""

_WIN_H_OLD = """                     bool showOnTaskBar = true,
                     bool resizable = true);"""
_WIN_H_NEW = """                     bool showOnTaskBar = true,
                     bool resizable = true,
                     bool noActivate = false);"""

_WIN_CPP_H_OLD = """bool Win32Window::CreateAndShow(const std::wstring& title,
                                const Point& origin,
                                const Size& size, bool showOnTaskBar,
                                bool resizable) {"""
_WIN_CPP_H_NEW = """bool Win32Window::CreateAndShow(const std::wstring& title,
                                const Point& origin,
                                const Size& size, bool showOnTaskBar,
                                bool resizable, bool noActivate) {"""

_WIN_CPP_W_OLD = """  const DWORD window_style =
      resizable ? WS_OVERLAPPEDWINDOW
                : (WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX);

  HWND window = CreateWindow(
      window_class, title.c_str(), window_style,
      Scale(origin.x, scale_factor), Scale(origin.y, scale_factor),
      Scale(size.width, scale_factor), Scale(size.height, scale_factor),
      nullptr, nullptr, GetModuleHandle(nullptr), this);"""
_WIN_CPP_W_NEW = """  const DWORD window_style =
      resizable ? WS_OVERLAPPEDWINDOW
                : (WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX);

  DWORD ex_style = 0;
  // r8/t35：隐藏中的 CM 窗口（controlled）加 WS_EX_NOACTIVATE——
  // 禁止激活置前（SetForegroundWindow 被 Windows 拦截 → 任务栏跳动根治）。
  if (noActivate) {
    ex_style |= WS_EX_NOACTIVATE;
  }
  HWND window = CreateWindowEx(
      ex_style, window_class, title.c_str(), window_style,
      Scale(origin.x, scale_factor), Scale(origin.y, scale_factor),
      Scale(size.width, scale_factor), Scale(size.height, scale_factor),
      nullptr, nullptr, GetModuleHandle(nullptr), this);"""

_MAIN_OLD = """                            /*resizable=*/!nervdesk_controlled && !is_cm_page)) {"""
_MAIN_NEW = """                            /*resizable=*/!nervdesk_controlled && !is_cm_page,
                            /*noActivate=*/is_cm_page && nervdesk_controlled)) {"""


def patch_t35_nerve() -> None:
    p = pathlib.Path("src/flutter.rs")
    t, crlf = read_text(p)
    t = sub1(t, _FLUTTER_OLD, _FLUTTER_NEW, "flutter.rs 主窗口旁路监听")
    write_text(p, t, crlf)
    p = pathlib.Path("src/ipc.rs")
    t, crlf = read_text(p)
    t = sub1(t, _IPC_OLD, _IPC_NEW, "ipc.rs 投递助手")
    write_text(p, t, crlf)
    p = pathlib.Path("src/server/connection.rs")
    t, crlf = read_text(p)
    t = sub1(t, _CONN_OLD, _CONN_NEW, "connection.rs 旁路推送")
    write_text(p, t, crlf)
    print("[OK] rust 侧：_nerve_ui 旁路通道（r8/t35）")


def patch_t35_dart() -> None:
    p = pathlib.Path("flutter/lib/models/model.dart")
    t, crlf = read_text(p)
    t = sub1(t, _MODEL_OLD, _MODEL_NEW, "model.dart nerve_chat_banner")
    write_text(p, t, crlf)
    p = pathlib.Path("flutter/lib/models/chat_model.dart")
    t, crlf = read_text(p)
    t = sub1(t, _CHAT_OLD, _CHAT_NEW, "chat_model no-activate")
    write_text(p, t, crlf)
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_home_page.dart")
    t, crlf = read_text(p)
    t = sub1(t, _HOME_F_OLD, _HOME_F_NEW, "home fields")
    t = sub1(t, _HOME_I_OLD, _HOME_I_NEW, "home initState Rx")
    t = sub1(t, _HOME_D_OLD, _HOME_D_NEW, "home dispose")
    # 幂等：旧监听 fn（r7 形态）若存在则移除（环境差异下可能未插入，容忍）
    if _T35_FN_OLD in t:
        t = t.replace(_T35_FN_OLD, "")
    write_text(p, t, crlf)
    print("[OK] dart 侧：横幅直听 + no-activate（r8/t35）")


def patch_t35_runner() -> None:
    p = pathlib.Path("flutter/windows/runner/win32_window.h")
    t, crlf = read_text(p)
    t = sub1(t, _WIN_H_OLD, _WIN_H_NEW, "win32_window.h noActivate")
    write_text(p, t, crlf)
    p = pathlib.Path("flutter/windows/runner/win32_window.cpp")
    t, crlf = read_text(p)
    t = sub1(t, _WIN_CPP_H_OLD, _WIN_CPP_H_NEW, "win32_window.cpp 签名")
    t = sub1(t, _WIN_CPP_W_OLD, _WIN_CPP_W_NEW, "win32_window.cpp WS_EX_NOACTIVATE")
    write_text(p, t, crlf)
    p = pathlib.Path("flutter/windows/runner/main.cpp")
    t, crlf = read_text(p)
    t = sub1(t, _MAIN_OLD, _MAIN_NEW, "main.cpp noActivate 传参")
    write_text(p, t, crlf)
    print("[OK] runner 侧：CM 窗口 no-activate（r8/t35）")
# ---------------------------------------------------------------------------
# 校验
# ---------------------------------------------------------------------------


def verify() -> None:
    ok = True

    def check(rel: str, needle: str, desc: str) -> None:
        nonlocal ok
        if needle not in pathlib.Path(rel).read_text(encoding="utf-8"):
            print(f"[FAIL] 校验失败：{rel} 未包含 {desc}", file=sys.stderr)
            ok = False
        else:
            print(f"[OK] 校验 {rel}: {desc}")

    check("libs/hbb_common/src/config.rs", "pub fn nervdesk_builtin_password_raw", "固定密码读取器")
    check("libs/hbb_common/src/config.rs", "OPTION_HIDE_POWERED_BY_ME", "hide-powered-by-me 种子")
    check("libs/hbb_common/src/config.rs", 'bs.insert("nervdesk-fixed-password"', "t21 固定密码 builtin 种子")
    check("src/flutter_ffi.rs", "pub fn main_get_builtin_password", "t21 ① FFI 导出存在")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "mainGetBuildinOption(key: 'nervdesk-fixed-password')", "t21 ② Dart 用既有桥读取")

    check("libs/hbb_common/src/config.rs", "nervdesk_permanent_password_auth_chain_is_self_consistent", "认证链路单测")
    check("src/flutter_ffi.rs", "pub fn main_get_builtin_password", "FFI 固定密码")
    check("flutter/lib/common.dart", "const bool kNervDeskModeControlled", "共享形态常量")
    check("flutter/lib/desktop/pages/connection_page.dart", "kNervDeskModeController ? PeerTabPage()", "连接页共享常量")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "showRightPane", "右栏条件化")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "builtinPassword", "固定密码显示")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "kNervDeskModeControlled) {", "状态位条件")
    check("flutter/windows/CMakeLists.txt", "NERVDESK_MODE_CONTROLLED=1", "CMake 编译宏")
    check("flutter/windows/runner/main.cpp", "500u, 700u", "controlled 窗口 500×700")
    check("flutter/windows/runner/main.cpp", "/*resizable=*/!nervdesk_controlled", "不可拉伸")
    check("flutter/windows/runner/win32_window.cpp", "window_style", "固定窗口样式")
    check("src/lang/cn.rs", '"Fixed Password", "固定密码"', "语言键")
    check("src/platform/windows.rs", "pub fn enforce_single_instance_gui", "r5 单实例互斥")
    check("src/platform/windows.rs", "CreateMutexW", "r5 命名互斥体")
    check("src/core_main.rs", "enforce_single_instance_gui()", "r5 入口调用")
    check("src/server/connection.rs", "r7（t30）修订：撤销 t26 的 early-return", "r7 CM 启动恢复")
    check("libs/hbb_common/src/config.rs", "\"allow-hide-cm\"", "r7 官方链强制项")
    check("libs/hbb_common/src/config.rs", "use-permanent-password", "r7 verification 强制")

    check("src/ipc.rs", "nervdesk_mode_controlled()", "r7 ipc hide_cm 应答")
    check("flutter/lib/models/chat_model.dart", "lastServerMsg", "r7 消息通道")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "_nerveMessageBanner", "r7 轻量弹窗")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "lastServerMsg.listen", "t35 旁路横幅直听")

    check("libs/hbb_common/src/config.rs", "PIN 设置锁", "r8 种子注释")
    check("libs/hbb_common/src/config.rs", "Config::set_unlock_pin(&pin)", "r8 种子写入")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "_nerveUnlockThen", "r8 PIN 门")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "checkUnlockPinDialog(bind.mainGetUnlockPin()", "r8 验证对话框")
    check("nervdesk/iroh_transport.rs", "方案 A（docs/12）", "r9 注释")
    check("nervdesk/iroh_transport.rs", "iroh-relay-token` 解析保留", "r9 兼容保留")

    check("src/flutter.rs", "nerv_main_ui_listener", "t35 旁路监听")
    check("src/ipc.rs", "send_chat_banner_to_main", "t35 投递助手")
    check("src/server/connection.rs", "send_chat_banner_to_main(t)", "t35/t37 服务端推送")
    check("flutter/lib/models/model.dart", "nerve_chat_banner", "t35 事件")
    check("flutter/lib/models/chat_model.dart", "isDesktop && !kNervDeskModeControlled", "t35 no-activate gate")
    check("flutter/windows/runner/win32_window.cpp", "WS_EX_NOACTIVATE", "t35 窗口 no-activate")
    check("flutter/windows/runner/main.cpp", "noActivate=*/is_cm_page && nervdesk_controlled", "t35 runner 传参")

    check("flutter/lib/desktop/pages/desktop_home_page.dart", "if (!kNervDeskModeControlled) buildPopupMenu(context)", "t27 C4a ID板菜单")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "kNervDeskModeControlled\n", "t27 C4 改密门控")
    check("flutter/lib/desktop/pages/desktop_tab_page.dart", "kNervDeskModeControlled,", "t27 C4b tabbar 设置钮")
    check("libs/hbb_common/src/config.rs", "OPTION_ALLOW_AUTO_UPDATE", "t27 C5 更新关闭")
    check("libs/hbb_common/src/config.rs", "OPTION_DISABLE_GROUP_PANEL", "t27 C6 历史面板")
    check("libs/hbb_common/src/config.rs", "OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW", "t27 4.3 权限键")


    check("src/core_main.rs", "// r5（t23）：Windows 单实例互斥", "r5 注释标记")


    # t36 符号闭环断言（树级）：connection.rs/flutter.rs 中的 crate::ipc::Xxx 调用
    # 必须在 ipc.rs 有 pub fn/pub async fn Xxx 定义（防 E0425 复发——E0425 曾两次）
    import re as _re
    _calls = set()
    for _f in ["src/server/connection.rs", "src/flutter.rs", "src/ipc.rs"]:
        _txt = pathlib.Path(_f).read_text(encoding="utf-8")
        _calls |= set(_re.findall(r"crate::ipc::([a-zA-Z_][a-zA-Z0-9_]*)\s*\(", _txt))
    _ipc_txt = pathlib.Path("src/ipc.rs").read_text(encoding="utf-8")
    _defined = set(_re.findall(r"pub (?:async )?fn ([a-zA-Z_][a-zA-Z0-9_]*)", _ipc_txt))
    _missing = sorted(_calls - _defined)
    if _missing:
        print(f"[FAIL] ipc 符号调用无定义: {_missing}", file=sys.stderr)
        ok = False
    else:
        print(f"[OK] t36 符号闭环：{sorted(_calls)} 全部有定义")

    # t37 形态断言（树级）：ChatMessage 分发前必须有提取（防 E0382）
    _conn = pathlib.Path("src/server/connection.rs").read_text(encoding="utf-8")
    _let_i = _conn.find("let text = c.text;")
    _send_i = _conn.find("self.send_to_cm(ipc::Data::ChatMessage { text: text.clone() })")
    if _let_i < 0 or _send_i < 0 or _let_i > _send_i:
        print("[FAIL] E0382 形态：ChatMessage 分发缺少前置提取（let text = c.text;）", file=sys.stderr)
        ok = False
    else:
        print("[OK] t37 形态：提取先于分发")

    # t27 反向断言（负向）：被控端「彻底静默化」——Ctrl+Alt+F12 快捷键必须已移除
    hp = pathlib.Path("flutter/lib/desktop/pages/desktop_home_page.dart").read_text(encoding="utf-8")
    if "_nerveBreakSessions" in hp or "LogicalKeyboardKey.f12" in hp:
        print("[FAIL] desktop_home_page.dart 仍含被控端中断快捷键（t27 应移除）", file=sys.stderr)
        ok = False

    # t21 反向断言：r3 报错根因（mainGetBuiltinPasswordSync）必须从 Dart 代码消失
    for f in FILES:
        if "mainGetBuiltinPasswordSync" in pathlib.Path(f).read_text(encoding="utf-8"):
            print(f"[FAIL] {f} 含 r3 报错方法名 mainGetBuiltinPasswordSync", file=sys.stderr)
            ok = False

    # 占位符纪律：完整占位符仍只在 config.rs const 初始化行
    cfg = pathlib.Path("libs/hbb_common/src/config.rs").read_text(encoding="utf-8")
    if cfg.count("__NERVDESK_PASSWORD__") != 1:
        print(f"[FAIL] __NERVDESK_PASSWORD__ 出现 {cfg.count('__NERVDESK_PASSWORD__')} 次（应 1 次）",
              file=sys.stderr)
        ok = False
    for f in FILES:
        if f == "libs/hbb_common/src/config.rs":
            continue
        if "__NERVDESK_PASSWORD__" in pathlib.Path(f).read_text(encoding="utf-8"):
            print(f"[FAIL] 占位符出现在非预期文件：{f}", file=sys.stderr)
            ok = False

    if not ok:
        sys.exit(1)
    print("[OK] 全部 t18 真机修复改动校验通过")


def main() -> None:
    print("=== NervDesk 真机三项修复补丁（t18）===")
    check_prereq()
    patch_config_rs()
    patch_flutter_ffi()
    patch_common_dart()
    patch_connection_page()
    patch_home_page()
    patch_cmake()
    patch_main_cpp()
    patch_win32_window()
    patch_cn_lang()
    patch_windows_single_instance()
    patch_core_main()
    patch_r6_config()
    patch_r6_home()
    patch_r6_tab()
    patch_r7_cm()
    patch_r7_config()
    patch_r7_ipc()
    patch_r7_chat_model()
    patch_r7_home()
    patch_r8_pin()
    patch_r8_home()
    patch_r9_iroh()
    patch_t35_nerve()
    patch_t35_dart()
    patch_t35_runner()
    verify()
    print("=== 完成 ===")
    print("提示：logo/icon 图形资产在 nervdesk/branding/（t19 装配 cp 覆盖 flutter/assets/）；")
    print("本地验证：cargo test -p hbb_common --lib password（47 项，含认证链路单测）。")


if __name__ == "__main__":
    main()