#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
把 RustDesk 客户端品牌定制为 NervDesk（最小 diff，其余代码零改动）。

用法（在 RustDesk 源码根目录执行，也就是有 Cargo.toml 的那一层）：

    python3 patches/branding-apply.py

可通过环境变量覆盖品牌名与站点：

    BRAND_NAME       默认 NervDesk（决定 APP_NAME、窗口标题、exe 属性、About 标题）
    BRAND_SITE       默认 https://nervcode.eu.org（About 页 Website / Privacy 链接）
    BRAND_COPYRIGHT  默认同 BRAND_NAME（exe 属性 LegalCopyright / About 版权行）

改动范围（与 patches/branding-change-list.md 完全一致）：

  1. libs/hbb_common/src/config.rs      APP_NAME -> BRAND_NAME
  2. flutter/windows/runner/main.cpp    窗口标题静态兜底值 -> BRAND_NAME
  3. flutter/windows/runner/Runner.rc   exe 文件属性（Company/Product/FileDescription/...）
  4. flutter/lib/desktop/pages/desktop_setting_page.dart   About 标题 / 链接 / 版权行
  5. flutter/lib/mobile/pages/settings_page.dart           About 标题

图标是二进制文件，不在此脚本范围：按 branding-change-list.md §2 替换
  flutter/windows/runner/resources/app_icon.ico
  res/tray-icon.ico
  res/icon.ico

特性（与 apply_custom_client.py 一致）：
  - 锚点匹配数 != 1 立即失败，绝不静默跳过；
  - CRLF/LF 自适应（Windows CI checkout 出来是 CRLF 也能打）；
  - 打完后立刻校验，防止「看起来打了、实际没打上」。
"""

import os
import pathlib
import sys

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

BRAND_NAME = os.environ.get("BRAND_NAME", "NervDesk").strip()
BRAND_SITE = os.environ.get("BRAND_SITE", "https://nervcode.eu.org").strip()
BRAND_COPYRIGHT = os.environ.get("BRAND_COPYRIGHT", BRAND_NAME).strip()

# 只处理文本文件；图标请按文档手动替换
FILES = [
    "libs/hbb_common/src/config.rs",
    "flutter/windows/runner/main.cpp",
    "flutter/windows/runner/Runner.rc",
    "flutter/lib/desktop/pages/desktop_setting_page.dart",
    "flutter/lib/mobile/pages/settings_page.dart",
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


def patch_config_rs() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    if not p.is_file():
        die("找不到 libs/hbb_common/src/config.rs（子模块没拉下来？）")
    t, crlf = read_text(p)
    t = sub1(
        t,
        'pub static ref APP_NAME: RwLock<String> = RwLock::new("RustDesk".to_owned());',
        f'pub static ref APP_NAME: RwLock<String> = RwLock::new("{BRAND_NAME}".to_owned());',
        "config.rs APP_NAME",
    )
    write_text(p, t, crlf)
    print(f"[OK] config.rs: APP_NAME = {BRAND_NAME!r}")


def patch_main_cpp() -> None:
    p = pathlib.Path("flutter/windows/runner/main.cpp")
    if not p.is_file():
        die("找不到 flutter/windows/runner/main.cpp")
    t, crlf = read_text(p)
    t = sub1(
        t,
        '  std::wstring app_name = L"RustDesk";',
        f'  std::wstring app_name = L"{BRAND_NAME}";',
        "main.cpp app_name",
    )
    write_text(p, t, crlf)
    print(f"[OK] main.cpp: 窗口标题静态兜底 = {BRAND_NAME!r}")


def patch_runner_rc() -> None:
    p = pathlib.Path("flutter/windows/runner/Runner.rc")
    if not p.is_file():
        die("找不到 flutter/windows/runner/Runner.rc")
    t, crlf = read_text(p)
    t = sub1(t, 'VALUE "CompanyName", "Purslane Tech Pte. Ltd." "\\0"',
                f'VALUE "CompanyName", "{BRAND_COPYRIGHT}" "\\0"', "Runner.rc CompanyName")
    t = sub1(t, 'VALUE "FileDescription", "RustDesk Remote Desktop" "\\0"',
                f'VALUE "FileDescription", "{BRAND_NAME} Remote Desktop" "\\0"', "Runner.rc FileDescription")
    t = sub1(t, 'VALUE "InternalName", "rustdesk" "\\0"',
                f'VALUE "InternalName", "{BRAND_NAME.lower()}" "\\0"', "Runner.rc InternalName")
    t = sub1(t, 'VALUE "LegalCopyright", "Copyright \u00a9 2026 Purslane Tech Pte. Ltd. All rights reserved." "\\0"',
                f'VALUE "LegalCopyright", "Copyright \u00a9 2026 {BRAND_COPYRIGHT}" "\\0"', "Runner.rc LegalCopyright")
    t = sub1(t, 'VALUE "OriginalFilename", "rustdesk.exe" "\\0"',
                f'VALUE "OriginalFilename", "{BRAND_NAME.lower()}.exe" "\\0"', "Runner.rc OriginalFilename")
    t = sub1(t, 'VALUE "ProductName", "RustDesk" "\\0"',
                f'VALUE "ProductName", "{BRAND_NAME}" "\\0"', "Runner.rc ProductName")
    write_text(p, t, crlf)
    print(f"[OK] Runner.rc: ProductName/Company/Description = {BRAND_NAME!r} / {BRAND_COPYRIGHT!r}")


def patch_desktop_about() -> None:
    p = pathlib.Path("flutter/lib/desktop/pages/desktop_setting_page.dart")
    if not p.is_file():
        die("找不到 flutter/lib/desktop/pages/desktop_setting_page.dart")
    t, crlf = read_text(p)
    t = sub1(t, "child: _Card(title: translate('About RustDesk'), children: [",
                f"child: _Card(title: translate('About {BRAND_NAME}'), children: [",
                "desktop About 标题")
    t = sub1(t, "launchUrlString('https://rustdesk.com/privacy.html');",
                f"launchUrlString('{BRAND_SITE}/privacy.html');", "desktop Privacy 链接")
    t = sub1(t, "launchUrlString('https://rustdesk.com');",
                f"launchUrlString('{BRAND_SITE}');", "desktop Website 链接")
    t = sub1(t, "'Copyright \u00a9 ${DateTime.now().toString().substring(0, 4)} Purslane Tech Pte. Ltd.\\n$license',",
                f"'Copyright \u00a9 ${{DateTime.now().toString().substring(0, 4)}} {BRAND_COPYRIGHT}\\n$license',",
                "desktop 版权行")
    write_text(p, t, crlf)
    print(f"[OK] desktop_setting_page.dart: About 标题/链接/版权 = {BRAND_NAME!r} / {BRAND_SITE!r}")


def patch_mobile_about() -> None:
    p = pathlib.Path("flutter/lib/mobile/pages/settings_page.dart")
    if not p.is_file():
        die("找不到 flutter/lib/mobile/pages/settings_page.dart")
    t, crlf = read_text(p)
    t = sub1(t, "title: Text(translate('About RustDesk')),",
                f"title: Text(translate('About {BRAND_NAME}')),", "mobile About 标题")
    write_text(p, t, crlf)
    print(f"[OK] mobile settings_page.dart: About 标题 = {BRAND_NAME!r}")


def verify() -> None:
    ok = True
    checks = [
        ("libs/hbb_common/src/config.rs", f'APP_NAME: RwLock<String> = RwLock::new("{BRAND_NAME}".to_owned())'),
        ("flutter/windows/runner/main.cpp", f'app_name = L"{BRAND_NAME}"'),
        ("flutter/windows/runner/Runner.rc", f'"ProductName", "{BRAND_NAME}"'),
        ("flutter/lib/desktop/pages/desktop_setting_page.dart", f"translate('About {BRAND_NAME}')"),
    ]
    for rel, needle in checks:
        if not pathlib.Path(rel).is_file():
            print(f"[FAIL] 校验失败：{rel} 不存在", file=sys.stderr)
            ok = False
            continue
        if needle not in pathlib.Path(rel).read_text(encoding="utf-8"):
            print(f"[FAIL] 校验失败：{rel} 未包含 {needle!r}", file=sys.stderr)
            ok = False
        else:
            print(f"[OK] 校验 {rel}")
    # 反向校验：允许残留 'RustDesk' 的地方必须只剩我们没承诺改的（如 Permission 文案、github 链接）
    # 但 Apply 到 1.4.9 时，这 5 个文件里除 lang 文案外不应再有主要品牌字符串。
    if not ok:
        sys.exit(1)
    print("[OK] 全部品牌改动校验通过")


def main() -> None:
    print("=== NervDesk 品牌定制补丁 ===")
    print(f"  品牌名  : {BRAND_NAME}")
    print(f"  站点    : {BRAND_SITE}")
    print(f"  版权     : {BRAND_COPYRIGHT}")
    patch_config_rs()
    patch_main_cpp()
    patch_runner_rc()
    patch_desktop_about()
    patch_mobile_about()
    verify()
    print("=== 完成 ===")
    print("提示：图标（app_icon.ico / tray-icon.ico / icon.ico）是二进制文件，")
    print("请按 patches/branding-change-list.md §2 手工替换，本脚本不处理。")


if __name__ == "__main__":
    main()