use std::ptr;

use objc2_core_foundation::{CFType, CGPoint, CGRect};
use objc2_core_graphics::CGContext;

use crate::common::collections::HashMap;
use crate::sys::cgs_window::{CgsWindow, CgsWindowError};
use crate::sys::skylight::{CFRelease, SLSFlushWindowContentRegion, SLWindowContextCreate, G_CONNECTION};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGContextFlush(ctx: *mut CGContext);
    fn CGGradientCreateWithColorComponents(
        space: *const std::ffi::c_void,
        components: *const f64,
        locations: *const f64,
        count: usize,
    ) -> *const std::ffi::c_void;
    fn CGColorSpaceCreateDeviceRGB() -> *const std::ffi::c_void;
    fn CGColorSpaceRelease(space: *const std::ffi::c_void);
    fn CGGradientRelease(gradient: *const std::ffi::c_void);
    fn CGContextDrawLinearGradient(
        ctx: *mut CGContext,
        gradient: *const std::ffi::c_void,
        start_point: CGPoint,
        end_point: CGPoint,
        options: u32,
    );
    fn CGContextDrawRadialGradient(
        ctx: *mut CGContext,
        gradient: *const std::ffi::c_void,
        start_center: CGPoint,
        start_radius: f64,
        end_center: CGPoint,
        end_radius: f64,
        options: u32,
    );
}

/// Manages fullscreen overlays to hide inactive workspaces
pub struct WorkspaceOverlayManager {
    /// Map of display_id -> overlay window
    overlays: HashMap<usize, CgsWindow>,
    /// Track which displays currently have overlays shown
    overlay_shown: HashMap<usize, bool>,
    /// Track which windows were last raised per display to prevent redundant raises
    last_raised_windows: HashMap<usize, Vec<crate::actor::app::WindowId>>,
}

impl WorkspaceOverlayManager {
    pub fn new() -> Self {
        Self {
            overlays: HashMap::default(),
            overlay_shown: HashMap::default(),
            last_raised_windows: HashMap::default(),
        }
    }

    /// Check if the given windows are different from what was last raised for this display
    /// Returns true if raising is needed, and updates the tracking if so
    pub fn should_raise_windows(&mut self, display_id: usize, mut windows: Vec<crate::actor::app::WindowId>) -> bool {
        // Sort for consistent comparison
        windows.sort_by_key(|w| (w.pid, w.idx));

        let last = self.last_raised_windows.get(&display_id);
        let needs_raise = match last {
            None => !windows.is_empty(),
            Some(last_windows) => last_windows != &windows,
        };

        if needs_raise {
            self.last_raised_windows.insert(display_id, windows);
        }

        needs_raise
    }

    /// Order the overlay behind a specific window
    pub fn order_overlay_behind(&self, display_id: usize, window_id: u32) -> Result<(), CgsWindowError> {
        if let Some(overlay) = self.overlays.get(&display_id) {
            overlay.order_below(Some(window_id))?;
        }
        Ok(())
    }

    /// Order the overlay above a specific window (to hide it)
    pub fn order_overlay_above(&self, display_id: usize, window_id: u32) -> Result<(), CgsWindowError> {
        if let Some(overlay) = self.overlays.get(&display_id) {
            overlay.order_above(Some(window_id))?;
        }
        Ok(())
    }

    /// Show an overlay on the specified display to hide windows
    /// Returns true if the overlay state changed (was hidden, now shown)
    pub fn show_overlay(&mut self, display_id: usize, screen: CGRect) -> Result<bool, CgsWindowError> {
        // Check if already shown
        if self.overlay_shown.get(&display_id).copied().unwrap_or(false) {
            return Ok(false); // Already shown, no change
        }

        println!("[WORKSPACE_OVERLAY] Showing overlay for display {}: {:?}", display_id, screen);

        // Create or update the overlay
        if let Some(overlay) = self.overlays.get(&display_id) {
            // Overlay exists but was hidden - bring it back
            overlay.set_shape(screen)?;
            // Don't order it yet - let the z-ordering code handle positioning
            Self::draw_background(overlay, screen)?;
        } else {
            // Create new overlay at normal window level
            // Z-ordering will position it above offscreen windows and below active windows
            let overlay = CgsWindow::new(screen)?;
            overlay.set_opacity(true)?; // Opaque background
            overlay.set_alpha(1.0)?;
            // Use normal window level (0) so z-ordering can position overlay correctly
            // Level 3 (floating) would always be above normal windows, breaking z-order
            overlay.set_level(0)?; // NSNormalWindowLevel
            Self::draw_background(&overlay, screen)?;
            self.overlays.insert(display_id, overlay);
        }

        self.overlay_shown.insert(display_id, true);
        Ok(true) // State changed
    }

    /// Draw a modern gradient background on the overlay
    fn draw_background(overlay: &CgsWindow, screen: CGRect) -> Result<(), CgsWindowError> {
        unsafe {
            // Create a window context
            let ctx = SLWindowContextCreate(
                *G_CONNECTION,
                overlay.id(),
                ptr::null_mut() as *mut CFType,
            );

            if ctx.is_null() {
                println!("[WORKSPACE_OVERLAY] Failed to create window context");
                return Ok(());
            }

            // Create RGB color space
            let color_space = CGColorSpaceCreateDeviceRGB();

            // Modern multi-stop gradient: Deep space theme with rich blues and purples
            // Each color is RGBA (4 components)
            let gradient_colors: [f64; 16] = [
                0.02, 0.02, 0.08, 1.0,   // Deep midnight blue (#050514)
                0.08, 0.05, 0.20, 1.0,   // Rich purple-blue (#140D33)
                0.05, 0.10, 0.25, 1.0,   // Deep ocean blue (#0D1940)
                0.01, 0.05, 0.12, 1.0,   // Nearly black blue (#01070F)
            ];

            // Gradient positions for smooth transitions
            let locations: [f64; 4] = [0.0, 0.35, 0.70, 1.0];

            // Create base gradient
            let gradient = CGGradientCreateWithColorComponents(
                color_space,
                gradient_colors.as_ptr(),
                locations.as_ptr(),
                4,
            );

            // Draw diagonal gradient from top-left to bottom-right
            let start_point = CGPoint::new(0.0, 0.0);
            let end_point = CGPoint::new(screen.size.width, screen.size.height);

            CGContextDrawLinearGradient(
                ctx,
                gradient,
                start_point,
                end_point,
                0,
            );

            // Add a subtle radial overlay for depth
            let radial_colors: [f64; 8] = [
                0.10, 0.08, 0.25, 0.4,   // Subtle bright center (semi-transparent)
                0.0, 0.0, 0.0, 0.0,      // Transparent edges
            ];

            let radial_locations: [f64; 2] = [0.0, 1.0];

            let radial_gradient = CGGradientCreateWithColorComponents(
                color_space,
                radial_colors.as_ptr(),
                radial_locations.as_ptr(),
                2,
            );

            // Center point for radial gradient
            let center_x = screen.size.width / 2.0;
            let center_y = screen.size.height / 2.0;
            let center = CGPoint::new(center_x, center_y);

            // Radius covers the entire screen
            let radius = ((screen.size.width * screen.size.width + screen.size.height * screen.size.height).sqrt()) / 1.5;

            CGContextDrawRadialGradient(
                ctx,
                radial_gradient,
                center,
                0.0,
                center,
                radius,
                0,
            );

            CGContextFlush(ctx);

            // Flush to display
            SLSFlushWindowContentRegion(*G_CONNECTION, overlay.id(), ptr::null_mut());

            // Clean up
            CGGradientRelease(radial_gradient);
            CGGradientRelease(gradient);
            CGColorSpaceRelease(color_space);
            CFRelease(ctx as *mut CFType);
        }

        Ok(())
    }

    /// Hide the overlay on the specified display to show windows
    /// Returns true if the overlay state changed (was shown, now hidden)
    pub fn hide_overlay(&mut self, display_id: usize) -> Result<bool, CgsWindowError> {
        // Check if already hidden
        if !self.overlay_shown.get(&display_id).copied().unwrap_or(false) {
            return Ok(false); // Already hidden, no change
        }

        println!("[WORKSPACE_OVERLAY] Hiding overlay for display {}", display_id);

        if let Some(overlay) = self.overlays.get(&display_id) {
            overlay.order_out()?;
        }

        self.overlay_shown.insert(display_id, false);
        Ok(true) // State changed
    }

    /// Remove all overlays
    pub fn clear_all(&mut self) {
        self.overlays.clear();
    }
}
