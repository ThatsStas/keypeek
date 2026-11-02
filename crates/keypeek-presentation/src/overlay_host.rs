//! Interface to request window changes from the host platform.

/// Interface provided by the window host to control passthrough and window closing.
pub trait OverlayHost {
    fn set_passthrough(&mut self, enabled: bool);
    fn request_close(&mut self);

    /// Show/hide (withdraw) the OS window backing the overlay.
    ///
    /// When the overlay has nothing to display, the window is unmapped on X11
    /// so the window manager can neither focus it nor capture pointer events
    /// for the transparent-but-empty window. Hosts without support keep it
    /// mapped.
    fn set_mapped(&mut self, _mapped: bool) {}
}
