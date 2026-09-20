#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
NervDesk 网络与服务补丁（M4 网络 + M7 服务）应用脚本。

依赖：必须先应用 t2（patches/nervdesk-features/），本脚本会先校验
      `NERVDESK_FORCED_OPTIONS` 是否存在，缺失则报错并提示顺序。

用法（在 RustDesk 源码根目录执行，也就是有 Cargo.toml 的那一层）：

    python3 patches/nervdesk-network-service/apply_nervdesk_network_service.py

改动范围（与 nervdesk-network-service-change-list.md 完全一致，锚点逐一校验）：

  1. libs/hbb_common/src/config.rs
       - NERVDESK_RELAY_SERVER / NERVDESK_API_SERVER / NERVDESK_BUILTIN_RELAY_TOKEN
         三个内置 const（参数化注入点）
       - nerve_apply_network_defaults()：隐藏网络/服务器/代理设置入口；把内置
         ID/Relay/API/Key 写进 OVERWRITE_SETTINGS（写入被丢弃+UI 锁定）；
         iroh relay 令牌占位符被 CI 替换后编译期内置
       - t2 的 nerve_apply_forced_defaults() 末尾追加 network 固化调用
  2. src/common.rs
       - get_custom_rendezvous_server / get_api_server_：Windows 上恒返回内置值
       - API 兜底字符串改为读取 config.rs 的 NERVDESK_API_SERVER const
  3. src/platform/windows.rs
       - get_license_from_exe_name()：屏蔽「exe 文件名携带服务器配置」覆盖路径
         （M4-6 最严格），并清理随之不再使用的 import

特性（与 nervdesk-features/apply_nervdesk_features.py 一致）：
  - 锚点匹配数 != 1 立即失败，绝不静默跳过；
  - CRLF/LF 自适应（Windows CI checkout 出来是 CRLF 也能打）；
  - 打完后立刻校验，防止「看起来打了、实际没打上」；
  - 占位符纪律：__NERVDESK_RELAY_TOKEN__ 只允许出现在
    libs/hbb_common/src/config.rs（const 定义与守卫比较）。

注意：M7 没有源码改动，服务注册/自愈/ACL 见 tools/ 下三个 ps1 脚本：
  - tools/nervdesk-service-install.ps1                （注册+开机自启+失败自重启+ACL+watchdog）
  - tools/nervdesk-service-watchdog.ps1               （计划任务兜底脚本）
  - tools/nervdesk-service-watchdog-register.ps1      （watchdog 计划任务注册/注销）
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
    "src/platform/windows.rs",
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


NETWORK_BLOCK = """// ============================================================================
// NervDesk 定制（M4 网络：内置地址参数化 / 界面隐藏强制 / relay 令牌编译期内置）
// 仅 Windows 生效（M5 平台承诺）；其它平台保持上游行为。
// ============================================================================

/// 内置中继服务器（hbbr）：写入 `relay-server` 选项，恒优先于 ID 服务器下发的列表。
/// 非机密值（域名端口公开），CI 仍可替换此 const 参数化。
/// 内置中继服务器（hbbr / 2.x ws relay）：写入 `relay-server` 选项，恒优先于
/// ID 服务器下发的列表。r5（t23）：改用 **api.nervcode.eu.org 裸域**——客户端
/// `socket_client::check_ws` 对「域:relay端口」目标会构造 `wss://api.nervcode.eu.org/ws/relay`
/// （2.x ws relay），无端口时按既有 relay 端口补全后同路径；legacy 服务器亦可
/// 自适应。非机密值，CI 仍可替换此 const 参数化。
pub const NERVDESK_RELAY_SERVER: &str = "api.nervcode.eu.org";

/// 内置 API 服务器：common.rs 的 get_api_server_ 最终兜底改读此 const（注入点）。
/// 非机密值，CI 可替换。
pub const NERVDESK_API_SERVER: &str = "https://api.nervcode.eu.org";

/// iroh relay 访问令牌占位符：构建期由 CI Secret（NERVDESK_RELAY_TOKEN）替换。
/// 未替换时不注入任何令牌（保持既有「不配 = 无令牌」语义）；
/// 令牌编译进二进制可被提取，真正的防线是 relay 侧 access.shared_token +
/// EndpointId 白名单（docs/04 §5），详见 docs/09 §M4-8。
pub const NERVDESK_BUILTIN_RELAY_TOKEN: &str = "__NERVDESK_RELAY_TOKEN__";

/// F1 修复：relay 令牌哨兵同样运行时拼接（`format!("__{}_{}", "NERVDESK",
/// "RELAY_TOKEN__")`），guard 右值不被 CI 整树替换；产物 rodata 中无连续
/// 占位符串，M6-3 扫描不误报。
#[inline]
fn nervdesk_relay_token_marker() -> String {
    format!("__{}_{}", "NERVDESK", "RELAY_TOKEN__")
}

/// M4 网络层固化（在 nerve_apply_forced_defaults 之后调用）：
/// 1) 隐藏「设置 → 网络 / 服务器 / 代理」编辑入口（builtin）；
/// 2) 把内置服务器地址写进 OVERWRITE_SETTINGS —— 用户配置/注册表/命令行的写入
///    被 is_option_can_save 丢弃（与 common.rs 读取层强制构成双重保证）；
/// 3) iroh relay 令牌：占位符已被 CI 替换时编译期内置；读侧
///    iroh_transport::config_from_options() 调用 get_option("iroh-relay-token") 即命中。
#[cfg(target_os = "windows")]
pub fn nerve_apply_network_defaults() {
    {
        let mut bs = BUILTIN_SETTINGS.write().unwrap();
        bs.insert(keys::OPTION_HIDE_NETWORK_SETTINGS.to_owned(), "Y".to_owned());
        bs.insert(keys::OPTION_HIDE_SERVER_SETTINGS.to_owned(), "Y".to_owned());
        bs.insert(keys::OPTION_HIDE_PROXY_SETTINGS.to_owned(), "Y".to_owned());
    }
    {
        let builtin_rendezvous: String = RENDEZVOUS_SERVERS.join(",");
        let mut ow = OVERWRITE_SETTINGS.write().unwrap();
        if !builtin_rendezvous.is_empty() {
            ow.insert(
                keys::OPTION_CUSTOM_RENDEZVOUS_SERVER.to_owned(),
                builtin_rendezvous,
            );
        }
        ow.insert(keys::OPTION_RELAY_SERVER.to_owned(), NERVDESK_RELAY_SERVER.to_owned());
        ow.insert(keys::OPTION_API_SERVER.to_owned(), NERVDESK_API_SERVER.to_owned());
        ow.insert(keys::OPTION_KEY.to_owned(), RS_PUB_KEY.to_owned());
        if NERVDESK_BUILTIN_RELAY_TOKEN != nervdesk_relay_token_marker() {
            ow.insert(
                "iroh-relay-token".to_owned(),
                NERVDESK_BUILTIN_RELAY_TOKEN.to_owned(),
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn nerve_apply_network_defaults() {}
"""


def patch_config_rs() -> None:
    p = pathlib.Path("libs/hbb_common/src/config.rs")
    if not p.is_file():
        die("找不到 libs/hbb_common/src/config.rs（子模块没拉下来？）")
    t, crlf = read_text(p)
    # 前置校验：t2 必须先应用（本补丁的锚点在 t2 新增代码上）
    if "pub const NERVDESK_FORCED_OPTIONS" not in t:
        die("未找到 t2 的 NERVDESK_FORCED_OPTIONS——请先应用 patches/nervdesk-features/（t2），再应用本补丁")
    # E1: t2 的 nerve_apply_forced_defaults 末尾追加 network 固化调用
    t = sub1(
        t,
        'log::info!("NervDesk: 写入出厂固定密码（构建期 Secret 替换）");\n'
        "        let _ = Config::set_permanent_password(NERVDESK_BUILTIN_PASSWORD);\n"
        "    }\n}",
        'log::info!("NervDesk: 写入出厂固定密码（构建期 Secret 替换）");\n'
        "        let _ = Config::set_permanent_password(NERVDESK_BUILTIN_PASSWORD);\n"
        "    }\n"
        "    // NervDesk 定制（M4 网络）：内置地址锁定 + 网络设置隐藏 + iroh relay 令牌\n"
        "    nerve_apply_network_defaults();\n}",
        "config.rs nerve_apply_forced_defaults 追加调用",
    )
    # E2: 在 t2 块之后插入 M4 网络块（锚点：空 fn + common_load 开头）
    t = sub1(
        t,
        "#[cfg(not(target_os = \"windows\"))]\n"
        "pub fn nerve_apply_forced_defaults() {}\n\npub fn common_load<",
        "#[cfg(not(target_os = \"windows\"))]\n"
        "pub fn nerve_apply_forced_defaults() {}\n\n"
        + NETWORK_BLOCK
        + "\npub fn common_load<",
        "config.rs M4 网络块",
    )
    write_text(p, t, crlf)
    print("[OK] config.rs: 内置地址 const + nerve_apply_network_defaults + 调用链")


def patch_common_rs() -> None:
    p = pathlib.Path("src/common.rs")
    if not p.is_file():
        die("找不到 src/common.rs")
    t, crlf = read_text(p)
    # E3: get_custom_rendezvous_server Windows 恒返回内置
    t = sub1(
        t,
        "pub fn get_custom_rendezvous_server(custom: String) -> String {\n"
        "    #[cfg(windows)]\n"
        "    if let Ok(lic) = crate::platform::windows::get_license_from_exe_name() {",
        "pub fn get_custom_rendezvous_server(custom: String) -> String {\n"
        "    // NervDesk 定制（M4-6 最严格）：Windows 上内置 ID 服务器地址恒优先，\n"
        "    // 屏蔽 exe 改名 / config 选项 / PROD_RENDEZVOUS_SERVER 等一切覆盖路径。\n"
        "    #[cfg(target_os = \"windows\")]\n"
        "    {\n"
        "        let builtin: Vec<String> = config::RENDEZVOUS_SERVERS\n"
        "            .iter()\n"
        "            .map(|x| x.to_string())\n"
        "            .collect();\n"
        "        if !builtin.is_empty() {\n"
        "            return builtin.join(\",\");\n"
        "        }\n"
        "    }\n"
        "    #[cfg(windows)]\n"
        "    if let Ok(lic) = crate::platform::windows::get_license_from_exe_name() {",
        "common.rs get_custom_rendezvous_server 内置优先",
    )
    # E4: get_api_server_ Windows 恒返回内置
    t = sub1(
        t,
        "fn get_api_server_(api: String, custom: String) -> String {\n"
        "    #[cfg(windows)]\n"
        "    if let Ok(lic) = crate::platform::windows::get_license_from_exe_name() {",
        "fn get_api_server_(api: String, custom: String) -> String {\n"
        "    // NervDesk 定制（M4-6）：Windows 上 API 服务器恒为编译期内置值\n"
        "    // （NERVDESK_API_SERVER const，CI 可替换；config.rs 参数化注入点）。\n"
        "    #[cfg(target_os = \"windows\")]\n"
        "    {\n"
        "        let builtin = config::NERVDESK_API_SERVER;\n"
        "        if !builtin.is_empty() {\n"
        "            return builtin.to_owned();\n"
        "        }\n"
        "    }\n"
        "    #[cfg(windows)]\n"
        "    if let Ok(lic) = crate::platform::windows::get_license_from_exe_name() {",
        "common.rs get_api_server_ 内置优先",
    )
    # E5: API 兜底字符串改 const
    t = sub1(
        t,
        '    "https://admin.rustdesk.com".to_owned()\n}',
        "    // NervDesk 定制（M4-3）：API 兜底改读 config.rs 内置 const（参数化注入点）。\n"
        "    config::NERVDESK_API_SERVER.to_owned()\n}",
        "common.rs API 兜底 const",
    )
    write_text(p, t, crlf)
    print("[OK] common.rs: 读取层强制内置 + API 兜底 const")


def patch_windows_rs() -> None:
    p = pathlib.Path("src/platform/windows.rs")
    if not p.is_file():
        die("找不到 src/platform/windows.rs")
    t, crlf = read_text(p)
    # E6: 屏蔽 exe 文件名携带服务器配置的覆盖路径
    t = sub1(
        t,
        "pub fn get_license_from_exe_name() -> ResultType<CustomServer> {\n"
        "    let mut exe = std::env::current_exe()?.to_str().unwrap_or(\"\").to_owned();\n"
        "    // if defined portable appname entry, replace original executable name with it.\n"
        "    if let Ok(portable_exe) = std::env::var(PORTABLE_APPNAME_RUNTIME_ENV_KEY) {\n"
        "        exe = portable_exe;\n"
        "    }\n"
        "    get_custom_server_from_string(&exe)\n"
        "}",
        "pub fn get_license_from_exe_name() -> ResultType<CustomServer> {\n"
        "    // NervDesk 定制（M4-6 最严格）：屏蔽「exe 文件名携带服务器配置」的覆盖路径\n"
        "    // （改名为 `nervdesk host=xxx,key=xxx.exe` 不再生效），强制使用编译期内置地址。\n"
        "    // 本补丁只进 NervDesk 产物线；官方行为不在该源码上保留。\n"
        "    let _ = std::env::current_exe();\n"
        "    Err(anyhow!(\"NervDesk: exe name server override disabled\"))\n"
        "}",
        "windows.rs get_license_from_exe_name 屏蔽",
    )
    # E7: 清理随之不再使用的 import
    t = sub1(
        t,
        "use crate::{\n"
        "    common::PORTABLE_APPNAME_RUNTIME_ENV_KEY,\n"
        "    custom_server::*,\n"
        "    ipc,\n"
        "    privacy_mode::win_topmost_window::{self, WIN_TOPMOST_INJECTED_PROCESS_EXE},\n"
        "};",
        "use crate::{\n"
        "    custom_server::*,\n"
        "    ipc,\n"
        "    privacy_mode::win_topmost_window::{self, WIN_TOPMOST_INJECTED_PROCESS_EXE},\n"
        "};",
        "windows.rs import 清理",
    )
    write_text(p, t, crlf)
    print("[OK] windows.rs: exe 文件名覆盖屏蔽 + import 清理")


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

    check("libs/hbb_common/src/config.rs", "pub const NERVDESK_RELAY_SERVER", "relay 内置 const")
    check("libs/hbb_common/src/config.rs", "pub const NERVDESK_API_SERVER", "API 内置 const")
    check("libs/hbb_common/src/config.rs", 'pub const NERVDESK_BUILTIN_RELAY_TOKEN: &str = "__NERVDESK_RELAY_TOKEN__"', "relay 令牌占位符")
    check("libs/hbb_common/src/config.rs", "nervdesk_relay_token_marker", "F1 修复：relay 哨兵运行时拼接")
    check("libs/hbb_common/src/config.rs", "NERVDESK_BUILTIN_RELAY_TOKEN != nervdesk_relay_token_marker()", "F1 修复：relay 守卫无完整占位符字面量")
    check("libs/hbb_common/src/config.rs", "pub fn nerve_apply_network_defaults()", "网络固化入口")
    check("libs/hbb_common/src/config.rs", "nerve_apply_network_defaults();", "调用链")
    check("src/common.rs", "// NervDesk 定制（M4-6 最严格）", "rendezvous 读取强制")
    check("src/common.rs", "config::NERVDESK_API_SERVER.to_owned()", "API 兜底 const")
    check("src/platform/windows.rs", "exe name server override disabled", "exe 文件名屏蔽")

    # 反向校验：地址值正确（M4-1/2/3）
    cfg = pathlib.Path("libs/hbb_common/src/config.rs").read_text(encoding="utf-8")
    for addr in ['"api.nervcode.eu.org"', '"https://api.nervcode.eu.org"']:
        if addr not in cfg:
            print(f"[FAIL] 内置地址缺失 {addr}", file=sys.stderr)
            ok = False

    # 占位符纪律：__NERVDESK_RELAY_TOKEN__ 只允许出现在 config.rs 的 const 定义行
    # （CI 替换点）；守卫比较行不得含完整占位符字面量（F1 修复）。
    for f in FILES:
        if f == "libs/hbb_common/src/config.rs":
            continue
        if "__NERVDESK_RELAY_TOKEN__" in pathlib.Path(f).read_text(encoding="utf-8"):
            print(f"[FAIL] 占位符出现在非预期文件：{f}", file=sys.stderr)
            ok = False
    # F1：config.rs 内完整占位符只允许出现在 const 初始化行
    for line in cfg.splitlines():
        if "NERVDESK_BUILTIN_RELAY_TOKEN" in line and "__NERVDESK_RELAY_TOKEN__" in line and "pub const" not in line:
            print(f"[FAIL] relay 守卫/注释行含完整占位符字面量（F1 隐患）：{line.strip()}", file=sys.stderr)
            ok = False

    if not ok:
        sys.exit(1)
    print("[OK] 全部 NervDesk 网络/服务(源码侧)改动校验通过")


def main() -> None:
    print("=== NervDesk 网络与服务补丁（M4 网络 + M7 服务·源码侧）===")
    patch_config_rs()
    patch_common_rs()
    patch_windows_rs()
    verify()
    print("=== 完成 ===")
    print("提示：M7 服务常驻自愈无源码改动，见 tools/nervdesk-service-*.ps1 三个脚本；")
    print("relay 令牌由 CI Secret（NERVDESK_RELAY_TOKEN）在构建期替换 __NERVDESK_RELAY_TOKEN__。")


if __name__ == "__main__":
    main()