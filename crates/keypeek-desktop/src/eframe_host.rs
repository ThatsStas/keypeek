use keypeek_presentation::{add_phosphor_to_fonts, DesktopOverlayApp, OverlayHost, SettingsStore};
use keypeek_protocol::{DiscoveredDevice, UiWake};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(target_os = "linux")]
struct X11Mapper {
    conn: x11rb::xcb_ffi::XCBConnection,
    xid: u32,
}

/// Where the overlay lives in PHYSICAL pixels on the DP-2 4K panel (the rect
/// the user dialed in with i3). winit scales the builder's logical hints by
/// the runtime scale factor (5/3 here), so the override-redirect window is
/// placed with these exact values via `configure_window` instead.
#[cfg(target_os = "linux")]
const OVERLAY_GEOMETRY: (i32, i32, u32, u32) = (3759, 1095, 1553, 964);

/// Full physical rect + always-on-top, re-asserted whenever the overlay is
/// mapped (see [`OVERLAY_GEOMETRY`]).
#[cfg(target_os = "linux")]
fn overlay_geometry_aux() -> x11rb::protocol::xproto::ConfigureWindowAux {
    let (x, y, width, height) = OVERLAY_GEOMETRY;
    x11rb::protocol::xproto::ConfigureWindowAux {
        x: Some(x),
        y: Some(y),
        width: Some(width),
        height: Some(height),
        stack_mode: Some(x11rb::protocol::xproto::StackMode::ABOVE),
        ..Default::default()
    }
}

struct EframeHost<'a> {
    ctx: &'a egui::Context,
    #[cfg(target_os = "linux")]
    mapper: Option<&'a mut X11Mapper>,
}

impl OverlayHost for EframeHost<'_> {
    fn set_passthrough(&mut self, enabled: bool) {
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::MousePassthrough(enabled));
    }

    fn request_close(&mut self) {
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Withdraw/restore the X window without involving winit/egui: egui's own
    /// `ViewportCommand::Visible(false)` flips the root viewport's visibility
    /// and stops running the UI pass, which would strand the overlay (nothing
    /// would notice the next keypress to bring it back). Unmapping via raw
    /// X11 leaves egui believing the window is visible, so the event loop
    /// keeps ticking (invisible windows repaint at >=100 ms intervals).
    ///
    /// The window is override-redirect, so no WM sees these requests: there
    /// is no re-manage, no focus change, and no saved-geometry drift; the
    /// window simply reappears exactly where the app placed it. Raise as
    /// part of mapping so the lit keys land above the focused window (XMap
    /// alone keeps the previous stack position).
    #[cfg(target_os = "linux")]
    fn set_mapped(&mut self, mapped: bool) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        let Some(mapper) = self.mapper.as_mut() else {
            return;
        };
        let cookie = if mapped {
            let _ = mapper
                .conn
                .configure_window(mapper.xid, &overlay_geometry_aux());
            mapper.conn.map_window(mapper.xid)
        } else {
            mapper.conn.unmap_window(mapper.xid)
        };
        if cookie.is_ok() {
            let _ = mapper.conn.flush();
        }
    }
}

struct EframeApp {
    app: DesktopOverlayApp,
    _tray: super::tray::Tray,
    // Undecorated transparent windows don't reliably honor `with_maximized`, so we
    // size to the monitor explicitly once known. Linux never WM-maximizes at all,
    // since Mutter drops always-on-top on a maximized window.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    sized_to_monitor: bool,
    // winit's always-on-top request is sent before the window is mapped, which
    // EWMH WMs like Mutter ignore, so re-assert it for a few frames after mapping.
    #[cfg(target_os = "linux")]
    x11_above_ticks: u32,
    /// Raw X11 handle for idle-window withdrawal (unmap/map); None on Wayland,
    /// where the window simply stays mapped.
    #[cfg(target_os = "linux")]
    mapper: Option<X11Mapper>,
}

impl eframe::App for EframeApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.app.clear_color().to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Re-assert always-on-top now that the window is mapped (see field docs).
        #[cfg(target_os = "linux")]
        if self.x11_above_ticks > 0 {
            self.x11_above_ticks -= 1;
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::WindowLevel::AlwaysOnTop,
            ));
            ctx.request_repaint();
        }

        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if !self.sized_to_monitor {
            if let Some(monitor_size) = ctx.input(|i| i.viewport().monitor_size) {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(0.0, 0.0)));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(monitor_size));
                self.sized_to_monitor = true;
            }
        }

        let mut host = EframeHost {
            ctx: &ctx,
            #[cfg(target_os = "linux")]
            mapper: self.mapper.as_mut(),
        };
        self.app.ui(&ctx, &mut host);
    }
}

// Keep the overlay visible when switching Spaces or using fullscreen apps.
#[cfg(target_os = "macos")]
fn show_on_all_spaces(cc: &eframe::CreationContext<'_>) {
    use objc2_app_kit::{NSView, NSWindowCollectionBehavior};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = cc.window_handle() else {
        return;
    };
    if let RawWindowHandle::AppKit(handle) = handle.as_raw() {
        let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        if let Some(window) = view.window() {
            window.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
        }
    }
}

// DWM only composites a window's per-pixel alpha when asked to via
// `DwmEnableBlurBehindWindow`; otherwise the overlay can render opaque (black)
// on some systems. See https://github.com/srwi/keypeek/issues/16
#[cfg(target_os = "windows")]
fn enable_dwm_per_pixel_alpha(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::core::BOOL;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmEnableBlurBehindWindow, DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND,
    };
    use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HGDIOBJ};

    let Ok(RawWindowHandle::Win32(handle)) = cc.window_handle().map(|h| h.as_raw()) else {
        return;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);

    // A region covering the whole window makes DWM honor the window's alpha.
    let region = unsafe { CreateRectRgn(0, 0, -1, -1) };
    let blur_behind = DWM_BLURBEHIND {
        dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
        fEnable: BOOL(1),
        hRgnBlur: region,
        ..Default::default()
    };
    unsafe {
        let _ = DwmEnableBlurBehindWindow(hwnd, &blur_behind);
        let _ = DeleteObject(HGDIOBJ(region.0));
    }
}

// `force_x11` (Linux only) makes winit use XWayland instead of native Wayland,
// since Mutter honors always-on-top for XWayland clients but not native ones.
pub fn run(
    settings_store: Arc<dyn SettingsStore>,
    devices: Vec<DiscoveredDevice>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] force_x11: bool,
) -> Result<(), eframe::Error> {
    #[cfg(target_os = "linux")]
    if force_x11 {
        match run_inner(settings_store.clone(), devices.clone(), true) {
            Ok(()) => return Ok(()),
            Err(e) => {
                eprintln!(
                    "KeyPeek: XWayland/X11 backend unavailable ({e}); \
                     retrying on Wayland (overlay will not stay always-on-top)."
                );
            }
        }
    }
    run_inner(settings_store, devices, false)
}

fn run_inner(
    settings_store: Arc<dyn SettingsStore>,
    devices: Vec<DiscoveredDevice>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] force_x11: bool,
) -> Result<(), eframe::Error> {
    #[allow(unused_mut)]
    let mut viewport = egui::ViewportBuilder::default()
        .with_decorations(false)
        .with_taskbar(false)
        .with_transparent(true)
        .with_has_shadow(false)
        .with_always_on_top();

    #[cfg(not(target_os = "linux"))]
    {
        viewport = viewport.with_maximized(true);
    }

    #[cfg(target_os = "linux")]
    {
        // The window bypasses the WM entirely (override-redirect): never
        // managed by i3, hence never focusable, never restacked, and free of
        // i3's unmap/remap geometry drift. The app owns placement, which the
        // builder hints below provide; the overlay is inherently "sticky"
        // (visible on all workspaces) and the launch script does no placement.
        viewport = viewport
            .with_window_type(egui::X11WindowType::Utility)
            .with_override_redirect(true)
            .with_inner_size(egui::vec2(1553.0, 964.0))
            .with_position(egui::pos2(3759.0, 1095.0));
    }

    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow, // Glow is required for a transparent background (https://github.com/emilk/egui/issues/4451)
        viewport,
        ..Default::default()
    };

    // Hide from the macOS dock so the app only appears as a tray icon.
    #[cfg(target_os = "macos")]
    {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
            builder.with_activation_policy(ActivationPolicy::Accessory);
        }));
    }

    // Force XWayland so always-on-top is honored on GNOME (see `run`).
    #[cfg(target_os = "linux")]
    if force_x11 {
        options.event_loop_builder = Some(Box::new(|builder| {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }));
    }

    eframe::run_native(
        "KeyPeek",
        options,
        Box::new(move |cc| {
            #[cfg(target_os = "macos")]
            show_on_all_spaces(cc);
            #[cfg(target_os = "windows")]
            enable_dwm_per_pixel_alpha(cc);

            egui_extras::install_image_loaders(&cc.egui_ctx);

            let ctx = cc.egui_ctx.clone();
            let ui_wake = UiWake::new(Arc::new(move || ctx.request_repaint()));
            let settings_requested = Arc::new(AtomicBool::new(false));
            let tray_icon = super::tray::create_tray_icon({
                let settings_requested = settings_requested.clone();
                let ui_wake = ui_wake.clone();
                Arc::new(move || {
                    settings_requested.store(true, Ordering::Relaxed);
                    ui_wake.request_repaint();
                })
            });

            let mut fonts = egui::FontDefinitions::default();
            add_phosphor_to_fonts(&mut fonts);
            cc.egui_ctx.set_fonts(fonts);

            // Grab the XID while the WM hasn't reparented anything confusing
            // yet, open a dedicated connection for map/unmap requests, and
            // cut the window out of the input geometry: with an empty input
            // shape every click/hover falls through to the windows behind it
            // (the overlay has no interactive UI, so it never needs pointer
            // events). The Shape extension must be queried once first.
            #[cfg(target_os = "linux")]
            let mapper = (|| -> Option<X11Mapper> {
                use winit::raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
                use x11rb::connection::Connection as _;
                use x11rb::protocol::shape::ConnectionExt as _;
                use x11rb::protocol::xproto::ConnectionExt as _;
                use x11rb::protocol::{shape, xproto};
                let RawWindowHandle::Xlib(handle) = cc.window_handle().ok()?.as_raw() else {
                    return None;
                };
                let (conn, _screen) = x11rb::xcb_ffi::XCBConnection::connect(None).ok()?;
                let xid = handle.window as u32;
                if let Ok(cookie) = conn.shape_query_version() {
                    let _ = cookie.reply();
                    let _ = conn.shape_rectangles(
                        shape::SO::SET,
                        shape::SK::INPUT,
                        xproto::ClipOrdering::UNSORTED,
                        xid,
                        0,
                        0,
                        &[],
                    );
                }
                // Claim the physical geometry now, before eframe's first map:
                // winit placed the window using DPI-scaled logical coordinates
                // and an override-redirect window has no WM to correct it.
                let _ = conn.configure_window(xid, &overlay_geometry_aux());
                let _ = conn.flush();
                Some(X11Mapper { conn, xid })
            })();

            let app = DesktopOverlayApp::new(settings_requested, ui_wake, settings_store, devices);
            Ok(Box::new(EframeApp {
                app,
                _tray: tray_icon,
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                // On Linux the window is override-redirect and places itself
                // from the builder hints; winit's `monitor_size` query is
                // unreliable on X11, so never run that path there.
                sized_to_monitor: cfg!(target_os = "linux"),
                #[cfg(target_os = "linux")]
                x11_above_ticks: 10,
                #[cfg(target_os = "linux")]
                mapper,
            }))
        }),
    )
}
