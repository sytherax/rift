use std::time::Instant;

use objc2_core_foundation::{CGRect, CGSize};
use tracing::trace;

use super::main_window::MainWindowTracker;
use super::replay::Record;
use super::{
    AppState, AutoWorkspaceSwitch, Event, FullscreenTrack, PendingSpaceChange, Screen, WindowState,
};
use crate::actor;
use crate::actor::app::{WindowId, pid_t};
use crate::actor::broadcast::BroadcastSender;
use crate::actor::drag_swap::DragManager as DragSwapManager;
use crate::actor::reactor::Reactor;
use crate::actor::reactor::animation::AnimationManager;
use crate::actor::{event_tap, menu_bar, raise_manager, stack_line, window_notify, wm_controller};
use crate::common::collections::{HashMap, HashSet};
use crate::common::config::{Config, WindowSnappingSettings};
use crate::layout_engine::LayoutEngine;
use crate::sys::screen::SpaceId;
use crate::sys::window_server::{WindowServerId, WindowServerInfo};

/// Manages window state and lifecycle
pub struct WindowManager {
    pub windows: HashMap<WindowId, WindowState>,
    pub window_ids: HashMap<WindowServerId, WindowId>,
    pub visible_windows: HashSet<WindowServerId>,
    pub observed_window_server_ids: HashSet<WindowServerId>,
}

/// Manages application state and rules
pub struct AppManager {
    pub apps: HashMap<pid_t, AppState>,
    pub app_rules_recently_applied: Instant,
}

/// Manages space and screen state
pub struct SpaceManager {
    pub screens: Vec<Screen>,
    pub fullscreen_by_space: HashMap<u64, FullscreenTrack>,
    pub changing_screens: HashSet<WindowServerId>,
}

/// Manages drag operations and window swapping
pub struct DragManager {
    pub drag_state: super::DragState,
    pub drag_swap_manager: DragSwapManager,
    pub skip_layout_for_window: Option<WindowId>,
}

impl DragManager {
    pub fn reset(&mut self) { self.drag_swap_manager.reset(); }

    pub fn last_target(&self) -> Option<WindowId> { self.drag_swap_manager.last_target() }

    pub fn dragged(&self) -> Option<WindowId> { self.drag_swap_manager.dragged() }

    pub fn origin_frame(&self) -> Option<CGRect> { self.drag_swap_manager.origin_frame() }

    pub fn update_config(&mut self, config: WindowSnappingSettings) {
        self.drag_swap_manager.update_config(config);
    }
}

/// Manages window notifications
pub struct NotificationManager {
    pub last_sls_notification_ids: Vec<u32>,
    pub window_notify_tx: Option<window_notify::Sender>,
}

/// Manages menu state and interactions
pub struct MenuManager {
    pub menu_state: super::MenuState,
    pub menu_tx: Option<menu_bar::Sender>,
}

/// Manages Mission Control state
pub struct MissionControlManager {
    pub mission_control_state: super::MissionControlState,
    pub pending_mission_control_refresh: HashSet<pid_t>,
}

/// Manages workspace switching state
pub struct WorkspaceSwitchManager {
    pub workspace_switch_state: super::WorkspaceSwitchState,
    pub workspace_switch_generation: u64,
    pub active_workspace_switch: Option<u64>,
    pub last_auto_workspace_switch: Option<AutoWorkspaceSwitch>,
    pub pending_workspace_mouse_warp: Option<WindowId>,
    /// Track which apps are currently visible per display (for app hiding)
    pub visible_apps_per_display: crate::common::collections::HashMap<usize, crate::common::collections::HashSet<crate::sys::app::pid_t>>,
}

/// Manages refocus and cleanup state
pub struct RefocusManager {
    pub stale_cleanup_state: super::StaleCleanupState,
    pub refocus_state: super::RefocusState,
}

/// Manages communication channels to other actors
pub struct CommunicationManager {
    pub event_tap_tx: Option<event_tap::Sender>,
    pub stack_line_tx: Option<stack_line::Sender>,
    pub raise_manager_tx: raise_manager::Sender,
    pub event_broadcaster: BroadcastSender,
    pub wm_sender: Option<wm_controller::Sender>,
    pub events_tx: Option<actor::Sender<Event>>,
}

/// Manages recording state
pub struct RecordingManager {
    pub record: Record,
}

/// Manages configuration state
pub struct ConfigManager {
    pub config: Config,
}

/// Manages layout engine state
pub struct LayoutManager {
    pub layout_engine: LayoutEngine,
}

pub type LayoutResult = Vec<(SpaceId, Vec<(WindowId, CGRect)>)>;

impl LayoutManager {
    pub fn update_layout(
        reactor: &mut Reactor,
        is_resize: bool,
        is_workspace_switch: bool,
    ) -> Result<bool, super::error::ReactorError> {
        println!("[UPDATE_LAYOUT] Called with is_resize={}, is_workspace_switch={}", is_resize, is_workspace_switch);
        let layout_result = Self::calculate_layout(reactor);
        Self::apply_layout(reactor, layout_result, is_resize, is_workspace_switch)
    }

    fn calculate_layout(reactor: &mut Reactor) -> LayoutResult {
        // Clear cycle tracking at the start of each layout calculation
        reactor.layout_manager.layout_engine.positioned_windows_this_cycle.clear();

        let screens = reactor.space_manager.screens.clone();
        // let mut layout_result = Vec::new();
        let mut layout_result = LayoutResult::new();

        for screen in screens {
            let Some(space) = screen.space else { continue };
            let layout =
                reactor.layout_manager.layout_engine.calculate_layout_with_virtual_workspaces(
                    space,
                    screen.frame.clone(),
                    reactor.config_manager.config.settings.ui.stack_line.thickness(),
                    reactor.config_manager.config.settings.ui.stack_line.horiz_placement,
                    reactor.config_manager.config.settings.ui.stack_line.vert_placement,
                    |wid| {
                        reactor
                            .window_manager
                            .windows
                            .get(&wid)
                            .map(|w| w.frame_monotonic.size)
                            .unwrap_or_else(|| CGSize::new(500.0, 500.0))
                    },
                );
            layout_result.push((space, layout));
        }

        layout_result
    }

    fn apply_layout(
        reactor: &mut Reactor,
        layout_result: LayoutResult,
        is_resize: bool,
        is_workspace_switch: bool,
    ) -> Result<bool, super::error::ReactorError> {
        let main_window = reactor.main_window();
        trace!(?main_window);
        let skip_wid = reactor
            .drag_manager
            .skip_layout_for_window
            .take()
            .or(reactor.drag_manager.drag_swap_manager.dragged());
        let mut any_frame_changed = false;

        for (space, layout) in &layout_result {
            println!("[APPLY_LAYOUT] space {:?}: {} windows to position", space, layout.len());
            for (wid, rect) in layout {
                println!("[APPLY_LAYOUT] space {:?}: window {:?} -> {:?}", space, wid, rect);
            }

            // Handle stack_line
            if reactor.config_manager.config.settings.ui.stack_line.enabled {
                if let Some(tx) = &reactor.communication_manager.stack_line_tx {
                    let screen =
                        reactor.space_manager.screens.iter().find(|s| s.space == Some(*space));
                    if let Some(screen) = screen {
                        let group_infos = reactor
                            .layout_manager
                            .layout_engine
                            .collect_group_containers_in_selection_path(
                                *space,
                                screen.frame,
                                reactor.config_manager.config.settings.ui.stack_line.thickness(),
                                reactor
                                    .config_manager
                                    .config
                                    .settings
                                    .ui
                                    .stack_line
                                    .horiz_placement,
                                reactor.config_manager.config.settings.ui.stack_line.vert_placement,
                            );

                        let groups: Vec<crate::actor::stack_line::GroupInfo> = group_infos
                            .into_iter()
                            .map(|g| crate::actor::stack_line::GroupInfo {
                                node_id: g.node_id,
                                space_id: *space,
                                container_kind: g.container_kind,
                                frame: g.frame,
                                total_count: g.total_count,
                                selected_index: g.selected_index,
                            })
                            .collect();
                        if let Err(e) =
                            tx.try_send(crate::actor::stack_line::Event::GroupsUpdated {
                                space_id: *space,
                                groups,
                            })
                        {
                            tracing::warn!("Failed to send groups update to stack_line: {}", e);
                        }
                    }
                }
            }

            let suppress_animation = is_workspace_switch
                || reactor.workspace_switch_manager.active_workspace_switch.is_some();
            if suppress_animation {
                any_frame_changed |= AnimationManager::instant_layout(reactor, layout, skip_wid);
            } else {
                any_frame_changed |=
                    AnimationManager::animate_layout(reactor, *space, layout, is_resize, skip_wid);
            }
        }

        // Update workspace overlays based on offscreen window state
        // Window raise tracking in the manager prevents redundant operations and infinite loops
        for (display_id, screen) in reactor.space_manager.screens.iter().enumerate() {
            let active_workspace = reactor.layout_manager.layout_engine.active_workspace(display_id);

            // Count offscreen windows on this display
            let offscreen_count = reactor
                .layout_manager
                .layout_engine
                .windows_to_hide()
                .filter(|wid| {
                    reactor.layout_manager.layout_engine.offscreen_display_for_window(*wid) == Some(display_id)
                })
                .count();

            // Get visible (non-offscreen) windows in the active workspace
            let visible_windows: Vec<_> = if let Some(active_ws) = active_workspace {
                reactor
                    .layout_manager
                    .layout_engine
                    .virtual_workspace_manager()
                    .windows_in_workspace(display_id, active_ws)
                    .filter(|wid| !reactor.layout_manager.layout_engine.is_window_offscreen(*wid))
                    .collect()
            } else {
                Vec::new()
            };

            // Show overlay whenever there are offscreen windows (to hide them)
            let should_show_overlay = offscreen_count > 0;

            println!("[WORKSPACE_OVERLAY] Display {}: active_ws={:?}, offscreen={}, visible={}, show_overlay={}",
                     display_id, active_workspace, offscreen_count, visible_windows.len(), should_show_overlay);

            if should_show_overlay {
                // First, send offscreen windows to the back
                let offscreen_windows: Vec<_> = reactor
                    .layout_manager
                    .layout_engine
                    .windows_to_hide()
                    .filter(|wid| {
                        reactor.layout_manager.layout_engine.offscreen_display_for_window(*wid) == Some(display_id)
                    })
                    .collect();

                // Order offscreen windows below the overlay (will be done after overlay is shown)

                // Show overlay (state tracking prevents redundant operations)
                match reactor.workspace_overlay_manager.show_overlay(display_id, screen.frame) {
                    Ok(overlay_state_changed) => {
                        // Raise windows if: overlay just appeared OR window set changed
                        let window_set_changed = reactor.workspace_overlay_manager.should_raise_windows(display_id, visible_windows.clone());
                        let needs_raise = overlay_state_changed || window_set_changed;

                        println!("[WORKSPACE_OVERLAY] Display {}: overlay_changed={}, windows_changed={}, needs_raise={}, visible_count={}",
                                 display_id, overlay_state_changed, window_set_changed, needs_raise, visible_windows.len());

                        println!("[WORKSPACE_OVERLAY] About to check raise condition: needs_raise={}, empty={}", needs_raise, visible_windows.is_empty());

                        if needs_raise && !visible_windows.is_empty() {
                            println!("[WORKSPACE_OVERLAY] Raising {} visible windows above overlay (overlay_changed={}, windows_changed={})",
                                     visible_windows.len(), overlay_state_changed, window_set_changed);

                            // Group windows by PID since each app thread handles its own windows
                            let mut windows_by_app: crate::common::collections::HashMap<crate::sys::app::pid_t, Vec<WindowId>> =
                                crate::common::collections::HashMap::default();

                            for wid in &visible_windows {
                                windows_by_app.entry(wid.pid).or_default().push(*wid);
                            }

                            println!("[WORKSPACE_OVERLAY] Grouped into {} apps", windows_by_app.len());

                            // Collect app handles and group windows for raising
                            let mut app_handles = crate::common::collections::HashMap::default();
                            let mut raise_windows_groups = Vec::new();

                            for (pid, windows) in windows_by_app {
                                if let Some(app) = reactor.app_manager.apps.get(&pid) {
                                    app_handles.insert(pid, app.handle.clone());
                                    raise_windows_groups.push(windows);
                                } else {
                                    println!("[WORKSPACE_OVERLAY] WARNING: No app handle found for pid {}", pid);
                                }
                            }

                            println!("[WORKSPACE_OVERLAY] Collected {} app handles", app_handles.len());

                            // Send raise request with properly grouped windows
                            if !app_handles.is_empty() {
                                println!("[WORKSPACE_OVERLAY] Sending raise request for {} window groups", raise_windows_groups.len());
                                _ = reactor
                                    .communication_manager
                                    .raise_manager_tx
                                    .send(crate::actor::raise_manager::Event::RaiseRequest(
                                        crate::actor::reactor::RaiseRequest {
                                            raise_windows: raise_windows_groups,
                                            focus_window: None,
                                            app_handles,
                                        }
                                    ));
                            } else {
                                println!("[WORKSPACE_OVERLAY] ERROR: No app handles available, cannot raise windows");
                            }
                        } else {
                            println!("[WORKSPACE_OVERLAY] NOT raising windows: needs_raise={}, is_empty={}", needs_raise, visible_windows.is_empty());
                        }

                        // ALWAYS maintain z-order
                        // This ensures the order is preserved even if other operations re-order windows

                        if visible_windows.is_empty() {
                            // No visible windows - overlay should be on top to hide all offscreen windows
                            if !offscreen_windows.is_empty() {
                                println!("[WORKSPACE_OVERLAY] Empty workspace: ordering overlay above {} offscreen windows", offscreen_windows.len());
                                // Order overlay above all offscreen windows
                                for offscreen_wid in &offscreen_windows {
                                    if let Some(window_state) = reactor.window_manager.windows.get(offscreen_wid) {
                                        if let Some(ws_id) = window_state.window_server_id {
                                            if let Err(e) = reactor.workspace_overlay_manager.order_overlay_above(display_id, ws_id.into()) {
                                                println!("[WORKSPACE_OVERLAY] Failed to order overlay above offscreen window {:?}: {:?}", ws_id, e);
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            // Have visible windows - maintain z-order: offscreen < overlay < visible

                            // First, order overlay above all offscreen windows to hide them
                            if !offscreen_windows.is_empty() {
                                for offscreen_wid in &offscreen_windows {
                                    if let Some(window_state) = reactor.window_manager.windows.get(offscreen_wid) {
                                        if let Some(ws_id) = window_state.window_server_id {
                                            if let Err(e) = reactor.workspace_overlay_manager.order_overlay_above(display_id, ws_id.into()) {
                                                println!("[WORKSPACE_OVERLAY] Failed to order overlay above offscreen window {:?}: {:?}", ws_id, e);
                                            }
                                        }
                                    }
                                }
                            }

                            // Then, order overlay below ALL visible windows
                            // This ensures every visible window (including Chrome) is above the overlay
                            for visible_wid in &visible_windows {
                                if let Some(window_state) = reactor.window_manager.windows.get(visible_wid) {
                                    if let Some(ws_id) = window_state.window_server_id {
                                        if let Err(e) = reactor.workspace_overlay_manager.order_overlay_behind(display_id, ws_id.into()) {
                                            println!("[WORKSPACE_OVERLAY] Failed to order overlay below visible window {:?}: {:?}", ws_id, e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        println!("[WORKSPACE_OVERLAY] Failed to show overlay for display {}: {:?}", display_id, e);
                    }
                }
            } else {
                // No offscreen windows, hide the overlay and clear raised window tracking
                if let Err(e) = reactor.workspace_overlay_manager.hide_overlay(display_id) {
                    println!("[WORKSPACE_OVERLAY] Failed to hide overlay for display {}: {:?}", display_id, e);
                }
                // Clear the raised windows tracking when hiding overlay
                reactor.workspace_overlay_manager.should_raise_windows(display_id, Vec::new());
            }
        }

        reactor.maybe_send_menu_update();
        Ok(any_frame_changed)
    }
}

/// Manages window server information
pub struct WindowServerInfoManager {
    pub window_server_info: HashMap<WindowServerId, WindowServerInfo>,
}

/// Manages main window tracking
pub struct MainWindowTrackerManager {
    pub main_window_tracker: MainWindowTracker,
}

/// Manages pending space changes
pub struct PendingSpaceChangeManager {
    pub pending_space_change: Option<PendingSpaceChange>,
}
