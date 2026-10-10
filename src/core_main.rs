#[cfg(any(target_os = "windows", target_os = "macos"))]
use crate::client::translate;
#[cfg(not(debug_assertions))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::platform::breakdown_callback;
use base::config::keys;
#[cfg(not(debug_assertions))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use base::platform::register_breakdown_handler;
use hbb_common::{config, log};
#[cfg(windows)]
use tauri_winrt_notification::{Duration, Sound, Toast};

#[macro_export]
macro_rules! my_println{
    ($($arg:tt)*) => {
        #[cfg(not(windows))]
        println!("{}", format_args!($($arg)*));
        #[cfg(windows)]
        crate::platform::message_box(
            &format!("{}", format_args!($($arg)*))
        );
    };
}

/// shared by flutter and sciter main function
///
/// [Note]
/// If it returns [`None`], then the process will terminate, and flutter gui will not be started.
/// If it returns [`Some`], then the process will continue, and flutter gui will be started.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn core_main() -> Option<Vec<String>> {
    if !crate::common::global_init() {
        return None;
    }
    crate::load_custom_client();
    #[cfg(windows)]
    if !crate::platform::windows::bootstrap() {
        // return None to terminate the process
        return None;
    }
    let mut args = Vec::new();
    let mut flutter_args = Vec::new();
    let mut i = 0;
    let mut _is_elevate = false;
    let mut _is_run_as_system = false;
    let mut _is_quick_support = false;
    let mut _is_flutter_invoke_new_connection = false;
    let mut no_server = false;
    let mut arg_exe = Default::default();
    for arg in std::env::args() {
        if i == 0 {
            arg_exe = arg;
        } else if i > 0 {
            #[cfg(feature = "flutter")]
            if [
                "--connect",
                "--play",
                "--file-transfer",
                "--view-camera",
                "--port-forward",
                "--terminal",
                "--rdp",
            ]
            .contains(&arg.as_str())
            {
                _is_flutter_invoke_new_connection = true;
            }
            if arg == "--elevate" {
                _is_elevate = true;
            } else if arg == "--run-as-system" {
                _is_run_as_system = true;
            } else if arg == "--quick_support" {
                _is_quick_support = true;
            } else if arg == "--no-server" {
                no_server = true;
            } else {
                args.push(arg);
            }
        }
        i += 1;
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    if args.is_empty() {
        #[cfg(target_os = "linux")]
        let should_check_start_tray = crate::check_process("--server", false);
        // We can use `crate::check_process("--server", false)` on Windows.
        // Because `--server` process is the System user's process. We can't get the arguments in `check_process()`.
        // We can assume that self service running means the server is also running on Windows.
        #[cfg(target_os = "windows")]
        let should_check_start_tray = crate::platform::is_self_service_running()
            && crate::platform::is_cur_exe_the_installed();
        if should_check_start_tray && !crate::check_process("--tray", true) {
            #[cfg(target_os = "linux")]
            hbb_common::allow_err!(crate::platform::check_autostart_config());
            hbb_common::allow_err!(crate::run_me(vec!["--tray"]));
        }
    }
    #[cfg(not(debug_assertions))]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    register_breakdown_handler(breakdown_callback);
    #[cfg(target_os = "linux")]
    #[cfg(feature = "flutter")]
    {
        let (k, v) = ("LIBGL_ALWAYS_SOFTWARE", "1");
        if config::option2bool(
            "allow-always-software-render",
            &config::Config::get_option("allow-always-software-render"),
        ) {
            std::env::set_var(k, v);
        } else {
            std::env::remove_var(k);
        }
    }
    #[cfg(windows)]
    if args.contains(&"--connect".to_string()) || args.contains(&"--view-camera".to_string()) {
        base::platform::windows::start_cpu_performance_monitor();
    }
    #[cfg(feature = "flutter")]
    if _is_flutter_invoke_new_connection {
        return core_main_invoke_new_connection(std::env::args());
    }
    let click_setup = cfg!(windows) && args.is_empty() && crate::common::is_setup(&arg_exe);
    if click_setup && !config::is_disable_installation() {
        args.push("--install".to_owned());
        flutter_args.push("--install".to_string());
    }
    if args.contains(&"--noinstall".to_string()) {
        args.clear();
    }
    // The portable wrapper injects `--install` when its name ends with `install.exe`,
    // including `no-install.exe`. Drop the argument instead of exiting so disabled
    // clients can continue running as portable applications.
    if config::is_disable_installation() {
        args.retain(|arg| arg != "--install");
        flutter_args.retain(|arg| arg != "--install");
    }
    if args.len() > 0 {
        if args[0] == "--version" {
            println!("{}", crate::VERSION);
            return None;
        } else if args[0] == "--build-date" {
            println!("{}", crate::BUILD_DATE);
            return None;
        } else if args[0] == "--quic-probe-mode" {
            // P1d-tail-3：仅供 RT-01 / Win7 字节级实测用的调试入口。
            // 不接入产品路径，不改 RustDesk 任何业务流；只跑 C1 RPK + QUIC
            // dial + 字节交换 + 元信息打印，然后退出。调用形态：
            //   hbbndesk-client.exe --quic-probe-mode <peer_ip:port> <peer_pk_hex32>
            // 或：
            //   hbbndesk-client.exe --quic-probe-mode self-check
            //
            // 必须 cfg-gate：与 src/lib.rs:78 `pub mod quic_transport` 同条件，
            // feature-off 时 `crate::quic_transport` 整体不存在，否则 E0433。
            #[cfg(feature = "quic")]
            {
                if let Some(outcome) = crate::quic_transport::run_quic_probe_mode(&args) {
                    std::process::exit(if outcome { 0 } else { 1 });
                }
                return None;
            }
            #[cfg(not(feature = "quic"))]
            {
                eprintln!(
                    "--quic-probe-mode requires the `quic` feature at build time; \
                     rebuild with `cargo ... --features quic`"
                );
                std::process::exit(2);
            }
        } else if args[0] == "--quic-pair-mode" {
            // task-5：跨机 QUIC 实测诊断入口（无头、可机器判定）。
            // 与 `--quic-probe-mode` 的本质差别：复用产品同一 `punch_udp`
            // （src/common.rs:2898），在**同一条已打洞的 UDP socket** 上做 QUIC，
            // 与产品主控侧 udp_nat_connect（src/client.rs:5646）和被控侧
            // udp_nat_listen（src/rendezvous_mediator.rs:1475）同序。
            // 只作诊断入口：不进 Flutter UI、不进任何默认路径。
            // 调用形态：
            //   nervdesk --quic-pair-mode genkey
            //   nervdesk --quic-pair-mode dial   --local-port P --peer ip:port --peer-key <64hex> ...
            //   nervdesk --quic-pair-mode listen --port P [--peer ip:port] ...
            //
            // 必须 cfg-gate：与 src/lib.rs:78 `pub mod quic_transport` 同条件，
            // feature-off 时 `crate::quic_transport` 整体不存在，否则 E0433。
            #[cfg(feature = "quic")]
            {
                if let Some(outcome) = crate::quic_transport::run_quic_pair_mode(&args) {
                    std::process::exit(if outcome { 0 } else { 1 });
                }
                return None;
            }
            #[cfg(not(feature = "quic"))]
            {
                eprintln!(
                    "--quic-pair-mode requires the `quic` feature at build time; \
                     rebuild with `cargo ... --features quic`"
                );
                std::process::exit(2);
            }
        } else if args[0] == "--transport" {
            // P3 Path-A CLI: --transport <name>
            // Maps <name> to NERV_QUIC_MODE env-var that get_quic_mode() (src/common.rs:1259)
            // already reads. The connection-site dispatch at src/client.rs:1467 already honors
            // QuicMode::{Disabled,Prefer,Required}; this CLI is purely a per-launch override
            // that does NOT touch local-option storage. Falls through to Flutter GUI launch
            // (no `return None`) so the user can configure once and connect through GUI.
            //
            // 【2026-10-02 事实更正｜task-73 R3，仅注释，逻辑未动】
            // 上一段「Falls through to Flutter GUI launch」**只对 Flutter 构建成立**。
            // 在 **sciter 构建**（Win7 产物）上，`--transport` 会被 `ui::start` 的合法命令表
            // 判为未知参数，走到 `src/ui.rs:162` 的 `log::error!("Wrong command: {:?}", args);`
            // 后直接 `return`（`src/ui.rs:161-164`）——整个进程**不建立任何连接就退出**。
            // release 产物实测：`--transport quic-required` ⇒ EXIT=0 + 一行
            // `ERROR [src/ui.rs:162] Wrong command`，全程零 QUIC 代码
            //（依据 analysis/network/quic-required-recon.md §3.2 / §4.0，task-66）。
            // ⇒ 本 CLI 目前只在 Flutter 构建上是有效入口；sciter/Win7 上要覆盖 QUIC_MODE，
            //    必须直接设 `NERV_QUIC_MODE` 环境变量（`get_quic_mode()` 优先读它）。
            // 另：上面两个行号亦已复核更正 —— `src/common.rs:1224` → `:1259`；
            //     `src/client.rs:1474` → `:1467`。
            //
            // cfg-gated to mirror --quic-probe-mode (lines 145-169): quic-mode values are only
            // accepted when the binary was built with `--features quic`. feature-off binaries
            // accept only the four WebRTC/WebSocket/Tcp/default values, exactly matching the
            // pre-Path-A behavior (AGENTS.md: feature-off must take original path).
            #[cfg(feature = "quic")]
            {
                match args.get(1).map(String::as_str) {
                    Some("quic-prefer") | Some("quic") => {
                        std::env::set_var("NERV_QUIC_MODE", "prefer");
                    }
                    Some("quic-required") => {
                        std::env::set_var("NERV_QUIC_MODE", "required");
                    }
                    Some("quic-off")
                    | Some("web-rtc")
                    | Some("webrtc")
                    | Some("ws")
                    | Some("websocket")
                    | Some("tcp")
                    | Some("default") => {
                        std::env::set_var("NERV_QUIC_MODE", "disabled");
                    }
                    Some(other) => {
                        eprintln!(
                            "--transport: unknown value {:?}; expected one of \
                             quic-prefer, quic-required, quic-off, web-rtc, ws, tcp, default",
                            other
                        );
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--transport requires a value; expected one of \
                                   quic-prefer, quic-required, quic-off, web-rtc, ws, tcp, default");
                        std::process::exit(2);
                    }
                }
            }
            #[cfg(not(feature = "quic"))]
            {
                match args.get(1).map(String::as_str) {
                    Some("quic-prefer") | Some("quic") | Some("quic-required") => {
                        eprintln!(
                            "--transport: quic modes require the `quic` feature at build time; \
                             rebuild with `cargo ... --features quic`"
                        );
                        std::process::exit(2);
                    }
                    Some("quic-off")
                    | Some("web-rtc")
                    | Some("webrtc")
                    | Some("ws")
                    | Some("websocket")
                    | Some("tcp")
                    | Some("default") => {
                        // No-op: default build already maps to these transports.
                    }
                    Some(other) => {
                        eprintln!(
                            "--transport: unknown value {:?}; expected one of \
                             web-rtc, ws, tcp, default (quic modes require --features quic)",
                            other
                        );
                        std::process::exit(2);
                    }
                    None => {
                        eprintln!("--transport requires a value");
                        std::process::exit(2);
                    }
                }
            }
        }
    }
    #[cfg(windows)]
    {
        _is_quick_support |= !crate::platform::is_installed()
            && args.is_empty()
            && (is_quick_support_exe(&arg_exe)
                || config::LocalConfig::get_option("pre-elevate-service") == "Y"
                || (!click_setup && crate::platform::is_elevated(None).unwrap_or(false)));
        crate::portable_service::client::set_quick_support(_is_quick_support);
    }
    let mut log_name = "".to_owned();
    // Keep portable-service logs under a stable directory name.
    let has_portable_service_shmem_arg = args
        .iter()
        .any(|arg| arg.starts_with("--portable-service-shmem-name="));
    if has_portable_service_shmem_arg {
        log_name = "portable-service".to_owned();
    } else if args.len() > 0 && args[0].starts_with("--") {
        let name = args[0].replace("--", "");
        if !name.is_empty() {
            log_name = name;
        }
    }
    hbb_common::init_log(false, &log_name);

    // P6（plan.md §12）：QUIC 档位在进程启动时确定，且本进程只打这一行 —— 排查跨机问题时
    // 不必再猜「这次跑的到底是哪个档位」。来源优先级：NERV_QUIC_MODE 环境变量 > 本地选项 quic-mode。
    // 必须放在 init_log **之后**：logger 未装好时 log::max_level()==Off，这一行会被静默丢弃
    // （独立复核 944177453 的 N1 就是这个问题 —— 原位置在 init_log 之前的 :40）。
    #[cfg(feature = "quic")]
    log::info!(
        "QUIC 档位={}（启动时确定，本次进程内不再变化）",
        crate::common::get_quic_mode().as_str()
    );

    // linux uni (url) go here.
    #[cfg(all(target_os = "linux", feature = "flutter"))]
    if args.len() > 0 && args[0].starts_with(&crate::get_uri_prefix()) {
        return try_send_by_dbus(args[0].clone());
    }

    #[cfg(windows)]
    if !crate::platform::is_installed()
        && args.is_empty()
        && _is_quick_support
        && !_is_elevate
        && !_is_run_as_system
    {
        use crate::portable_service::client;
        if let Err(e) = client::start_portable_service(client::StartPara::Direct) {
            log::error!("Failed to start portable service: {:?}", e);
        }
    }
    #[cfg(windows)]
    if !crate::platform::is_installed() && (_is_elevate || _is_run_as_system) {
        crate::platform::elevate_or_run_as_system(click_setup, _is_elevate, _is_run_as_system);
        return None;
    }
    if args.is_empty() || crate::common::is_empty_uni_link(&args[0]) {
        #[cfg(target_os = "macos")]
        {
            crate::platform::macos::try_remove_temp_update_dir(None);
        }

        #[cfg(windows)]
        {
            crate::platform::try_remove_temp_update_files();
            hbb_common::config::PeerConfig::preload_peers();
        }
        std::thread::spawn(move || crate::start_server(false, no_server));
    } else {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        // Root CLI management commands must talk to the user `--server` main IPC.
        // Example: `sudo rustdesk --option custom-rendezvous-server` should query the
        // user's IPC instead of root's `/tmp/<app>-0/ipc`; `connect()` still limits this
        // routing to empty-postfix main IPC only.
        let _user_main_ipc_scope = if crate::platform::is_installed()
            && is_root()
            && is_user_main_ipc_scope_cli_command(&args)
        {
            Some(crate::ipc::UserMainIpcScope::new())
        } else {
            None
        };

        #[cfg(windows)]
        {
            use crate::platform;
            if args[0] == "--uninstall" {
                if let Err(err) = platform::uninstall_me(true) {
                    log::error!("Failed to uninstall: {}", err);
                }
                return None;
            } else if args[0] == "--update" {
                if config::is_disable_installation() {
                    return None;
                }

                let text = match crate::platform::prepare_custom_client_update() {
                    Err(e) => {
                        log::error!("Error preparing custom client update: {}", e);
                        "Update failed!".to_string()
                    }
                    Ok(false) => "Update failed!".to_string(),
                    Ok(true) => match platform::update_me(false) {
                        Ok(_) => "Updated successfully!".to_string(),
                        Err(err) => {
                            log::error!("Failed with error: {err}");
                            "Update failed!".to_string()
                        }
                    },
                };
                Toast::new(Toast::POWERSHELL_APP_ID)
                    .title(&config::APP_NAME.read().unwrap())
                    .text1(&translate(text))
                    .sound(Some(Sound::Default))
                    .duration(Duration::Short)
                    .show()
                    .ok();
                return None;
            } else if args[0] == "--after-install" {
                if let Err(err) = platform::run_after_install() {
                    log::error!("Failed to after-install: {}", err);
                }
                return None;
            } else if args[0] == "--before-uninstall" {
                if let Err(err) = platform::run_before_uninstall() {
                    log::error!("Failed to before-uninstall: {}", err);
                }
                return None;
            } else if args[0] == "--silent-install" {
                if config::is_disable_installation() {
                    return None;
                }
                let (printer_override, debug) = parse_silent_install_args(&args);
                let options = platform::get_silent_install_options(printer_override);
                let res = platform::install_me(options, "".to_owned(), true, debug);
                let text = match res {
                    Ok(_) => translate("Installation Successful!".to_string()),
                    Err(err) => {
                        println!("Failed with error: {err}");
                        translate("Installation failed!".to_string())
                    }
                };
                Toast::new(Toast::POWERSHELL_APP_ID)
                    .title(&config::APP_NAME.read().unwrap())
                    .text1(&text)
                    .sound(Some(Sound::Default))
                    .duration(Duration::Short)
                    .show()
                    .ok();
                return None;
            } else if args[0] == "--uninstall-cert" {
                #[cfg(windows)]
                hbb_common::allow_err!(crate::platform::windows::uninstall_cert());
                return None;
            } else if args[0] == "--install-idd" {
                #[cfg(windows)]
                if crate::virtual_display_manager::is_virtual_display_supported() {
                    hbb_common::allow_err!(
                        crate::virtual_display_manager::rustdesk_idd::install_update_driver()
                    );
                }
                return None;
            } else if args[0] == "--portable-service" {
                crate::platform::elevate_or_run_as_system(
                    click_setup,
                    _is_elevate,
                    _is_run_as_system,
                );
                return None;
            } else if args[0] == "--uninstall-amyuni-idd" {
                #[cfg(windows)]
                hbb_common::allow_err!(
                    crate::virtual_display_manager::amyuni_idd::uninstall_driver()
                );
                return None;
            } else if args[0] == "--install-remote-printer" {
                #[cfg(windows)]
                if crate::platform::is_win_10_or_greater() {
                    match remote_printer::install_update_printer(&crate::get_app_name()) {
                        Ok(_) => {
                            log::info!("Remote printer installed/updated successfully");
                        }
                        Err(e) => {
                            log::error!("Failed to install/update the remote printer: {}", e);
                        }
                    }
                } else {
                    log::error!("Win10 or greater required!");
                }
                return None;
            } else if args[0] == "--uninstall-remote-printer" {
                #[cfg(windows)]
                if crate::platform::is_win_10_or_greater() {
                    remote_printer::uninstall_printer(&crate::get_app_name());
                    log::info!("Remote printer uninstalled");
                }
                return None;
            }
        }
        #[cfg(target_os = "macos")]
        {
            use crate::platform;
            if args[0] == "--update" {
                if args.len() > 1 && args[1].ends_with(".dmg") {
                    // Version check is unnecessary unless downgrading to an older version
                    // that lacks "update dmg" support. This is a special case since we cannot
                    // detect the version before extracting the DMG, so we skip the check.
                    let dmg_path = &args[1];
                    println!("Updating from DMG: {}", dmg_path);
                    match platform::update_from_dmg(dmg_path) {
                        Ok(_) => {
                            println!("Update process from DMG started successfully.");
                            // The new process will handle the rest. We can exit.
                        }
                        Err(err) => {
                            eprintln!("Failed to start update from DMG: {}", err);
                        }
                    }
                } else {
                    println!("Starting update process...");
                    log::info!("Starting update process...");
                    let _text = match platform::update_me() {
                        Ok(_) => {
                            println!("{}", translate("Updated successfully!".to_string()));
                            log::info!("Updated successfully!");
                        }
                        Err(err) => {
                            eprintln!("Update failed with error: {}", err);
                            log::error!("Update failed with error: {err}");
                        }
                    };
                }
                return None;
            }
        }
        if args[0] == "--remove" {
            if args.len() == 2 {
                // sleep a while so that process of removed exe exit
                std::thread::sleep(std::time::Duration::from_secs(1));
                std::fs::remove_file(&args[1]).ok();
                return None;
            }
        } else if args[0] == "--tray" {
            if !crate::check_process("--tray", true) {
                crate::tray::start_tray();
            }
            return None;
        } else if args[0] == "--install-service" {
            log::info!("start --install-service");
            crate::platform::install_service();
            return None;
        } else if args[0] == "--uninstall-service" {
            log::info!("start --uninstall-service");
            crate::platform::uninstall_service(false, true);
            return None;
        } else if args[0] == "--service" {
            log::info!("start --service");
            crate::start_os_service();
            return None;
        } else if args[0] == "--server" {
            log::info!("start --server with user {}", crate::username());
            #[cfg(target_os = "linux")]
            {
                hbb_common::allow_err!(crate::platform::check_autostart_config());
                // The tray process is named after the real executable, which on
                // Linux is neither the display name nor the machine identifier.
                if let Some(exe_name) = crate::platform::linux::current_exe_name_lower() {
                    std::process::Command::new("pkill")
                        .arg("-f")
                        .arg(&format!("{exe_name} --tray"))
                        .status()
                        .ok();
                }
                hbb_common::allow_err!(crate::run_me(vec!["--tray"]));
            }
            #[cfg(windows)]
            crate::privacy_mode::restore_reg_connectivity(true, false);
            #[cfg(any(target_os = "linux", target_os = "windows"))]
            {
                crate::start_server(true, false);
            }
            #[cfg(target_os = "macos")]
            {
                let handler = std::thread::spawn(move || crate::start_server(true, false));
                crate::tray::start_tray();
                // prevent server exit when encountering errors from tray
                hbb_common::allow_err!(handler.join());
            }
            return None;
        } else if args[0] == "--import-config" {
            if args.len() == 2 {
                let filepath;
                let path = std::path::Path::new(&args[1]);
                if !path.is_absolute() {
                    let mut cur = std::env::current_dir().unwrap();
                    cur.push(path);
                    filepath = cur.to_str().unwrap().to_string();
                } else {
                    filepath = path.to_str().unwrap().to_string();
                }
                import_config(&filepath);
            }
            return None;
        } else if args[0] == "--password" {
            if is_cli_setting_change_disabled() {
                crate::my_println!("Settings are disabled!");
                return None;
            }
            let locked = config::Config::is_disable_change_permanent_password();
            if args.len() == 2 {
                if crate::platform::is_installed() && is_root() {
                    // A NERV Desk build factory-locks the permanent password, but this
                    // installed-administrator CLI is deliberately kept as the one way to
                    // set it once, so it goes through the daemon's administrator channel.
                    let result = if locked {
                        crate::ipc::set_permanent_password_as_admin(args[1].to_owned())
                    } else {
                        crate::ipc::set_permanent_password(args[1].to_owned())
                    };
                    // `my_println!` exists precisely because a release Windows build is a
                    // GUI-subsystem binary (`windows_subsystem = "windows"` in src/main.rs):
                    // it has no console, so a bare `println!` writes nowhere and the operator
                    // cannot tell "Done!" from a rejection. The macro shows a message box on
                    // Windows and keeps printing to stdout everywhere else.
                    if let Err(err) = result {
                        crate::my_println!("{err}");
                    } else {
                        crate::my_println!("Done!");
                    }
                } else {
                    // Also the factory-locked case: the lock is lifted for exactly one
                    // invocation shape (installed build, launched from an elevated prompt),
                    // so a missing privilege or a portable copy must not be reported as a
                    // disabled feature.
                    crate::my_println!("Installation and administrative privileges required!");
                }
            }
            return None;
        } else if args[0] == "--set-unlock-pin" {
            // FIX-03: this branch must use `my_println!`, not `println!`. A release Windows
            // build is a GUI subsystem binary (see `src/main.rs`), so it has no console: every
            // `println!` here is silently discarded and the user sees *nothing* after a
            // successful `--set-unlock-pin`. `my_println!` shows a message box on Windows and
            // still writes to stdout elsewhere, exactly like the `--password` branch above.
            if config::Config::is_disable_unlock_pin() {
                crate::my_println!("Unlock PIN is disabled!");
                return None;
            }
            #[cfg(feature = "flutter")]
            if args.len() == 2 {
                if crate::platform::is_installed() && is_root() {
                    if let Err(err) = crate::ipc::set_unlock_pin(args[1].to_owned(), false) {
                        crate::my_println!("{err}");
                    } else {
                        crate::my_println!("Done!");
                    }
                } else {
                    crate::my_println!("Installation and administrative privileges required!");
                }
            }
            return None;
        } else if args[0] == "--get-id" {
            println!("{}", crate::ipc::get_id());
            return None;
        } else if args[0] == "--set-id" {
            if is_cli_setting_change_disabled() {
                println!("Settings are disabled!");
                return None;
            }
            if config::Config::is_disable_change_id() {
                println!("Changing ID is disabled!");
                return None;
            }
            if args.len() == 2 {
                if crate::platform::is_installed() && is_root() {
                    let old_id = crate::ipc::get_id();
                    let mut res = crate::ui_interface::change_id_shared(args[1].to_owned(), old_id);
                    if res.is_empty() {
                        res = "Done!".to_owned();
                    }
                    println!("{}", res);
                } else {
                    println!("Installation and administrative privileges required!");
                }
            }
            return None;
        } else if args[0] == "--config" {
            if args.len() == 2 && !args[0].contains("host=") {
                if crate::platform::is_installed() && is_root() {
                    // encrypted string used in renaming exe.
                    let name = if args[1].ends_with(".exe") {
                        args[1].to_owned()
                    } else {
                        format!("{}.exe", args[1])
                    };
                    if let Ok(lic) = crate::custom_server::get_custom_server_from_string(&name) {
                        if !lic.host.is_empty() {
                            crate::ui_interface::set_option("key".into(), lic.key);
                            crate::ui_interface::set_option(
                                "custom-rendezvous-server".into(),
                                lic.host,
                            );
                            crate::ui_interface::set_option("api-server".into(), lic.api);
                            crate::ui_interface::set_option("relay-server".into(), lic.relay);
                        }
                    }
                } else {
                    println!("Installation and administrative privileges required!");
                }
            }
            return None;
        } else if args[0] == "--option" {
            if is_cli_setting_change_disabled() {
                println!("Settings are disabled!");
                return None;
            }
            if crate::platform::is_installed() && is_root() {
                if args.len() == 2 {
                    let options = crate::ipc::get_options();
                    println!("{}", options.get(&args[1]).unwrap_or(&"".to_owned()));
                } else if args.len() == 3 {
                    crate::ipc::set_option(&args[1], &args[2]);
                }
            } else {
                println!("Installation and administrative privileges required!");
            }
            return None;
        } else if args[0] == "--assign" {
            if config::Config::no_register_device() {
                println!("Cannot assign an unregistrable device!");
            } else if crate::platform::is_installed() && is_root() {
                let max = args.len() - 1;
                let pos = args.iter().position(|x| x == "--token").unwrap_or(max);
                if pos < max {
                    let token = args[pos + 1].to_owned();
                    let id = crate::ipc::get_id();
                    let uuid = crate::encode64(hbb_common::get_uuid());
                    let get_value = |c: &str| {
                        let pos = args.iter().position(|x| x == c).unwrap_or(max);
                        if pos < max {
                            Some(args[pos + 1].to_owned())
                        } else {
                            None
                        }
                    };
                    let user_name = get_value("--user_name");
                    let strategy_name = get_value("--strategy_name");
                    let address_book_name = get_value("--address_book_name");
                    let address_book_tag = get_value("--address_book_tag");
                    let address_book_alias = get_value("--address_book_alias");
                    let address_book_password = get_value("--address_book_password");
                    let address_book_note = get_value("--address_book_note");
                    let device_group_name = get_value("--device_group_name");
                    let note = get_value("--note");
                    let device_username = get_value("--device_username");
                    let device_name = get_value("--device_name");
                    let mut body = serde_json::json!({
                        "id": id,
                        "uuid": uuid,
                    });
                    let header = "Authorization: Bearer ".to_owned() + &token;
                    if user_name.is_none()
                        && strategy_name.is_none()
                        && address_book_name.is_none()
                        && device_group_name.is_none()
                        && note.is_none()
                        && device_username.is_none()
                        && device_name.is_none()
                    {
                        println!(
                            r#"At least one of the following options is required:
  --user_name
  --strategy_name
  --address_book_name
  --device_group_name
  --note
  --device_username
  --device_name"#
                        );
                    } else {
                        if let Some(name) = user_name {
                            body["user_name"] = serde_json::json!(name);
                        }
                        if let Some(name) = strategy_name {
                            body["strategy_name"] = serde_json::json!(name);
                        }
                        if let Some(name) = address_book_name {
                            body["address_book_name"] = serde_json::json!(name);
                            if let Some(name) = address_book_tag {
                                body["address_book_tag"] = serde_json::json!(name);
                            }
                            if let Some(name) = address_book_alias {
                                body["address_book_alias"] = serde_json::json!(name);
                            }
                            if let Some(name) = address_book_password {
                                body["address_book_password"] = serde_json::json!(name);
                            }
                            if let Some(name) = address_book_note {
                                body["address_book_note"] = serde_json::json!(name);
                            }
                        }
                        if let Some(name) = device_group_name {
                            body["device_group_name"] = serde_json::json!(name);
                        }
                        if let Some(name) = note {
                            body["note"] = serde_json::json!(name);
                        }
                        if let Some(name) = device_username {
                            body["device_username"] = serde_json::json!(name);
                        }
                        if let Some(name) = device_name {
                            body["device_name"] = serde_json::json!(name);
                        }
                        let url = crate::ui_interface::get_api_server() + "/api/devices/cli";
                        match crate::post_request_sync(url, body.to_string(), &header) {
                            Err(err) => println!("{}", err),
                            Ok(text) => {
                                if text.is_empty() {
                                    println!("Done!");
                                } else {
                                    println!("{}", text);
                                }
                            }
                        }
                    }
                } else {
                    println!("--token is required!");
                }
            } else {
                println!("Installation and administrative privileges required!");
            }
            return None;
        } else if args[0] == "--deploy" {
            if config::Config::no_register_device() {
                println!("Cannot deploy an unregistrable device!");
            } else if config::is_outgoing_only() {
                println!("Cannot deploy Outgoing-only clients.");
            } else if crate::platform::is_installed() && is_root() {
                let max = args.len() - 1;
                let pos = args.iter().position(|x| x == "--token").unwrap_or(max);
                if pos >= max {
                    println!("--token is required!");
                    return None;
                }
                let token = args[pos + 1].to_owned();
                let get_value = |c: &str| {
                    let pos = args.iter().position(|x| x == c).unwrap_or(max);
                    if pos < max {
                        Some(args[pos + 1].to_owned())
                    } else {
                        None
                    }
                };
                // An empty --id (e.g. an unset var) would deploy a blank id; the Android flow guards this too (#15146).
                let new_id = get_value("--id").filter(|s| !s.is_empty());
                match crate::ui_interface::deploy_device(token, new_id) {
                    crate::ui_interface::DeployResult::Ok => {
                        println!("Device deployed.");
                    }
                    crate::ui_interface::DeployResult::NotEnabled => {
                        println!("Server does not require deployment.");
                        std::process::exit(3);
                    }
                    crate::ui_interface::DeployResult::InvalidInput => {
                        println!("Invalid input.");
                        std::process::exit(5);
                    }
                    crate::ui_interface::DeployResult::IdTaken(id) => {
                        println!(
                            "Id `{}` is already used by another machine on the server.",
                            id
                        );
                        std::process::exit(6);
                    }
                    crate::ui_interface::DeployResult::Error(err) => {
                        println!("{}", err);
                        std::process::exit(1);
                    }
                }
            } else {
                println!("Installation and administrative privileges required!");
            }
            return None;
        } else if args[0] == "--check-hwcodec-config" {
            #[cfg(feature = "hwcodec")]
            crate::ipc::hwcodec_process();
            return None;
        } else if args[0] == "--terminal-helper" {
            // Terminal helper process - runs as user to create ConPTY
            // This is needed because ConPTY has compatibility issues with CreateProcessAsUserW
            #[cfg(target_os = "windows")]
            {
                let helper_args: Vec<String> = args[1..].to_vec();
                if let Err(e) = crate::server::terminal_helper::run_terminal_helper(&helper_args) {
                    log::error!("Terminal helper failed: {}", e);
                }
            }
            return None;
        } else if args[0] == "--cm" {
            // call connection manager to establish connections
            // meanwhile, return true to call flutter window to show control panel
            crate::ui_interface::start_option_status_sync();
        } else if args[0] == "--whiteboard" {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                crate::whiteboard::run();
            }
            return None;
        } else if args[0] == "-gtk-sudo" {
            // rustdesk service kill `rustdesk --` processes
            #[cfg(target_os = "linux")]
            if args.len() > 2 {
                crate::platform::gtk_sudo::exec();
            }
            return None;
        }
    }
    //_async_logger_holder.map(|x| x.flush());
    #[cfg(feature = "flutter")]
    return Some(flutter_args);
    #[cfg(not(feature = "flutter"))]
    return Some(args);
}

fn import_config(path: &str) {
    use hbb_common::{config::*, get_exe_time, get_modified_time};
    let path2 = path.replace(".toml", "2.toml");
    let path2 = std::path::Path::new(&path2);
    let path = std::path::Path::new(path);
    log::info!("import config from {:?} and {:?}", path, path2);
    let config: Config = load_path(path.into());
    if config.is_empty() {
        log::info!("Empty source config, skipped");
        return;
    }
    if get_modified_time(&path) > get_modified_time(&Config::file())
        && get_modified_time(&path) < get_exe_time()
    {
        if store_path(Config::file(), config).is_err() {
            log::info!("config written");
        }
    }
    let config2: Config2 = load_path(path2.into());
    if get_modified_time(&path2) > get_modified_time(&Config2::file()) {
        if store_path(Config2::file(), config2).is_err() {
            log::info!("config2 written");
        }
    }
}

/// invoke a new connection
///
/// [Note]
/// this is for invoke new connection from dbus.
/// If it returns [`None`], then the process will terminate, and flutter gui will not be started.
/// If it returns [`Some`], then the process will continue, and flutter gui will be started.
#[cfg(feature = "flutter")]
fn core_main_invoke_new_connection(mut args: std::env::Args) -> Option<Vec<String>> {
    let mut authority = None;
    let mut id = None;
    let mut param_array = vec![];
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" | "--play" | "--file-transfer" | "--view-camera" | "--port-forward"
            | "--terminal" | "--rdp" => {
                authority = Some((&arg.to_string()[2..]).to_owned());
                id = args.next();
            }
            "--password" => {
                if let Some(password) = args.next() {
                    param_array.push(format!("password={password}"));
                }
            }
            "--relay" => {
                param_array.push(format!("relay=true"));
            }
            // inner
            "--switch_uuid" => {
                if let Some(switch_uuid) = args.next() {
                    param_array.push(format!("switch_uuid={switch_uuid}"));
                }
            }
            _ => {}
        }
    }
    let mut uni_links = Default::default();
    if let Some(authority) = authority {
        if let Some(mut id) = id {
            // `id` may carry the portable-config extension (see
            // `ui_session_interface.rs`): strip the machine-facing identifier,
            // not the runtime display name.
            let ext = format!(".{}", hbb_common::config::APP_NAME_IDENT);
            if id.ends_with(&ext) {
                id = id.replace(&ext, "");
            }
            let params = param_array.join("&");
            let params_flag = if params.is_empty() { "" } else { "?" };
            uni_links = format!(
                "{}{}/{}{}{}",
                crate::get_uri_prefix(),
                authority,
                id,
                params_flag,
                params
            );
        }
    }
    if uni_links.is_empty() {
        return None;
    }

    #[cfg(target_os = "linux")]
    return try_send_by_dbus(uni_links);

    #[cfg(windows)]
    {
        use winapi::um::winuser::WM_USER;
        let res = crate::platform::send_message_to_hnwd(
            &crate::platform::FLUTTER_RUNNER_WIN32_WINDOW_CLASS,
            &crate::get_app_name(),
            (WM_USER + 2) as _, // referred from unilinks desktop pub
            uni_links.as_str(),
            false,
        );
        return if res { None } else { Some(Vec::new()) };
    }
    #[cfg(target_os = "macos")]
    {
        return if let Err(_) = crate::ipc::send_url_scheme(uni_links) {
            Some(Vec::new())
        } else {
            None
        };
    }
}

#[cfg(all(target_os = "linux", feature = "flutter"))]
fn try_send_by_dbus(uni_links: String) -> Option<Vec<String>> {
    use crate::dbus::invoke_new_connection;

    match invoke_new_connection(uni_links) {
        Ok(()) => {
            return None;
        }
        Err(err) => {
            log::error!("{}", err.as_ref());
            // return Some to invoke this url by self
            return Some(Vec::new());
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_root() -> bool {
    #[cfg(windows)]
    {
        return crate::platform::is_elevated(None).unwrap_or_default()
            || crate::platform::is_root();
    }
    #[allow(unreachable_code)]
    crate::platform::is_root()
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn is_user_main_ipc_scope_cli_command(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("--password")
            | Some("--set-unlock-pin")
            | Some("--get-id")
            | Some("--set-id")
            | Some("--config")
            | Some("--option")
            | Some("--assign")
            | Some("--deploy")
    )
}

#[inline]
fn is_cli_setting_change_disabled() -> bool {
    let option = keys::OPTION_ALLOW_COMMAND_LINE_SETTINGS_WHEN_SETTINGS_DISABLED;
    let allow_command_line_settings =
        config::option2bool(option, &crate::get_builtin_option(option));
    config::is_disable_settings() && !allow_command_line_settings
}

#[cfg(windows)]
fn parse_silent_install_args(args: &[String]) -> (Option<bool>, bool) {
    let mut printer_override = None;
    let mut debug = false;

    for arg in args.iter().skip(1) {
        match arg.as_str() {
            "printer=1" => printer_override = Some(true),
            "printer=0" => printer_override = Some(false),
            "debug" => debug = true,
            _ => {}
        }
    }

    (printer_override, debug)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn user_main_ipc_scope_cli_command_matches_management_commands_only() {
        for command in [
            "--password",
            "--set-unlock-pin",
            "--get-id",
            "--set-id",
            "--config",
            "--option",
            "--assign",
            "--deploy",
        ] {
            assert!(is_user_main_ipc_scope_cli_command(&args(&[command])));
        }

        for command in [
            "--service",
            "--server",
            "--tray",
            "--cm",
            "--check-hwcodec-config",
            "--connect",
        ] {
            assert!(!is_user_main_ipc_scope_cli_command(&args(&[command])));
        }
    }
}

/// Check if the executable is a Quick Support version.
/// Note: This function must be kept in sync with `libs/portable/src/main.rs`.
#[cfg(windows)]
#[inline]
fn is_quick_support_exe(exe: &str) -> bool {
    let exe = exe.to_lowercase();
    exe.contains("-qs-") || exe.contains("-qs.exe") || exe.contains("_qs.exe")
}
