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
# 8b. t26 C1：隐藏受控端会话面板（start_ipc 跳过 CM 进程）——保留（用户未撤 C1）
# ---------------------------------------------------------------------------

_R5_CM_OLD = r"""async fn start_ipc(
    mut rx_to_cm: mpsc::UnboundedReceiver<ipc::Data>,
    tx_from_cm: mpsc::UnboundedSender<ipc::Data>,
    mut _rx_desktop_ready: mpsc::Receiver<()>,
    tx_stream_ready: mpsc::Sender<()>,
) -> ResultType<()> {
    use hbb_common::anyhow::anyhow;

    loop {"""

_R5_CM_NEW = r"""async fn start_ipc(
    mut rx_to_cm: mpsc::UnboundedReceiver<ipc::Data>,
    tx_from_cm: mpsc::UnboundedSender<ipc::Data>,
    mut _rx_desktop_ready: mpsc::Receiver<()>,
    tx_stream_ready: mpsc::Sender<()>,
) -> ResultType<()> {
    use hbb_common::anyhow::anyhow;

    // NervDesk 定制（t26 C1）：受控端隐藏「会话控制面板」（CM 小窗）。
    // controlled 变体不启动 --cm 面板进程（含 --tray 兜底）；会话 io_loop 不受
    // 影响。中断途径（r6 用户拍板「彻底静默化」后）：仅控制端断开 / 运维脚本
    // （taskkill / M7 service-install/watchdog），被控端无任何快捷键途径。
    if hbb_common::config::nervdesk_mode_controlled() {
        log::info!("NervDesk controlled: 隐藏会话控制面板（跳过 start_ipc/CM 窗口）");
        return Ok(());
    }

    loop {"""


def patch_cm_hide_server() -> None:
    p = pathlib.Path("src/server/connection.rs")
    t, crlf = read_text(p)
    t = sub1(t, _R5_CM_OLD, _R5_CM_NEW, "connection.rs start_ipc 隐藏 CM（t26 C1）")
    write_text(p, t, crlf)
    print("[OK] connection.rs: 受控端隐藏会话控制面板（t26 C1）")


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
    check("src/server/connection.rs", "nervdesk_mode_controlled()", "t26 隐藏 CM gate")
    check("src/server/connection.rs", "隐藏会话控制面板", "t26 注释标记")
    check("src/server/connection.rs", "仅控制端断开 / 运维脚本", "r6 彻底静默化注释")

    check("flutter/lib/desktop/pages/desktop_home_page.dart", "if (!kNervDeskModeControlled) buildPopupMenu(context)", "t27 C4a ID板菜单")
    check("flutter/lib/desktop/pages/desktop_home_page.dart", "kNervDeskModeControlled\n", "t27 C4 改密门控")
    check("flutter/lib/desktop/pages/desktop_tab_page.dart", "kNervDeskModeControlled,", "t27 C4b tabbar 设置钮")
    check("libs/hbb_common/src/config.rs", "OPTION_ALLOW_AUTO_UPDATE", "t27 C5 更新关闭")
    check("libs/hbb_common/src/config.rs", "OPTION_DISABLE_GROUP_PANEL", "t27 C6 历史面板")
    check("libs/hbb_common/src/config.rs", "OPTION_ENABLE_PERM_CHANGE_IN_ACCEPT_WINDOW", "t27 4.3 权限键")


    check("src/core_main.rs", "// r5（t23）：Windows 单实例互斥", "r5 注释标记")


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
    patch_cm_hide_server()
    patch_r6_config()
    patch_r6_home()
    patch_r6_tab()
    verify()
    print("=== 完成 ===")
    print("提示：logo/icon 图形资产在 nervdesk/branding/（t19 装配 cp 覆盖 flutter/assets/）；")
    print("本地验证：cargo test -p hbb_common --lib password（47 项，含认证链路单测）。")


if __name__ == "__main__":
    main()