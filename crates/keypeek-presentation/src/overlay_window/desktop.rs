use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::app_core::KeyPeekAppCore;
use crate::overlay_host::OverlayHost;
use crate::settings::SettingsStore;
use keypeek_protocol::connection_manager::{ConnectOutcome, ConnectionEvent};
use keypeek_protocol::{DiscoveredDevice, Keyboard, UiWake};

/// Background clear color for desktop: transparent overlay or dimmed backdrop.
pub fn clear_color(is_any_window_open: bool) -> egui::Rgba {
    if is_any_window_open {
        egui::Rgba::from_black_alpha(0.65)
    } else {
        egui::Rgba::TRANSPARENT
    }
}

/// Top-level coordinator for desktop execution: multi-windowing, system tray,
/// mouse passthrough, and native file dialogs.

/// The settings dialog is removed in this fork; all tuning happens in
/// `~/.config/keypeek/settings.ini`. Kept as a switch for the disabled code path.
const SETTINGS_UI_ENABLED: bool = false;

pub struct DesktopOverlayApp {
    pub(super) settings_requested: Arc<AtomicBool>,
    /// Legacy sink from the removed SIGUSR1 settings toggle: requests are
    /// consumed and ignored (settings live in `~/.config/keypeek/settings.ini`).
    pub(super) settings_toggled: Arc<AtomicBool>,
    pub core: KeyPeekAppCore,
    pub(super) file_dialog: egui_file_dialog::FileDialog,
    pub(super) mouse_passthrough: Option<bool>,
    /// Last mapping state requested for the OS window (X11 map/unmap); see
    /// the idle-withdrawal logic in [`DesktopOverlayApp::update`].
    window_mapped: bool,
    /// False on the very first update pass: eframe creates the window hidden
    /// and maps it in `post_rendering` after that pass' paint. Deciding the
    /// mapping before that map lands would race winit's request, so the
    /// first pass is skipped (the map's Expose event guarantees a second one).
    first_update_done: bool,
}

pub type DesktopApp = DesktopOverlayApp;
pub type OverlayApp = DesktopOverlayApp;

impl std::ops::Deref for DesktopOverlayApp {
    type Target = KeyPeekAppCore;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl std::ops::DerefMut for DesktopOverlayApp {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl DesktopOverlayApp {
    pub fn new(
        settings_requested: Arc<AtomicBool>,
        ui_wake: UiWake,
        settings_store: Arc<dyn SettingsStore>,
        available_devices: Vec<DiscoveredDevice>,
    ) -> Self {
        Self {
            settings_requested,
            settings_toggled: Arc::new(AtomicBool::new(false)),
            core: KeyPeekAppCore::new(ui_wake, settings_store, available_devices, false),
            file_dialog: egui_file_dialog::FileDialog::new(),
            mouse_passthrough: None,
            // The window is created mapped; the first update pass withdraws it
            // if the overlay has nothing to show.
            window_mapped: true,
            first_update_done: false,
        }
    }

    /// Background color before egui paints.
    pub fn clear_color(&self) -> egui::Rgba {
        clear_color(self.is_any_window_open())
    }

    /// Initiates connection for the currently selected device in connection_mgr.
    pub fn connect_from_ui(&mut self) {
        if self.core.connect_selected_device() == ConnectOutcome::RequiresLayoutFile {
            self.pick_layout_file();
        }
    }

    /// Disconnects the active keyboard connection, tears down protocol session,
    /// and closes the keymap editor.
    pub fn disconnect_from_ui(&mut self) {
        self.core.disconnect_from_ui();
    }

    pub(super) fn get_anchor_params(&self) -> (egui::Align2, egui::Vec2) {
        self.settings
            .active
            .position
            .anchor_and_offset(self.settings.active.margin as f32)
    }

    pub(super) fn overlay_visible(&self) -> bool {
        self.core.is_overlay_visible()
    }

    /// Opens the native file dialog to pick a layout file.
    pub(super) fn pick_layout_file(&mut self) {
        self.file_dialog.pick_file();
    }

    /// Sets OS mouse passthrough mode based on current window state.
    pub(super) fn sync_mouse_passthrough(&mut self, host: &mut dyn OverlayHost) {
        let mouse_passthrough = !self.is_any_window_open();
        if self.mouse_passthrough == Some(mouse_passthrough) {
            return;
        }

        host.set_passthrough(mouse_passthrough);
        self.mouse_passthrough = Some(mouse_passthrough);
    }

    /// Updates state, handles background tasks, and processes input before drawing.
    pub fn update(&mut self, ctx: &egui::Context, host: &mut dyn OverlayHost) {
        // Settings UI removed in this fork: all open/flip requests (tray menu,
        // legacy flags) are consumed and ignored. Tune via
        // ~/.config/keypeek/settings.ini instead.
        let _ = self.settings_requested.swap(false, Ordering::Relaxed);
        let _ = self.settings_toggled.swap(false, Ordering::Relaxed);

        if let Some(ConnectionEvent::RequiresLayoutFile(_)) = self.core.update_connection(ctx) {
            self.pick_layout_file();
        }

        self.file_dialog.update(ctx);

        if let Some(path) = self.file_dialog.take_picked() {
            self.core
                .connection_mgr
                .set_layout_file_path(path.to_string_lossy().to_string());
            self.connect_from_ui();
        } else if self.file_dialog.state() == &egui_file_dialog::DialogState::Cancelled
            && self.core.connection_mgr.state().is_requires_layout_file()
        {
            self.core.connection_mgr.disconnect();
        }

        self.sync_mouse_passthrough(host);

        // Idle => withdrawn. While there is nothing to show, unmap the OS
        // window entirely so the transparent-but-empty overlay cannot be
        // focused or capture clicks. The window is override-redirect (see the
        // host), so i3 never sees these requests: no re-manage, no focus, no
        // geometry drift. The session's ui_wake wakes the event loop on the
        // next keypress and this check remaps the window at its saved rect.
        //
        // The very first pass is skipped: eframe creates the window hidden
        // and maps it in `post_rendering` after that pass' paint; requesting
        // an unmap before that map lands would race winit's request over a
        // different X connection. The startup repaint ticks guarantee a
        // second pass runs.
        if self.first_update_done {
            let want_mapped = self.overlay_visible()
                || self.core.is_any_window_open()
                || self.core.ui.settings_error.is_some()
                || self.core.ui.settings_warning.is_some()
                || matches!(
                    self.file_dialog.state(),
                    egui_file_dialog::DialogState::Open
                );
            if want_mapped != self.window_mapped {
                host.set_mapped(want_mapped);
                self.window_mapped = want_mapped;
                if want_mapped {
                    // The newly mapped window may show a stale buffer until
                    // the next natural repaint; don't wait for one.
                    ctx.request_repaint();
                }
            }
        }
        self.first_update_done = true;
    }

    /// Shows a modal dialog with `message` and an OK button that clears `slot`.
    fn message_window(ctx: &egui::Context, title: &str, slot: &mut Option<String>) {
        let Some(message) = slot.clone() else {
            return;
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(message);
                ui.add_space(10.0);
                if ui.button("OK").clicked() {
                    *slot = None;
                }
            });
    }

    /// Renders the overlay in an egui Window.
    pub fn draw_overlay_window(&mut self, ctx: &egui::Context, keyboard: &Keyboard, visible: bool) {
        let anchor_params = self.get_anchor_params();
        let mut window_open = visible;
        let hit_test_enabled = self.is_any_window_open();
        let size = self.core.settings.active.size as f32;

        egui::Window::new("KeyPeek")
            .open(&mut window_open)
            .auto_sized()
            .interactable(hit_test_enabled)
            .anchor(anchor_params.0, anchor_params.1)
            .frame(egui::Frame::NONE.fill(egui::Color32::TRANSPARENT))
            .fade_out(true)
            .title_bar(false)
            .show(ctx, |ui| {
                self.core
                    .overlay_view(keyboard, size, hit_test_enabled)
                    .show(ui);
            });
    }

    /// Renders desktop windows: overlay, key editor, settings, and notices.
    pub fn render(&mut self, ctx: &egui::Context, host: &mut dyn OverlayHost) {
        let connected = self.core.connection_mgr.connected_pair();
        if let Some((keyboard, profile)) = &connected {
            self.draw_overlay_window(ctx, keyboard, self.overlay_visible());
            self.core
                .draw_editor_window(ctx, keyboard, profile.as_ref());
        }

        // Settings UI removed in this fork; kept compiled but unreachable.
        if self.ui.settings_visible && SETTINGS_UI_ENABLED {
            self.draw_settings_window(ctx, host);
        }

        Self::message_window(ctx, "Error", &mut self.core.ui.settings_error);
        Self::message_window(ctx, "Notice", &mut self.core.ui.settings_warning);
    }

    /// Primary entry point called by the desktop platform runner.
    pub fn ui(&mut self, ctx: &egui::Context, host: &mut dyn OverlayHost) {
        self.update(ctx, host);
        self.render(ctx, host);
        self.schedule_overlay_repaint(ctx);
    }
}
