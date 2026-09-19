#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
NervDesk 品牌全深度补丁（M3）应用脚本。

依赖顺序（硬约束）：
  1) 先应用 A 档品牌补丁（patches/branding-apply.py 或 branding-custom-client.patch）——
     本脚本的 Runner.rc 锚点是品牌化后的状态；
  2) t2（patches/nervdesk-features/）与 t3（patches/nervdesk-network-service/）按序应用；
  3) 最后应用本补丁。脚本会在每步前校验前置特征，缺失即报错。

用法（在 RustDesk 源码根目录执行，也就是有 Cargo.toml 的那一层）：

    python3 patches/nervdesk-branding-depth/apply_nervdesk_branding_depth.py

改动范围（与 nervdesk-branding-depth-change-list.md 完全一致，锚点逐一校验）：

  1. flutter/windows/CMakeLists.txt    BINARY_NAME "rustdesk" -> "nervdesk"（可执行名）
  2. flutter/windows/runner/Runner.rc  LegalCopyright 追加 "Based on RustDesk (AGPL-3.0)"
  3. src/platform/windows.rs          消息框标题 "RustDesk Output" -> "NervDesk Output"
  4. src/auth_2fa.rs                  TOTP ISSUER "RustDesk" -> "NervDesk"
  5. src/server/connection.rs        打印机任务路径 RustDesk://FS... -> 跟随 uri prefix
  6. build.py                         portable 打包输入 exe 名 rustdesk.exe -> nervdesk.exe
  7. flutter/lib/common.dart          debugPrint "Start closing NervDesk..."
  8. flutter/lib/desktop/widgets/tabbar_widget.dart  顶部标题 "RustDesk" -> "NervDesk"

（图标为二进制文件：复制 nervdesk/branding/*.ico，见 nervdesk/branding/README.md；
 本脚本不处理二进制。）
"""

import pathlib
import sys

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass


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
    rc = pathlib.Path("flutter/windows/runner/Runner.rc")
    if not rc.is_file():
        die("找不到 flutter/windows/runner/Runner.rc")
    rc_t = rc.read_text(encoding="utf-8")
    if '"NervDesk" "\\0"' not in rc_t or '"nervdesk.exe" "\\0"' not in rc_t:
        die("Runner.rc 尚未品牌化（找不到 NervDesk / nervdesk.exe）——请先应用 A 档品牌补丁")
    if "OriginalFilename" not in rc_t:
        die("Runner.rc 缺少 OriginalFilename——请先应用 A 档品牌补丁")
    cfg = pathlib.Path("libs/hbb_common/src/config.rs")
    if cfg.is_file() and "pub const NERVDESK_FORCED_OPTIONS" not in cfg.read_text(encoding="utf-8"):
        die("未找到 t2 的 NERVDESK_FORCED_OPTIONS——建议按顺序先应用 t2（本补丁本身不强依赖，但装配顺序要求）")
    print("[OK] 前置校验：A 档品牌已应用")


def patch_cmake() -> None:
    p = pathlib.Path("flutter/windows/CMakeLists.txt")
    t, crlf = read_text(p)
    t = sub1(t, 'set(BINARY_NAME "rustdesk")', 'set(BINARY_NAME "nervdesk")',
             "CMakeLists BINARY_NAME")
    write_text(p, t, crlf)
    print("[OK] CMakeLists.txt: BINARY_NAME = nervdesk（可执行名）")


def patch_runner_rc() -> None:
    p = pathlib.Path("flutter/windows/runner/Runner.rc")
    t, crlf = read_text(p)
    t = sub1(t,
             'VALUE "LegalCopyright", "Copyright © 2026 NervDesk" "\\0"',
             'VALUE "LegalCopyright", "Copyright © 2026 NervDesk. Based on RustDesk (AGPL-3.0)." "\\0"',
             "Runner.rc LegalCopyright")
    write_text(p, t, crlf)
    print("[OK] Runner.rc: 版权行注明基于 AGPL RustDesk")


def patch_windows_rs() -> None:
    p = pathlib.Path("src/platform/windows.rs")
    t, crlf = read_text(p)
    t = sub1(t,
             'let caption = "RustDesk Output"',
             'let caption = "NervDesk Output"',
             "windows.rs 消息框标题")
    write_text(p, t, crlf)
    print("[OK] windows.rs: 消息框标题 NervDesk Output")


def patch_auth2fa() -> None:
    p = pathlib.Path("src/auth_2fa.rs")
    t, crlf = read_text(p)
    t = sub1(t, 'const ISSUER: &str = "RustDesk";', 'const ISSUER: &str = "NervDesk";',
             "auth_2fa.rs TOTP ISSUER")
    write_text(p, t, crlf)
    print("[OK] auth_2fa.rs: TOTP ISSUER = NervDesk")


def patch_connection() -> None:
    p = pathlib.Path("src/server/connection.rs")
    t, crlf = read_text(p)
    t = sub1(t,
             'let path = format!("RustDesk://FsJob//Printer/{}", get_time());',
             'let path = format!("{}FsJob//Printer/{}", crate::get_uri_prefix(), get_time());',
             "connection.rs 打印机任务路径")
    write_text(p, t, crlf)
    print("[OK] connection.rs: 打印机任务路径跟随 uri prefix（nervdesk://）")


def patch_build_py() -> None:
    p = pathlib.Path("build.py")
    t, crlf = read_text(p)
    # 普通字符串拼接，避免 f-string 花括号转义混淆
    old = "{flutter_build_dir_2}/rustdesk.exe')"
    new = "{flutter_build_dir_2}/nervdesk.exe')"
    t = sub1(t, old, new, "build.py portable 输入 exe")
    write_text(p, t, crlf)
    print("[OK] build.py: portable 打包输入 = nervdesk.exe")


def patch_common_dart() -> None:
    p = pathlib.Path("flutter/lib/common.dart")
    t, crlf = read_text(p)
    t = sub1(t, 'debugPrint("Start closing RustDesk...");',
             'debugPrint("Start closing NervDesk...");', "common.dart debugPrint")
    write_text(p, t, crlf)
    print("[OK] common.dart: debugPrint NervDesk")


def patch_tabbar() -> None:
    p = pathlib.Path("flutter/lib/desktop/widgets/tabbar_widget.dart")
    t, crlf = read_text(p)
    t = sub1(t,
             'child: const Text(\n                              "RustDesk",',
             'child: const Text(\n                              "NervDesk",',
             "tabbar_widget.dart 顶部标题")
    write_text(p, t, crlf)
    print("[OK] tabbar_widget.dart: 顶部标题 NervDesk")


def verify() -> None:
    ok = True

    def check(rel: str, needle: str, desc: str) -> None:
        nonlocal ok
        if needle not in pathlib.Path(rel).read_text(encoding="utf-8"):
            print(f"[FAIL] 校验失败：{rel} 未包含 {desc}", file=sys.stderr)
            ok = False
        else:
            print(f"[OK] 校验 {rel}: {desc}")

    check("flutter/windows/CMakeLists.txt", 'set(BINARY_NAME "nervdesk")', "BINARY_NAME nervdesk")
    check("flutter/windows/runner/Runner.rc", "Based on RustDesk (AGPL-3.0)", "版权注明 AGPL RustDesk")
    check("src/platform/windows.rs", 'let caption = "NervDesk Output"', "消息框标题 NervDesk")
    check("src/auth_2fa.rs", 'const ISSUER: &str = "NervDesk";', "TOTP ISSUER NervDesk")
    check("src/server/connection.rs", "crate::get_uri_prefix()", "打印机任务路径跟随前缀")
    check("build.py", "{flutter_build_dir_2}/nervdesk.exe')", "build.py 输入 nervdesk.exe")
    check("flutter/lib/common.dart", "Start closing NervDesk", "debugPrint NervDesk")
    check("flutter/lib/desktop/widgets/tabbar_widget.dart", 'child: const Text(\n                              "NervDesk",', "顶部标题 NervDesk")

    # 反向：可执行名相关的旧字面量不应再出现在运行时路径
    if 'set(BINARY_NAME "rustdesk")' in pathlib.Path("flutter/windows/CMakeLists.txt").read_text(encoding="utf-8"):
        print("[FAIL] CMakeLists 仍为 BINARY_NAME rustdesk", file=sys.stderr)
        ok = False

    if not ok:
        sys.exit(1)
    print("[OK] 全部 NervDesk 品牌全深度改动校验通过")


def main() -> None:
    print("=== NervDesk 品牌全深度补丁（M3，基于 A 档品牌 + t2 + t3）===")
    check_prereq()
    patch_cmake()
    patch_runner_rc()
    patch_windows_rs()
    patch_auth2fa()
    patch_connection()
    patch_build_py()
    patch_common_dart()
    patch_tabbar()
    verify()
    print("=== 完成 ===")
    print("提示：图标复制见 nervdesk/branding/README.md（app_icon.ico / tray-icon.ico / installer.ico）；")
    print("MSI 品牌参数 --app-name NervDesk --manufacturer NervDesk 由 t6 在 workflow 中追加。")


if __name__ == "__main__":
    main()