use crate::client::translate;
#[cfg(windows)]
use crate::ipc::Data;
#[cfg(windows)]
use hbb_common::tokio;
use hbb_common::{allow_err, log};
use base::config::keys;
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use std::time::Duration;

/// How often the tray process re-reads the taskbar colour.
///
/// The tray process owns no window, so tao never delivers a theme change to it
/// (`WM_WININICHANGE` only reaches window subclasses); polling the same registry
/// value is the only way to follow the taskbar. One registry read every couple
/// of seconds is cheap enough to be invisible.
#[cfg(windows)]
const TRAY_THEME_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Rate limit for the faults reported while reading the taskbar colour: the read
/// runs on every poll, so a broken setting must not write one line every 2s.
#[cfg(windows)]
const TRAY_THEME_LOG_INTERVAL: Duration = Duration::from_secs(600);

pub fn start_tray() {
    if crate::ui_interface::get_builtin_option(keys::OPTION_HIDE_TRAY) == "Y" {
        #[cfg(not(target_os = "macos"))]
        {
            return;
        }
    }

    #[cfg(target_os = "linux")]
    crate::server::check_zombie();

    allow_err!(make_tray());
}

fn make_tray() -> hbb_common::ResultType<()> {
    // https://github.com/tauri-apps/tray-icon/blob/dev/examples/tao.rs
    use hbb_common::anyhow::Context;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem},
        TrayIcon, TrayIconBuilder, TrayIconEvent as TrayEvent,
    };

    // Duplicated tray icons kept piling up through the blind spots of
    // `check_process("--tray", ..)`. https://github.com/rustdesk/rustdesk/issues/15689
    #[cfg(windows)]
    if !crate::platform::windows::try_lock_tray_single_instance() {
        log::info!("Another tray process is already running in this session, exit");
        return Ok(());
    }

    let icon;
    #[cfg(target_os = "macos")]
    {
        icon = include_bytes!("../res/mac-tray-dark-x2.png"); // use as template, so color is not important
    }
    #[cfg(not(target_os = "macos"))]
    {
        icon = include_bytes!("../res/tray-icon.ico");
    }

    let (icon_rgba, icon_width, icon_height) = {
        let image = load_icon_from_asset()
            .unwrap_or(image::load_from_memory(icon).context("Failed to open icon path")?)
            .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };
    let icon = tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .context("Failed to open icon")?;

    // The tray icon has two monochrome silhouettes, one per taskbar colour (white
    // for a dark taskbar, #1A1A1A for a light one). `themed_tray_icon` picks one
    // and returns `None` when the taskbar colour cannot be determined; the icon
    // built above is kept unchanged in that case.
    #[cfg(not(target_os = "macos"))]
    let taskbar_light = taskbar_is_light();
    #[cfg(not(target_os = "macos"))]
    log::info!(
        "Taskbar colour: {}",
        match taskbar_light {
            Some(true) => "light",
            Some(false) => "dark",
            None => "undetermined, keeping the current tray icon",
        }
    );
    #[cfg(not(target_os = "macos"))]
    let icon = taskbar_light.and_then(themed_tray_icon).unwrap_or(icon);
    // State for the taskbar-colour poll in the event loop below: when the colour
    // was last read, and the value that read observed.
    #[cfg(windows)]
    let mut last_theme_check = std::time::Instant::now();
    #[cfg(windows)]
    let mut last_taskbar_light = taskbar_light;

    let mut event_loop = EventLoopBuilder::new().build();

    let tray_menu = Menu::new();
    let hide_stop_service = crate::ui_interface::get_builtin_option(
        keys::OPTION_HIDE_STOP_SERVICE,
    ) == "Y";
    // The tray icon is only shown when the service is running, so we don't need to check
    // the `stop-service` option here.
    let quit_i = if !hide_stop_service {
        Some(MenuItem::new(translate("Stop service".to_owned()), true, None))
    } else {
        None
    };
    let open_i = MenuItem::new(translate("Open".to_owned()), true, None);
    if let Some(quit_i) = &quit_i {
        tray_menu.append_items(&[&open_i, quit_i]).ok();
    } else {
        tray_menu.append_items(&[&open_i]).ok();
    }
    let tooltip = |count: usize| {
        if count == 0 {
            format!(
                "{} {}",
                crate::get_app_name(),
                translate("Service is running".to_owned()),
            )
        } else {
            format!(
                "{} - {}\n{}",
                crate::get_app_name(),
                translate("Ready".to_owned()),
                translate("{".to_string() + &format!("{count}") + "} sessions"),
            )
        }
    };
    let mut _tray_icon: Arc<Mutex<Option<TrayIcon>>> = Default::default();

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayEvent::receiver();
    #[cfg(windows)]
    let (ipc_sender, ipc_receiver) = std::sync::mpsc::channel::<Data>();

    let open_func = move || {
        if cfg!(not(feature = "flutter")) {
            crate::run_me::<&str>(vec![]).ok();
            return;
        }
        #[cfg(target_os = "macos")]
        crate::platform::macos::handle_application_should_open_untitled_file();
        #[cfg(target_os = "windows")]
        {
            // Do not use "start uni link" way, it may not work on some Windows, and pop out error
            // dialog, I found on one user's desktop, but no idea why, Windows is shit.
            // Use `run_me` instead.
            // `allow_multiple_instances` in `flutter/windows/runner/main.cpp` allows only one instance without args.
            crate::run_me::<&str>(vec![]).ok();
        }
        #[cfg(target_os = "linux")]
        {
            // Do not use "xdg-open", it won't read the config.
            if crate::dbus::invoke_new_connection(crate::get_uri_prefix()).is_err() {
                if let Ok(task) = crate::run_me::<&str>(vec![]) {
                    crate::server::CHILD_PROCESS.lock().unwrap().push(task);
                }
            }
        }
    };

    #[cfg(windows)]
    std::thread::spawn(move || {
        start_query_session_count(ipc_sender.clone());
    });
    #[cfg(windows)]
    let mut last_click = std::time::Instant::now();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::EventLoopExtMacOS;
        event_loop.set_activation_policy(tao::platform::macos::ActivationPolicy::Accessory);
    }
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(100),
        );

        if let tao::event::Event::NewEvents(tao::event::StartCause::Init) = event {
            // for fixing https://github.com/rustdesk/rustdesk/discussions/10210#discussioncomment-14600745
            // so we start tray, but not to show it
            if crate::ui_interface::get_builtin_option(keys::OPTION_HIDE_TRAY) == "Y" {
                return;
            }
            // We create the icon once the event loop is actually running
            // to prevent issues like https://github.com/tauri-apps/tray-icon/issues/90
            let mut builder = TrayIconBuilder::new()
                .with_id(crate::get_app_name().to_lowercase())
                .with_menu(Box::new(tray_menu.clone()))
                .with_tooltip(tooltip(0))
                .with_icon(icon.clone());
            #[cfg(target_os = "macos")]
            {
                builder = builder.with_icon_as_template(true);
            }
            #[cfg(target_os = "windows")]
            {
                // Required since tray-icon 0.17
                // Fixes #15215, #15222, #15410
                builder = builder.with_menu_on_left_click(false);
            }
            let tray = builder.build();
            match tray {
                Ok(tray) => _tray_icon = Arc::new(Mutex::new(Some(tray))),
                Err(err) => {
                    log::error!("Failed to create tray icon: {}", err);
                }
            };

            // We have to request a redraw here to have the icon actually show up.
            // Tao only exposes a redraw method on the Window so we use core-foundation directly.
            #[cfg(target_os = "macos")]
            unsafe {
                use core_foundation::runloop::{CFRunLoopGetMain, CFRunLoopWakeUp};

                let rl = CFRunLoopGetMain();
                CFRunLoopWakeUp(rl);
            }
        }

        if let Ok(event) = menu_channel.try_recv() {
            if let Some(quit_i) = &quit_i {
                if event.id == quit_i.id() {
                    /* failed in windows, seems no permission to check system process
                    if !crate::check_process("--server", false) {
                        *control_flow = ControlFlow::Exit;
                        return;
                    }
                    */
                    // Remove the icon first: on success `uninstall_service()` ends
                    // this process with `std::process::exit`, which skips the
                    // destructor that would remove it, leaving a ghost icon behind.
                    #[cfg(windows)]
                    let _ = _tray_icon
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|t| t.set_visible(false));
                    if !crate::platform::uninstall_service(false, false) {
                        *control_flow = ControlFlow::Exit;
                    }
                    // Still alive, so stopping the service failed or was cancelled
                    // in the UAC prompt. Show the icon again.
                    #[cfg(windows)]
                    let _ = _tray_icon
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|t| t.set_visible(true));
                } else if event.id == open_i.id() {
                    open_func();
                }
            } else if event.id == open_i.id() {
                open_func();
            }
        }

        if let Ok(_event) = tray_channel.try_recv() {
            #[cfg(target_os = "windows")]
            match _event {
                TrayEvent::Click {
                    button,
                    button_state,
                    ..
                } => {
                    if button == tray_icon::MouseButton::Left
                        && button_state == tray_icon::MouseButtonState::Up
                    {
                        if last_click.elapsed() < std::time::Duration::from_secs(1) {
                            return;
                        }
                        open_func();
                        last_click = std::time::Instant::now();
                    }
                }
                _ => {}
            }
        }

        #[cfg(windows)]
        if let Ok(data) = ipc_receiver.try_recv() {
            match data {
                Data::ControlledSessionCount(count) => {
                    _tray_icon
                        .lock()
                        .unwrap()
                        .as_mut()
                        .map(|t| t.set_tooltip(Some(tooltip(count))));
                }
                _ => {}
            }
        }

        // Follow the taskbar colour while this process runs. Every other loop
        // iteration only checks the elapsed time, and the icon is swapped only
        // when the colour actually changed.
        #[cfg(windows)]
        if last_theme_check.elapsed() >= TRAY_THEME_POLL_INTERVAL {
            last_theme_check = std::time::Instant::now();
            let taskbar_light = taskbar_is_light();
            if taskbar_light != last_taskbar_light {
                last_taskbar_light = taskbar_light;
                if let Some(icon) = taskbar_light.and_then(themed_tray_icon) {
                    log::info!("Taskbar colour changed, updating the tray icon");
                    match _tray_icon.lock() {
                        Ok(mut tray) => {
                            if let Some(tray) = tray.as_mut() {
                                if let Err(err) = tray.set_icon(Some(icon)) {
                                    log::error!("Failed to update the tray icon: {}", err);
                                }
                            }
                        }
                        Err(err) => log::error!("Failed to lock the tray icon: {}", err),
                    }
                }
            }
        }
    });
}

#[cfg(windows)]
#[tokio::main(flavor = "current_thread")]
async fn start_query_session_count(sender: std::sync::mpsc::Sender<Data>) {
    let mut last_count = 0;
    loop {
        if let Ok(mut c) = crate::ipc::connect(1000, "").await {
            let mut timer = crate::rustdesk_interval(tokio::time::interval(Duration::from_secs(1)));
            loop {
                tokio::select! {
                    res = c.next() => {
                        match res {
                            Err(err) => {
                                log::error!("ipc connection closed: {}", err);
                                break;
                            }

                            Ok(Some(Data::ControlledSessionCount(count))) => {
                                if count != last_count {
                                    last_count = count;
                                    sender.send(Data::ControlledSessionCount(count)).ok();
                                }
                            }
                            _ => {}
                        }
                    }

                    _ = timer.tick() => {
                        c.send(&Data::ControlledSessionCount(0)).await.ok();
                    }
                }
            }
        }
        hbb_common::sleep(1.).await;
    }
}

fn load_icon_from_asset() -> Option<image::DynamicImage> {
    let Some(path) = std::env::current_exe().map_or(None, |x| x.parent().map(|x| x.to_path_buf()))
    else {
        return None;
    };
    #[cfg(target_os = "macos")]
    let path = path.join("../Frameworks/App.framework/Resources/flutter_assets/assets/icon.png");
    #[cfg(windows)]
    let path = path.join(r"data\flutter_assets\assets\icon.png");
    #[cfg(target_os = "linux")]
    let path = path.join(r"data/flutter_assets/assets/icon.png");
    if path.exists() {
        if let Ok(image) = image::open(path) {
            return Some(image);
        }
    }
    None
}

/// The dark/light tray silhouettes in `repos/rustdesk/brand/tray-dark-mode/`,
/// embedded so that a build without a Flutter `data/` folder next to the
/// executable still has a themed icon to show (`include_bytes!` costs a few KB
/// of read-only data). Generated by `analysis/brand/gen-tray-dark-mode.py`.
#[cfg(not(target_os = "macos"))]
const TRAY_ICON_WHITE: &[u8] = include_bytes!("../brand/tray-dark-mode/ico/tray-white.ico");
#[cfg(not(target_os = "macos"))]
const TRAY_ICON_DARK_INK: &[u8] = include_bytes!("../brand/tray-dark-mode/ico/tray-dark-ink.ico");

/// Reads the colour of the taskbar the tray icon is drawn on: `Some(true)` for a
/// light taskbar, `Some(false)` for a dark one.
///
/// This reads `SystemUsesLightTheme` and deliberately not `AppsUseLightTheme`:
/// the latter is the application theme, which is also what Flutter's
/// `platformBrightness` and tao's uxtheme probe report, and the two can differ —
/// following the app theme is what would pair a white silhouette with a light
/// taskbar.
///
/// `None` means the colour is undetermined: a platform without this setting, an
/// unreadable or missing value, or a value that is neither 0 nor 1. Callers keep
/// the icon they already show rather than guess a colour, and nothing here panics.
#[cfg(not(target_os = "macos"))]
fn taskbar_is_light() -> Option<bool> {
    #[cfg(windows)]
    {
        let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
        let key = match key {
            Ok(key) => key,
            Err(err) => {
                hbb_common::throttled_log!(
                    TRAY_THEME_LOG_INTERVAL,
                    warn,
                    "Failed to read the Windows theme settings: {}",
                    err
                );
                return None;
            }
        };
        match key.get_value::<u32, _>("SystemUsesLightTheme") {
            Ok(1) => Some(true),
            Ok(0) => Some(false),
            Ok(value) => {
                hbb_common::throttled_log!(
                    TRAY_THEME_LOG_INTERVAL,
                    warn,
                    "Unexpected SystemUsesLightTheme value: {}",
                    value
                );
                None
            }
            // Absent on installs that never changed the setting; this runs on
            // every poll, so it stays at trace level (AGENTS.md, "Logging").
            Err(err) => {
                log::trace!("SystemUsesLightTheme is not set: {}", err);
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        // macOS draws the tray icon from its alpha channel as a template image
        // (`with_icon_as_template`), so its colour is not ours to choose, and
        // neither it nor Linux exposes a taskbar colour this process could read.
        None
    }
}

/// Builds the tray icon for a known taskbar colour, or `None` when neither the
/// Flutter asset nor the embedded ICO can be turned into an icon (the caller
/// then keeps the icon it already has).
#[cfg(not(target_os = "macos"))]
fn themed_tray_icon(taskbar_light: bool) -> Option<tray_icon::Icon> {
    let embedded = if taskbar_light {
        TRAY_ICON_DARK_INK
    } else {
        TRAY_ICON_WHITE
    };
    let image = match themed_icon_from_asset(taskbar_light) {
        Some(image) => image,
        None => match image::load_from_memory(embedded) {
            Ok(image) => image,
            Err(err) => {
                log::error!("Failed to load the themed tray icon: {}", err);
                return None;
            }
        },
    }
    .into_rgba8();
    let (width, height) = image.dimensions();
    match tray_icon::Icon::from_rgba(image.into_raw(), width, height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            log::error!("Failed to build the themed tray icon: {}", err);
            None
        }
    }
}

/// The themed counterpart of `load_icon_from_asset`: the same Flutter asset
/// folder next to the executable, with the file name for the given taskbar
/// colour. Returning `None` lets the caller fall back to the embedded ICO.
#[cfg(not(target_os = "macos"))]
fn themed_icon_from_asset(taskbar_light: bool) -> Option<image::DynamicImage> {
    let name = if taskbar_light {
        "tray-256-dark-ink.png"
    } else {
        "tray-256-white.png"
    };
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    #[cfg(windows)]
    let path = dir.join(r"data\flutter_assets\assets").join(name);
    #[cfg(target_os = "linux")]
    let path = dir.join(r"data/flutter_assets/assets").join(name);
    if path.exists() {
        if let Ok(image) = image::open(path) {
            return Some(image);
        }
    }
    None
}
