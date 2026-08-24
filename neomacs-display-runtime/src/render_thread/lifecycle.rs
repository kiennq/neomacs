use super::RenderApp;
use super::state::RenderGpuContext;
use crate::thread_comm::InputEvent;
#[cfg(test)]
use neovm_core::window::GuiFrameGeometryHints;
use winit::event_loop::{ActiveEventLoop, ControlFlow};

impl RenderApp {
    pub(super) fn handle_daemon_primary_destroyed(&mut self, emacs_frame_id: u64) -> bool {
        if !self.lifecycle_flags.daemon_mode
            || self.frame_windows.primary_window().is_none()
            || self.frame_windows.primary_frame_id() != Some(emacs_frame_id)
        {
            return false;
        }

        self.comms
            .send_input(InputEvent::WindowClose { emacs_frame_id });
        self.frame_windows.take_primary_window();
        self.frame_windows.clear_primary_mapping();
        self.lifecycle_flags.primary_deferred = self
            .frame_windows
            .promote_active_secondary_to_primary()
            .is_none();
        self.lifecycle_flags.shutdown_requested = false;
        self.frame_coordinator
            .remove_window(super::frame_sched::NativeWindowId(emacs_frame_id));
        true
    }

    #[cfg(test)]
    pub(super) fn install_first_client_as_primary(&mut self, emacs_frame_id: u64) {
        self.frame_windows.set_primary_pending_request(
            emacs_frame_id,
            800,
            600,
            "test".to_owned(),
            GuiFrameGeometryHints {
                base_width: 0,
                base_height: 0,
                min_width: 0,
                min_height: 0,
                width_inc: 1,
                height_inc: 1,
            },
        );
        self.lifecycle_flags.primary_deferred = false;
    }

    fn collect_monitor_snapshot(
        event_loop: &ActiveEventLoop,
    ) -> Vec<crate::thread_comm::MonitorInfo> {
        let mut monitors = Vec::new();
        for monitor in event_loop.available_monitors() {
            let pos = monitor.position();
            let size = monitor.size();
            let scale = monitor.scale_factor();
            let name = monitor.name();
            let width_mm = if scale > 0.0 {
                (size.width as f64 * 25.4 / (96.0 * scale)) as i32
            } else {
                0
            };
            let height_mm = if scale > 0.0 {
                (size.height as f64 * 25.4 / (96.0 * scale)) as i32
            } else {
                0
            };
            monitors.push(crate::thread_comm::MonitorInfo {
                x: pos.x,
                y: pos.y,
                width: size.width as i32,
                height: size.height as i32,
                scale,
                width_mm,
                height_mm,
                name,
            });
        }
        monitors
    }

    fn refresh_monitor_snapshot(&mut self, event_loop: &ActiveEventLoop, emit_change_event: bool) {
        let snapshot = Self::collect_monitor_snapshot(event_loop);
        let had_snapshot = self.monitors_populated;
        let changed = !had_snapshot || self.last_monitor_snapshot != snapshot;

        if !changed {
            return;
        }

        self.last_monitor_snapshot = snapshot.clone();
        self.monitors_populated = true;

        for monitor in &snapshot {
            tracing::info!(
                "Monitor: {:?} pos=({},{}) size={}x{} scale={} mm={}x{}",
                monitor.name,
                monitor.x,
                monitor.y,
                monitor.width,
                monitor.height,
                monitor.scale,
                monitor.width_mm,
                monitor.height_mm
            );
        }

        if let Some(ref shared) = self.shared_monitors {
            let (ref lock, ref cvar) = **shared;
            if let Ok(mut shared) = lock.lock() {
                *shared = snapshot.clone();
                cvar.notify_all();
            }
        }

        if emit_change_event && had_snapshot {
            self.comms
                .send_input(InputEvent::MonitorsChanged { monitors: snapshot });
        }
    }

    pub(super) fn handle_resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.lifecycle_flags.resumed_seen {
            tracing::info!(
                "Render thread resumed: primary_window_exists={} size={}x{} title={:?}",
                self.frame_windows
                    .primary_window()
                    .and_then(|ws| ws.window())
                    .is_some(),
                self.frame_windows
                    .primary_window()
                    .map_or((0, 0), |ws| ws.native_size())
                    .0,
                self.frame_windows
                    .primary_window()
                    .map_or((0, 0), |ws| ws.native_size())
                    .1,
                self.frame_windows
                    .primary_window()
                    .map_or_else(String::new, |window| window.chrome().title.clone())
            );
            self.lifecycle_flags.resumed_seen = true;
        }
        let needs_native = self
            .frame_windows
            .primary_window()
            .is_some_and(|ws| !ws.lifecycle.is_active());
        if needs_native {
            self.create_pending_primary(event_loop);
        }

        self.refresh_monitor_snapshot(event_loop, false);
    }

    pub(super) fn handle_about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        super::frame_stats::count(&super::frame_stats::EVENT_LOOP_WAKEUPS);
        super::frame_stats::maybe_log_snapshot(std::time::Instant::now());
        if !self.lifecycle_flags.about_to_wait_seen {
            tracing::info!(
                "Render thread entered about_to_wait: primary_window_exists={} frame_windows={}",
                self.frame_windows
                    .primary_window()
                    .and_then(|ws| ws.window())
                    .is_some(),
                self.frame_windows.count()
            );
            self.lifecycle_flags.about_to_wait_seen = true;
        }
        if self.lifecycle_flags.shutdown_requested {
            event_loop.exit();
            return;
        }
        // Device-loss recovery (SHADER_SURFACES.md: user shader hang → TDR):
        // latched by the wgpu device-lost callback, by a streak of
        // consecutive surface-Lost acquisitions, or by the debug simulation
        // command. Rebuild the whole GPU stack before doing anything else
        // with it.
        if self.device_lost.take() {
            self.recover_from_device_loss(event_loop);
        }
        self.refresh_monitor_snapshot(event_loop, true);
        if self.process_commands() {
            event_loop.exit();
            return;
        }
        self.create_pending_primary(event_loop);

        // Decoder workers cannot wake winit directly. Poll their result channel
        // while work is pending so decoded image metadata and pixels become visible.
        self.process_pending_images();

        if let Some(gpu) = &self.gpu {
            self.frame_windows.process_creates(
                event_loop,
                &mut self.window_icon,
                &gpu.instance,
                &gpu.device,
                &gpu.adapter,
            );
        }
        self.frame_windows.process_destroys();

        self.poll_frame();

        // Decoder workers wake this loop only after publishing a control
        // event or replacing their bounded latest-frame slot. Service after
        // frame ingestion so visibility routing sees the newest accepted
        // root/child presentation.
        self.process_video_frames(std::time::Instant::now());

        self.pump_glib();

        let now = std::time::Instant::now();
        self.frame_windows.tick_top_level_cursor_blinks(
            now,
            self.effects.cursor_wake.enabled,
            self.renderer.as_ref(),
        );

        self.frame_windows.tick_top_level_cursor_animations();

        self.frame_windows.tick_top_level_cursor_size_animations();

        if self.effects.idle_dim.enabled {
            let idle_dim_config = self.effects.idle_dim.clone();
            self.frame_windows.tick_top_level_idle_dim(&idle_dim_config);
        } else {
            self.frame_windows.clear_top_level_idle_dim();
        }

        if self.effects.cursor_pulse.enabled && self.effects.cursor_glow.enabled {
            self.frame_windows.mark_top_level_dirty();
        }

        self.frame_windows.mark_active_top_level_visuals_dirty();

        if self.has_terminal_activity() {
            self.frame_windows.mark_top_level_dirty();
        }

        #[cfg(feature = "wpe-webkit")]
        if self.has_webkit_needing_redraw() {
            self.frame_windows
                .for_each_top_level_window_mut(|window_state| {
                    if !window_state.render.floating_webkits.is_empty() {
                        window_state.render.mark_dirty();
                    }
                });
        }

        // Stage 2 of the frame scheduling plan: legacy activity latches are
        // reconciled into the persistent frame coordinator, which owns
        // one-shot redraw requests (coalesced per window) and the loop's
        // wake deadline. Continuous activity is paced at the estimated
        // display cadence instead of a 4 ms poll; new-content demand fires
        // immediately on its first frame after idle.
        let now = std::time::Instant::now();
        self.declare_frame_demands(now);
        // Publish per-window active demand for diagnostics (plan:
        // Observability). Runs only on loop wakes that already reconcile
        // demand: a fully idle loop publishes nothing and is never woken by
        // the counters.
        super::frame_stats::publish_window_demand(self.frame_coordinator.window_demand());
        // Service the schedule before arming the wait. A deadline that has
        // come due is work, not a timeout: GNU's timer_check runs every ripe
        // timer and only then yields the pselect wait
        // (keyboard.c:4911-4945 -> process.c:5490). Demands whose producer
        // does not re-declare them every pass -- the bounded Expose retry
        // finish_frame arms after a present produces nothing -- have no other
        // path from deadline to frame, and arming an elapsed WaitUntil returns
        // immediately, so the loop would spin at zero wait forever.
        let mut service = self.frame_coordinator.service_deadlines(now);
        if service.video_service_due {
            // A media deadline may become ripe after the unconditional video
            // service pass near the start of this event-loop iteration. Run
            // the producer again before sleeping so the deadline cannot be
            // consumed without selecting its newly due frame. Servicing at
            // the same scheduler timestamp guarantees that the replacement
            // deadline is either absent or in the future.
            self.process_video_frames(now);
            let reconciled = self.frame_coordinator.service_deadlines(now);
            debug_assert!(
                !reconciled.video_service_due,
                "video service must return no deadline at or before its service time"
            );
            service.redraw.extend(reconciled.redraw);
            service.video_service_due = reconciled.video_service_due;
            service.wake = reconciled.wake;
        }
        for id in &service.redraw {
            super::frame_stats::count(&super::frame_stats::DEADLINE_SERVICED_REDRAWS);
            if let Some(window_state) = self.frame_windows.get(id.0) {
                window_state.request_redraw();
            }
        }
        let mut deadline = match service.wake {
            super::frame_sched::LoopWake::At(at) => Some(at.instant()),
            super::frame_sched::LoopWake::Idle => None,
        };
        // GLib service wake (frame scheduling plan, invariant 1 carve-out):
        // WPE WebKit needs its thread-default GMainContext pumped for IPC,
        // networking, and JS timers even when no frame is needed. While any
        // WebKit view is alive, cap the wake at a bounded service interval so
        // pump_glib runs regularly; this is a wake, not frame demand — it
        // renders nothing unless separate demand exists. With no WebKit view
        // there is no service wake and the loop may Wait indefinitely.
        if self.has_live_webkit_views() {
            const WPE_SERVICE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
            let service = now + WPE_SERVICE_INTERVAL;
            deadline = Some(deadline.map_or(service, |d| d.min(service)));
        }
        if self.has_pending_images() {
            const IMAGE_DECODE_POLL_INTERVAL: std::time::Duration =
                std::time::Duration::from_millis(16);
            let image_poll = now + IMAGE_DECODE_POLL_INTERVAL;
            deadline = Some(deadline.map_or(image_poll, |d| d.min(image_poll)));
        }

        match deadline {
            Some(deadline) => event_loop.set_control_flow(ControlFlow::WaitUntil(deadline)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    /// Whether any live WPE WebKit view exists, requiring its GMainContext to
    /// be serviced. Always false in builds without the `wpe-webkit` feature,
    /// where `pump_glib` is a no-op and the loop can Wait indefinitely.
    fn has_live_webkit_views(&self) -> bool {
        #[cfg(feature = "wpe-webkit")]
        {
            if !self.webkit_views.is_empty() {
                return true;
            }
            let mut any = false;
            self.frame_windows
                .for_each_top_level_window(|window_state| {
                    any |= !window_state.render.floating_webkits.is_empty();
                });
            any
        }
        #[cfg(not(feature = "wpe-webkit"))]
        {
            false
        }
    }

    /// Reconcile the legacy activity latches into the persistent frame
    /// coordinator: declare demand for active signals, retract reasons whose
    /// signals ceased, and execute the coordinator's one-shot redraw
    /// requests. Continuous demand is paced at the estimated display cadence
    /// (the plan's bounded synthetic clock); this replaced the legacy 4 ms
    /// active poll.
    fn declare_frame_demands(&mut self, now: std::time::Instant) {
        use super::frame_sched::{
            Cadence, Damage, DemandReason, FrameDemand, Invalidation, LayerMask, NativeWindowId,
            PacingAction,
        };
        const LEGACY_IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(16);
        /// Loop-global demands (idle poll) with no specific frame window.
        /// Compat-only; deleted when poll_when_idle's owners migrate.
        const LOOP_WINDOW: NativeWindowId = NativeWindowId(u64::MAX);

        // The coordinator is keyed by the event frame id: 0 for the primary
        // window before Emacs adopts it, the Emacs frame id afterwards
        // (matching RedrawRequested dispatch).
        let native_window_id = |key: &super::frame_windows::FrameKey| match key {
            super::frame_windows::FrameKey::Pending => NativeWindowId(0),
            super::frame_windows::FrameKey::Adopted(id) => NativeWindowId(*id),
        };
        let legacy_repaint = Invalidation::RepaintLayers {
            layers: LayerMask::all(),
            damage: Damage::FullLayer,
        };

        // WebKit and shader-surface signals remain process-wide. Video is
        // presentation-indexed below and never wakes unrelated windows.
        let webkit_active = self.has_webkit_needing_redraw();
        let surfaces_active = self.has_active_shader_surfaces();
        let frame_shader_installed = self
            .renderer
            .as_ref()
            .is_some_and(|renderer| renderer.has_frame_post());
        // Destroyed windows must not keep waking the loop.
        let live: std::collections::HashSet<NativeWindowId> = self
            .frame_windows
            .windows
            .keys()
            .map(&native_window_id)
            .collect();
        self.frame_coordinator
            .prune_windows(|id| id == LOOP_WINDOW || live.contains(&id));

        for (key, window_state) in &self.frame_windows.windows {
            let id = native_window_id(key);
            if window_state.window().is_none() {
                // A window with no native surface cannot render; drop any
                // scheduling state so a stale outstanding-request token or
                // deadline cannot survive to its activation.
                self.frame_coordinator.remove_window(id);
                continue;
            }
            let max_rate = Self::window_max_rate(window_state);
            let dynamic_animation_rate = self.render_policy.dynamic_animation_rate(max_rate);
            let dynamic_effects_allowed = dynamic_animation_rate.is_some();
            let dynamic_animation_rate = dynamic_animation_rate.unwrap_or(max_rate);
            let frame_shader_active = self.render_policy.frame_post_scheduler_active(
                frame_shader_installed,
                window_state.render.compositor.current_frame.is_some()
                    && window_state.render.present_mapping().is_some(),
            );

            // Stage 6: each active render-effect family (and dirty content,
            // cursor animation, and transitions) submits its own typed demand
            // so diagnostics can attribute the demand, rather than one opaque
            // catch-all. All use the same repaint invalidation and cadence, so
            // aggregate behavior is unchanged; only the reason differs.
            let fx = &window_state.render.compositor.renderer_effects;
            let effect_demands = [
                (
                    window_state.has_presentable_dirty_content(),
                    DemandReason::Redisplay,
                    max_rate,
                ),
                (
                    dynamic_effects_allowed && fx.cursor_effects_active(),
                    DemandReason::CursorEffect,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed && fx.window_effects_active(),
                    DemandReason::WindowEffect,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed && fx.text_effects_active(),
                    DemandReason::TextEffect,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed && fx.scroll_effects_active(),
                    DemandReason::ScrollEffect,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed && fx.decorative_effects_active(),
                    DemandReason::DecorativeEffect,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed && fx.transient_effects_active(),
                    DemandReason::TransientEffect,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed && window_state.render.cursor.is_animating(),
                    DemandReason::CursorAnimation,
                    dynamic_animation_rate,
                ),
                (
                    dynamic_effects_allowed
                        && window_state.render.compositor.transitions.has_active(),
                    DemandReason::Transition,
                    dynamic_animation_rate,
                ),
            ];
            let mut action = PacingAction::Sleep;
            for (active, reason, rate) in effect_demands {
                if active {
                    let a = self.frame_coordinator.submit_demand(
                        id,
                        FrameDemand {
                            invalidation: legacy_repaint,
                            cadence: Cadence::MaxRate(rate),
                            reason,
                        },
                        now,
                    );
                    if a == PacingAction::RequestRedraw {
                        action = PacingAction::RequestRedraw;
                    }
                } else {
                    self.frame_coordinator.retract(id, reason);
                }
            }

            // Shader surfaces may cap their animation rate (`:fps`): when they
            // are the demand, throttle the compositor cadence to the max of
            // their caps so an ambient background shader lets the frame idle
            // instead of pinning it at display refresh (battery).
            let surface_rate = if surfaces_active {
                let capped = self.shader_surface_demand_rate(u32::from(max_rate.get()));
                std::num::NonZeroU16::new(capped.min(u32::from(max_rate.get())) as u16)
                    .unwrap_or(max_rate)
            } else {
                max_rate
            };

            for (active, reason, rate) in [
                (webkit_active, DemandReason::WebKit, max_rate),
                (surfaces_active, DemandReason::ShaderSurface, surface_rate),
            ] {
                if active {
                    let media_action = self.frame_coordinator.submit_demand(
                        id,
                        FrameDemand {
                            invalidation: legacy_repaint,
                            cadence: Cadence::MaxRate(rate),
                            reason,
                        },
                        now,
                    );
                    if media_action == PacingAction::RequestRedraw {
                        action = PacingAction::RequestRedraw;
                    }
                } else {
                    self.frame_coordinator.retract(id, reason);
                }
            }

            // A full-frame shader samples the retained scene at a new
            // presentation time; editor content and glyph geometry have not
            // changed. Keep that demand compositor-only so animated post
            // processing never rebuilds the scene at display cadence.
            if frame_shader_active {
                let shader_action = self.frame_coordinator.submit_demand(
                    id,
                    FrameDemand {
                        invalidation: Invalidation::CompositeOnly {
                            layers: LayerMask::FRAME_POST,
                        },
                        cadence: Cadence::MaxRate(max_rate),
                        reason: DemandReason::FrameShader,
                    },
                    now,
                );
                if shader_action == PacingAction::RequestRedraw {
                    action = PacingAction::RequestRedraw;
                }
            } else {
                self.frame_coordinator
                    .retract(id, DemandReason::FrameShader);
            }

            // Infinite ambient effect: cursor color cycle animates whenever a
            // cursor exists in a committed frame. Its configured cadence is
            // non-zero in the control-plane type and capped to the display rate
            // at this scheduling boundary; the draw path no longer latches
            // continuation. Policy (Stage 7): unfocused windows and blinked-off
            // cursors pause ambient demand, while hollow cursor visuals do not
            // contribute to the aggregate rate because the cycle cannot change
            // their pixels. Color remains a function of elapsed presentation
            // time, not the number of ticks.
            let cycle_frame = if dynamic_effects_allowed {
                window_state.render.compositor.current_frame.as_ref()
            } else {
                None
            };
            let cycle_action = Self::reconcile_cursor_color_cycle_demand(
                &mut self.frame_coordinator,
                id,
                cycle_frame,
                &self.effects,
                dynamic_animation_rate,
                window_state.render.cursor.blink_on,
                now,
            );
            if cycle_action == PacingAction::RequestRedraw {
                action = PacingAction::RequestRedraw;
            }

            // A blink toggle that already happened needs its frame now. It
            // changes only the cursor layer, so it asks for a composite of the
            // retained scene; a content repaint owed on the same pass has
            // already submitted the stronger Redisplay demand above and wins
            // the strongest-invalidation merge.
            if window_state.has_presentable_cursor_change() {
                let cursor_action = self.frame_coordinator.submit_demand(
                    id,
                    FrameDemand {
                        invalidation: Invalidation::CompositeOnly {
                            layers: LayerMask::CURSOR_EFFECTS,
                        },
                        cadence: Cadence::NextPresentation,
                        reason: DemandReason::CursorAnimation,
                    },
                    now,
                );
                if cursor_action == PacingAction::RequestRedraw {
                    action = PacingAction::RequestRedraw;
                }
            }

            match window_state.render.cursor.next_blink_deadline() {
                Some(blink) => {
                    self.frame_coordinator.submit_demand(
                        id,
                        FrameDemand {
                            invalidation: Invalidation::CompositeOnly {
                                layers: LayerMask::CURSOR_EFFECTS,
                            },
                            cadence: Cadence::At(blink),
                            reason: DemandReason::CursorAnimation,
                        },
                        now,
                    );
                }
                None => {
                    self.frame_coordinator
                        .retract(id, DemandReason::CursorAnimation);
                }
            }

            if action == PacingAction::RequestRedraw {
                window_state.request_redraw();
            }
        }

        if should_arm_legacy_idle_poll(
            self.lifecycle_flags.poll_when_idle,
            self.frame_windows
                .primary_window()
                .is_some_and(|primary| primary.lifecycle.is_active()),
        ) {
            self.frame_coordinator.submit_demand(
                LOOP_WINDOW,
                FrameDemand {
                    invalidation: legacy_repaint,
                    cadence: Cadence::At(now + LEGACY_IDLE_POLL),
                    reason: DemandReason::Redisplay,
                },
                now,
            );
        } else {
            self.frame_coordinator.remove_window(LOOP_WINDOW);
        }
    }

    /// Convert a monitor-reported refresh rate into the scheduler's display
    /// limit. A real, low refresh rate is preserved: the limit must never make
    /// an effect run faster than the display can present. Missing or zero
    /// reports fall back to 60 Hz, and implausibly high reports are bounded.
    pub(super) fn display_rate_limit(refresh_rate_millihertz: Option<u32>) -> std::num::NonZeroU16 {
        let hz = refresh_rate_millihertz
            .filter(|millihertz| *millihertz != 0)
            .map(|millihertz| {
                (u64::from(millihertz) / 1_000)
                    .clamp(1, 240)
                    .try_into()
                    .expect("the display-rate bound fits u16")
            })
            .unwrap_or(60);
        std::num::NonZeroU16::new(hz).expect("the display-rate limit is non-zero")
    }

    /// Estimated maximum presentation cadence for a window, derived from the
    /// current monitor and defaulting to 60 Hz when it is unavailable.
    pub(super) fn window_max_rate(
        window_state: &super::frame_windows::GuiFrameWindowState,
    ) -> std::num::NonZeroU16 {
        let reported_millihertz = window_state
            .window()
            .and_then(|window| window.current_monitor())
            .and_then(|monitor| monitor.refresh_rate_millihertz());
        Self::display_rate_limit(reported_millihertz)
    }

    /// Convert the user-facing integer rate into the scheduler's valid rate
    /// domain, never scheduling faster than the target display can present.
    pub(super) fn cursor_color_cycle_rate(
        configured_fps: neomacs_display_protocol::FrameRate,
        display_max_rate: std::num::NonZeroU16,
    ) -> std::num::NonZeroU16 {
        let display_hz = u32::from(display_max_rate.get());
        let effective_hz = configured_fps.get().min(display_hz);
        std::num::NonZeroU16::new(effective_hz as u16)
            .expect("a non-zero frame rate capped to a non-zero display rate stays non-zero")
    }

    /// Derive standing cursor-cycle demand from every cursor visual the
    /// renderer will color-cycle. `None` represents no visible cycle work;
    /// otherwise the fastest enabled profile wins and is display-capped.
    pub(super) fn cursor_color_cycle_cadence(
        frame: &crate::core::frame_glyphs::FrameGlyphBuffer,
        global_effects: &neomacs_display_protocol::EffectsConfig,
        display_max_rate: std::num::NonZeroU16,
        cursor_visible: bool,
    ) -> Option<std::num::NonZeroU16> {
        if !cursor_visible {
            return None;
        }
        frame
            .window_cursors
            .iter()
            .filter(|cursor| !cursor.style.is_hollow())
            .filter_map(|cursor| {
                let cycle = &frame
                    .effective_window_cursor_effects(cursor.window_id, global_effects)
                    .cursor_color_cycle;
                cycle
                    .enabled
                    .then(|| Self::cursor_color_cycle_rate(cycle.fps, display_max_rate))
            })
            .max_by_key(|rate| rate.get())
    }

    /// Translate renderer-visible cursor state into one typed standing demand.
    /// This is the policy seam between visual configuration and the generic
    /// frame coordinator: lifecycle code does not reconstruct the cadence,
    /// invalidation, or diagnostic reason independently.
    pub(super) fn cursor_color_cycle_demand(
        frame: &crate::core::frame_glyphs::FrameGlyphBuffer,
        global_effects: &neomacs_display_protocol::EffectsConfig,
        display_max_rate: std::num::NonZeroU16,
        cursor_visible: bool,
        window_focused: bool,
    ) -> Option<super::frame_sched::FrameDemand> {
        use super::frame_sched::{Cadence, DemandReason, FrameDemand, Invalidation, LayerMask};

        if !window_focused {
            return None;
        }
        let rate = Self::cursor_color_cycle_cadence(
            frame,
            global_effects,
            display_max_rate,
            cursor_visible,
        )?;
        Some(FrameDemand {
            invalidation: Invalidation::CompositeOnly {
                layers: LayerMask::CURSOR_EFFECTS,
            },
            cadence: Cadence::MaxRate(rate),
            reason: DemandReason::CursorColorCycle,
        })
    }

    /// Atomically reconcile the cursor-cycle producer's standing demand with
    /// the generic coordinator. This is the runtime integration seam: absent,
    /// blinked-off, hollow, disabled, or unfocused cursor state withdraws the
    /// producer; visible cycling state submits its exact typed demand.
    pub(super) fn reconcile_cursor_color_cycle_demand(
        coordinator: &mut super::frame_sched::FrameCoordinator,
        id: super::frame_sched::NativeWindowId,
        frame: Option<&crate::core::frame_glyphs::FrameGlyphBuffer>,
        global_effects: &neomacs_display_protocol::EffectsConfig,
        display_max_rate: std::num::NonZeroU16,
        cursor_visible: bool,
        now: std::time::Instant,
    ) -> super::frame_sched::PacingAction {
        use super::frame_sched::{DemandReason, PacingAction};

        let demand = frame.and_then(|frame| {
            Self::cursor_color_cycle_demand(
                frame,
                global_effects,
                display_max_rate,
                cursor_visible,
                coordinator.is_focused(id),
            )
        });
        match demand {
            Some(demand) => coordinator.submit_demand(id, demand, now),
            None => {
                coordinator.retract(id, DemandReason::CursorColorCycle);
                PacingAction::Sleep
            }
        }
    }

    pub(super) fn handle_exiting(&mut self) {
        // Explicitly drop wgpu resources while the Wayland connection is still alive.
        // Without this, RenderApp's implicit drop happens AFTER the event loop's
        // Wayland display is torn down, causing SEGV in eglTerminate → dri2_teardown_wayland.
        //
        // wgpu uses internal Arc reference counting: the Adapter holds Arc<Instance>,
        // and Device/Surface/Texture objects hold indirect Arc references back to it.
        // Even after .take()'ing all Option fields, other RenderApp fields (transition
        // textures, child frames, etc.) may still hold transitive Arc references that
        // keep the EGL Instance alive until the final implicit drop of RenderApp —
        // at which point the Wayland connection is already torn down.
        //
        // Solution: leak the adapter to prevent eglTerminate from ever running.
        // The OS reclaims all GPU resources on process exit anyway.
        tracing::info!("Event loop exiting, cleaning up GPU resources");

        self.window_icon.shutdown();

        // The Wayland clipboard borrows Winit's wl_display. Stop its worker
        // before dropping any native windows or the event-loop connection.
        drop(std::mem::replace(
            &mut self.clipboard,
            Err("display is shutting down".to_owned()),
        ));

        // Drop WebKit views and WPE backend (hold EGL contexts)
        #[cfg(feature = "wpe-webkit")]
        {
            self.webkit_views.clear();
            self.wpe_backend = None;
        }
        // Drop renderer (holds device/queue references, textures, pipelines)
        drop(self.renderer.take());
        // Drop adopted primary state (surface holds wl_surface proxy if on Wayland)
        drop(self.frame_windows.take_primary_window());
        // Drop multi-window state (secondary surfaces)
        self.frame_windows.destroy_all();
        // Leak the adapter to prevent eglTerminate crash on Wayland.
        // The adapter's Drop triggers eglTerminate → dri2_teardown_wayland which
        // SEGVs if the Wayland connection is already gone. Since we're exiting,
        // the OS will reclaim all GPU/EGL resources.
        if let Some(gpu) = self.gpu.take() {
            let RenderGpuContext {
                instance,
                adapter,
                device,
                queue,
            } = gpu;
            drop(device);
            drop(queue);
            drop(instance);
            std::mem::forget(adapter);
        }

        tracing::info!("GPU resources cleaned up");
    }
}

fn should_arm_legacy_idle_poll(poll_when_idle: bool, primary_active: bool) -> bool {
    poll_when_idle && primary_active
}

#[cfg(test)]
mod tests {
    use super::super::frame_sched::{
        Cadence, Damage, DemandReason, FrameCoordinator, FrameDemand, Invalidation, LayerMask,
        NativeWindowId,
    };
    use super::super::state::RenderStartupMode;
    use super::RenderApp;
    use std::time::Instant;

    const LOOP_WINDOW: NativeWindowId = NativeWindowId(u64::MAX);

    fn legacy_idle_poll_demand(now: Instant) -> FrameDemand {
        FrameDemand {
            invalidation: Invalidation::RepaintLayers {
                layers: LayerMask::all(),
                damage: Damage::FullLayer,
            },
            cadence: Cadence::At(now + std::time::Duration::from_millis(16)),
            reason: DemandReason::Redisplay,
        }
    }

    #[test]
    fn pending_primary_does_not_arm_legacy_idle_poll_deadline() {
        let mut app = RenderApp::new_for_test(RenderStartupMode::DeferredPrimary);
        app.install_first_client_as_primary(42);

        app.declare_frame_demands(Instant::now());

        assert_eq!(app.frame_coordinator.next_wake_deadline_unserviced(), None);
    }

    #[test]
    fn active_primary_still_arms_legacy_idle_poll_deadline() {
        let mut coordinator = FrameCoordinator::new();
        let now = Instant::now();

        if super::should_arm_legacy_idle_poll(true, true) {
            coordinator.submit_demand(LOOP_WINDOW, legacy_idle_poll_demand(now), now);
        }

        assert!(coordinator.next_wake_deadline_unserviced().is_some());
    }
}
