use super::{RenderApp, RenderUserEvent, SharedImageMetadata, SharedMonitorInfo, surface_readback};
use crate::render_thread::frame_windows::{FrameLifecycle, GuiFrameNativeWindowState};
use crate::render_thread::state::{
    DeviceRecoveryTarget, PrimaryGpuInitialization, RenderGpuContext, RenderStartupMode,
    device_recovery_target, primary_gpu_initialization_plan,
};
use crate::render_thread::x11_hints::apply_window_geometry_hints;
use crate::thread_comm::{InputEvent, RenderComms};
use neomacs_renderer_wgpu::WgpuRenderer;
use std::sync::Arc;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
#[cfg(target_os = "linux")]
use winit::platform::wayland::EventLoopBuilderExtWayland;
#[cfg(target_os = "linux")]
use winit::platform::x11::EventLoopBuilderExtX11;
use winit::window::Window;
#[cfg(target_os = "linux")]
use x11_dl::xlib;

#[cfg(feature = "wpe-webkit")]
use crate::backend::wpe::WpeBackend;
#[cfg(all(feature = "wpe-webkit", wpe_platform_available))]
use crate::backend::wpe::sys::platform as plat;

impl RenderApp {
    #[cfg(test)]
    pub(super) fn record_primary_gpu_initialization(&mut self, kind: PrimaryGpuInitialization) {
        if kind != PrimaryGpuInitialization::Reuse {
            self.full_gpu_initializations += 1;
        }
        self.primary_surface_creations += 1;
    }

    pub(super) fn init_wgpu(&mut self, event_loop: &ActiveEventLoop, window: Arc<Window>) -> bool {
        tracing::info!("Initializing wgpu for render thread");

        let instance_descriptor =
            crate::wgpu_instance_descriptor_with_display(event_loop.owned_display_handle());
        tracing::info!(
            "wgpu requested backends: {:?}",
            instance_descriptor.backends
        );
        let instance = wgpu::Instance::new(instance_descriptor);

        let surface = match instance.create_surface(window.clone()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to create wgpu surface: {:?}", e);
                return false;
            }
        };

        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: crate::gpu_power_preference(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })) {
                Ok(a) => a,
                Err(e) => {
                    tracing::error!("Failed to find suitable GPU adapter: {:?}", e);
                    return false;
                }
            };

        let adapter_info = adapter.get_info();
        tracing::info!(
            "wgpu adapter: {} (vendor={:04x}, device={:04x}, backend={:?})",
            adapter_info.name,
            adapter_info.vendor,
            adapter_info.device,
            adapter_info.backend
        );

        let (device, queue) =
            match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("Neomacs Render Thread Device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: Default::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })) {
                Ok((d, q)) => (d, q),
                Err(e) => {
                    tracing::error!("Failed to create wgpu device: {:?}", e);
                    return false;
                }
            };

        let device = Arc::new(device);
        let queue = Arc::new(queue);

        // Device-loss detection (doc/display-engine/SHADER_SURFACES.md): a
        // user shader with an infinite loop can hang the GPU; the driver
        // resets (TDR) and this device is lost. Latch the loss and let
        // handle_about_to_wait rebuild the GPU state. The callback may fire
        // on any thread from inside wgpu's maintain paths. `Destroyed` is an
        // intentional teardown (shutdown or a recovery rebuild), never a
        // loss.
        let lost_flag = self.device_lost.shared_flag();
        device.set_device_lost_callback(move |reason, message| {
            if matches!(reason, wgpu::DeviceLostReason::Destroyed) {
                return;
            }
            tracing::error!(?reason, message, "wgpu device lost");
            lost_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });

        let phys = window.inner_size();
        let raw_scale_factor = window.scale_factor();
        let effective_scale = super::state::effective_window_scale_factor(raw_scale_factor);

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);

        let alpha_mode = if caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            caps.alpha_modes[0]
        };
        let surface_usage = surface_readback::surface_usage_for_debug_readback(
            caps.usages,
            &mut self.debug_first_frame_readback_pending,
            self.debug_surface_readback_frames_remaining > 0,
        );

        {
            let primary = self.frame_windows.primary_window_mut().unwrap();
            if let FrameLifecycle::Pending {
                width: pw,
                height: ph,
                scale_factor: sf,
                ..
            } = &mut primary.lifecycle
            {
                *pw = phys.width;
                *ph = phys.height;
                *sf = effective_scale;
            }
        }

        let (pending_width, pending_height) = self
            .frame_windows
            .primary_window()
            .unwrap()
            .lifecycle
            .native_size();
        let pending_scale_factor = self
            .frame_windows
            .primary_window()
            .unwrap()
            .lifecycle
            .scale_factor();

        let config = wgpu::SurfaceConfiguration {
            usage: surface_usage,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: pending_width,
            height: pending_height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let renderer = WgpuRenderer::with_device(
            device.clone(),
            queue.clone(),
            pending_width,
            pending_height,
            format,
            pending_scale_factor as f32,
        );

        tracing::info!(
            "wgpu initialized: {}x{}, format: {:?}",
            pending_width,
            pending_height,
            format
        );

        self.gpu = Some(RenderGpuContext {
            instance,
            adapter,
            device: device.clone(),
            queue: queue.clone(),
        });
        let mut renderer = renderer;
        if let Some(max_bytes) = super::state::media_budget_env_limit() {
            renderer.set_media_budget_limit(max_bytes);
        }
        self.renderer = Some(renderer);

        self.frame_windows
            .populate_primary_native(GuiFrameNativeWindowState {
                window,
                surface,
                surface_config: config,
                width: pending_width,
                height: pending_height,
                scale_factor: pending_scale_factor,
                chrome: {
                    let primary = self.frame_windows.primary_window().unwrap();
                    primary.lifecycle.chrome().clone()
                },
            });

        {
            let primary = self.frame_windows.primary_window_mut().unwrap();
            primary
                .render
                .populate_glyph_atlas(&device, pending_scale_factor);
            primary
                .render
                .cursor
                .apply_config(self.cursor_defaults.config_snapshot());
            primary
                .render
                .compositor
                .transitions
                .apply_policy(self.transition_policy);
        }

        let pending_frame_chrome = self
            .frame_windows
            .primary_window()
            .and_then(|ws| ws.render.compositor.current_frame.as_ref())
            .map(|frame| frame.frame_chrome.clone());
        if let Some(frame_chrome) = pending_frame_chrome.as_ref() {
            self.sync_frame_chrome_assets(frame_chrome);
        }

        // The WPE backend is independent of wgpu; keep the existing backend
        // (and its live views) when init_wgpu re-runs for device-loss
        // recovery.
        #[cfg(feature = "wpe-webkit")]
        if self.wpe_backend.is_none() {
            use crate::backend::wgpu::get_render_node_from_adapter_info;

            let render_node = get_render_node_from_adapter_info(&adapter_info)
                .map(|p| p.to_string_lossy().into_owned());

            tracing::info!("Initializing WPE backend (render_node: {:?})", render_node);

            match unsafe {
                WpeBackend::new_with_device(std::ptr::null_mut(), render_node.as_deref())
            } {
                Ok(backend) => {
                    tracing::info!("WPE backend initialized successfully");
                    self.wpe_backend = Some(backend);
                }
                Err(e) => {
                    tracing::warn!("Failed to initialize WPE backend: {:?}", e);
                }
            }
        }

        #[cfg(feature = "video")]
        tracing::info!("Video cache initialized");

        #[cfg(test)]
        self.record_primary_gpu_initialization(PrimaryGpuInitialization::Initialize);
        true
    }

    fn create_primary_surface_with_existing_gpu(&mut self, window: Arc<Window>) -> bool {
        let Some((instance, adapter, device)) = self
            .gpu
            .as_ref()
            .map(|gpu| (&gpu.instance, &gpu.adapter, gpu.device.clone()))
        else {
            return false;
        };
        let surface = match instance.create_surface(window.clone()) {
            Ok(surface) => surface,
            Err(error) => {
                tracing::error!("Failed to create primary surface: {:?}", error);
                return false;
            }
        };
        let caps = surface.get_capabilities(adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|format| format.is_srgb())
            .unwrap_or(caps.formats[0]);
        let alpha_mode = if caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            caps.alpha_modes[0]
        };
        let surface_usage = surface_readback::surface_usage_for_debug_readback(
            caps.usages,
            &mut self.debug_first_frame_readback_pending,
            self.debug_surface_readback_frames_remaining > 0,
        );
        let phys = window.inner_size();
        let scale_factor = super::state::effective_window_scale_factor(window.scale_factor());
        let config = wgpu::SurfaceConfiguration {
            usage: surface_usage,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: phys.width,
            height: phys.height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let chrome = self
            .frame_windows
            .primary_window()
            .map(|primary| primary.lifecycle.chrome().clone());
        let Some(chrome) = chrome else {
            return false;
        };
        self.frame_windows
            .populate_primary_native(GuiFrameNativeWindowState {
                window,
                surface,
                surface_config: config,
                width: phys.width,
                height: phys.height,
                scale_factor,
                chrome,
            });
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.resize(phys.width, phys.height);
        }
        if let Some(primary) = self.frame_windows.primary_window_mut() {
            primary.render.populate_glyph_atlas(&device, scale_factor);
            primary
                .render
                .cursor
                .apply_config(self.cursor_defaults.config_snapshot());
            primary
                .render
                .compositor
                .transitions
                .apply_policy(self.transition_policy);
        }
        self.frame_windows.mark_top_level_dirty();
        #[cfg(test)]
        self.record_primary_gpu_initialization(PrimaryGpuInitialization::Reuse);
        true
    }

    pub(super) fn create_pending_primary(&mut self, event_loop: &ActiveEventLoop) {
        let Some(primary) = self.frame_windows.primary_window() else {
            return;
        };
        if primary.lifecycle.is_active() {
            return;
        }
        let (width, height) = primary.native_size();
        let chrome = primary.lifecycle.chrome().clone();
        let geometry_hints = primary.lifecycle.geometry_hints();
        let attrs = Window::default_attributes()
            .with_title(&chrome.title)
            .with_inner_size(super::state::window_size_from_emacs_pixels(width, height))
            .with_decorations(chrome.decorations_enabled)
            .with_transparent(true);
        let attrs = crate::window_identity::apply_platform_window_identity(attrs);

        tracing::info!(
            "Render thread creating primary window: emacs_pixels={}x{} title={:?}",
            width,
            height,
            chrome.title
        );
        let Ok(window) = event_loop.create_window(attrs) else {
            tracing::error!("Failed to create primary window");
            return;
        };
        let window = Arc::new(window);
        super::frame_windows::NativeTextInputPolicy::for_gui_frame().apply_to_window(&window);
        self.window_icon.apply(&window);

        if self.clipboard.is_err() {
            self.clipboard =
                crate::clipboard::ClipboardService::for_display(event_loop.owned_display_handle());
            if let Err(error) = &self.clipboard {
                tracing::error!("Failed to initialize clipboard service: {error}");
            }
        }

        let effective_scale = super::state::effective_window_scale_factor(window.scale_factor());
        let phys = window.inner_size();
        if let Some(primary) = self.frame_windows.primary_window_mut() {
            if let FrameLifecycle::Pending {
                width,
                height,
                scale_factor,
                ..
            } = &mut primary.lifecycle
            {
                *width = phys.width;
                *height = phys.height;
                *scale_factor = effective_scale;
            }
        }

        let deferred_recovery = self.lifecycle_flags.device_recovery_deferred;
        let initialization = primary_gpu_initialization_plan(
            self.gpu.is_some(),
            self.renderer.is_some(),
            deferred_recovery,
        );
        match initialization {
            PrimaryGpuInitialization::Initialize | PrimaryGpuInitialization::RebuildAfterLoss => {
                if initialization == PrimaryGpuInitialization::RebuildAfterLoss {
                    self.toolbar.icon_textures.clear();
                    self.renderer = None;
                    self.frame_windows.clear_gpu_resident_state();
                    self.gpu = None;
                }
                if !self.init_wgpu(event_loop, window.clone()) {
                    return;
                }
            }
            PrimaryGpuInitialization::Reuse => {
                if !self.create_primary_surface_with_existing_gpu(window.clone()) {
                    return;
                }
            }
        }
        self.lifecycle_flags.device_recovery_deferred = false;

        if deferred_recovery {
            if let Some(gpu) = self.gpu.as_ref() {
                self.frame_windows.recreate_secondary_native_surfaces(
                    &gpu.instance,
                    &gpu.device,
                    &gpu.adapter,
                );
            }
            self.frame_windows.mark_top_level_dirty();
            self.frame_windows
                .for_each_top_level_window(|window_state| window_state.request_redraw());
            self.comms.send_input(InputEvent::DisplayReset);
        }

        if let Some(geometry_hints) = geometry_hints {
            apply_window_geometry_hints(&window, geometry_hints);
        }
    }

    /// Rebuild the entire GPU stack after the wgpu device was lost (a user
    /// shader hang / TDR, or a simulated loss via
    /// `AssetCommand::DebugSimulateDeviceLoss`).
    ///
    /// Everything renderer-owned (pipelines and the image/video/webkit/
    /// shader-surface caches) dies with the old device and is recreated
    /// empty; each window's committed `current_frame` is CPU data and is
    /// kept, so the next redraw re-renders the same scene (media quads stay
    /// blank for a moment — the evaluator re-resolves media after receiving
    /// `InputEvent::DisplayReset`).
    pub(super) fn recover_from_device_loss(&mut self, event_loop: &ActiveEventLoop) {
        tracing::error!(
            "wgpu device lost — rebuilding GPU state and asking the evaluator to re-resolve media"
        );

        let primary_active = self
            .frame_windows
            .primary_window()
            .is_some_and(|window| window.window().is_some());
        match device_recovery_target(
            primary_active,
            self.frame_windows.has_active_secondary_native_window(),
        ) {
            DeviceRecoveryTarget::Primary => {}
            DeviceRecoveryTarget::SurvivingSecondary => {
                if self
                    .frame_windows
                    .promote_active_secondary_to_primary()
                    .is_none()
                {
                    self.lifecycle_flags.device_recovery_deferred = true;
                    tracing::warn!(
                        "device-loss recovery deferred: surviving secondary could not become primary"
                    );
                    return;
                }
            }
            DeviceRecoveryTarget::Deferred => {
                self.lifecycle_flags.device_recovery_deferred = true;
                tracing::warn!("device-loss recovery deferred until a primary window is realized");
                return;
            }
        }

        // Renderer first: pipelines and every media cache (image, video,
        // webkit, shader surfaces, frame post shader) hold old-device
        // objects.
        self.renderer = None;

        // Per-window GPU-resident compositor state. `current_frame` /
        // `current_row_damage` (CPU glyph data) are deliberately kept.
        self.frame_windows.clear_gpu_resident_state();

        // Toolbar icon ids point into the dropped renderer's image cache;
        // clear them so sync_frame_chrome_assets (run by init_wgpu below)
        // re-uploads the icons into the new renderer.
        self.toolbar.icon_textures.clear();

        // Old instance/adapter/device/queue handles. The old per-window
        // surfaces still hold internal references; they die as they are
        // replaced below.
        self.gpu = None;

        let Some(primary_window) = self
            .frame_windows
            .primary_window()
            .and_then(|window_state| window_state.window())
            .cloned()
        else {
            tracing::error!("device-loss recovery: no active primary window, cannot rebuild wgpu");
            return;
        };

        // New instance/adapter/device/queue + primary surface + renderer;
        // also re-registers the device-lost callback and repopulates the
        // primary glyph atlas (cleared above). populate_primary_native is
        // replace-safe: it re-keys the same winit window id and preserves
        // the Active lifecycle's IME/mouse state.
        self.init_wgpu(event_loop, primary_window);

        if self.gpu.is_none() {
            // The driver may still be resetting. Leave the latch set so the
            // next event-loop pass retries; no explicit wake, so this cannot
            // busy-spin.
            tracing::error!(
                "device-loss recovery: wgpu re-initialization failed; will retry on the next wake"
            );
            self.device_lost.mark_lost_now();
            return;
        }
        self.lifecycle_flags.device_recovery_deferred = false;

        // Secondary top-level windows: their surfaces belong to the dropped
        // instance and can never be configured against the new device;
        // recreate them on the new instance and rebuild their glyph atlases.
        if let Some(gpu) = self.gpu.as_ref() {
            self.frame_windows.recreate_secondary_native_surfaces(
                &gpu.instance,
                &gpu.device,
                &gpu.adapter,
            );
        }

        // Everything re-renders from the kept CPU frames on the next pass.
        self.frame_windows.mark_top_level_dirty();
        self.frame_windows
            .for_each_top_level_window(|window_state| window_state.request_redraw());

        // Tell the evaluator: it clears its media memos (declarative
        // surfaces/videos/webkits re-create on the next redisplay walk),
        // re-sends the frame shader, re-uploads images, and forces a full
        // redisplay.
        self.comms.send_input(InputEvent::DisplayReset);
    }
}

fn build_render_event_loop_impl(
    allow_any_thread: bool,
) -> Result<EventLoop<RenderUserEvent>, String> {
    #[cfg(target_os = "linux")]
    {
        validate_linux_display_before_winit()?;
        tracing::info!(
            "Building winit event loop (allow_any_thread={} wayland_display_present={})",
            allow_any_thread,
            std::env::var("WAYLAND_DISPLAY").is_ok(),
        );
        let mut builder = EventLoop::<RenderUserEvent>::with_user_event();
        // Try Wayland first, fall back to X11.
        if allow_any_thread {
            if std::env::var("WAYLAND_DISPLAY").is_ok() {
                EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
            } else {
                EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
            }
        }
        let event_loop = builder
            .build()
            .map_err(|err| format!("Failed to create event loop: {err}"))?;
        tracing::info!("Built winit event loop");
        Ok(event_loop)
    }

    #[cfg(not(target_os = "linux"))]
    {
        EventLoop::<RenderUserEvent>::with_user_event()
            .build()
            .map_err(|err| format!("Failed to create event loop: {err}"))
    }
}

#[cfg(target_os = "linux")]
fn validate_linux_display_before_winit() -> Result<(), String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return Ok(());
    }
    let Some(display) = std::env::var_os("DISPLAY") else {
        return Ok(());
    };
    if display.is_empty() {
        return Ok(());
    }

    let display_for_error = display.to_string_lossy().into_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::Builder::new()
        .name("x11-display-probe".to_string())
        .spawn(move || {
            let result = x11_display_responds();
            let _ = tx.send(result);
        });

    match rx.recv_timeout(std::time::Duration::from_secs(1)) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!("Cannot open X display {display_for_error}")),
        Err(_) => Err(format!(
            "Cannot open X display {display_for_error}: connection timed out"
        )),
    }
}

#[cfg(target_os = "linux")]
fn x11_display_responds() -> bool {
    let Ok(xlib) = xlib::Xlib::open() else {
        return false;
    };
    let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
    if display.is_null() {
        return false;
    }
    unsafe {
        (xlib.XCloseDisplay)(display);
    }
    true
}

/// Build a render event loop for the current OS thread.
pub fn build_render_event_loop() -> Result<EventLoop<RenderUserEvent>, String> {
    build_render_event_loop_impl(false)
}

/// Build a render event loop for the legacy render-thread helper.
pub(crate) fn build_render_event_loop_any_thread() -> Result<EventLoop<RenderUserEvent>, String> {
    build_render_event_loop_impl(true)
}

/// Run the render loop with an already-created event loop.
pub(crate) fn run_render_loop_with_event_loop(
    event_loop: EventLoop<RenderUserEvent>,
    comms: RenderComms,
    width: u32,
    height: u32,
    title: String,
    image_metadata: SharedImageMetadata,
    shared_monitors: SharedMonitorInfo,
    poll_when_idle: bool,
    startup_mode: RenderStartupMode,
    #[cfg(feature = "neo-term")] shared_terminals: crate::terminal::SharedTerminals,
) -> Result<(), String> {
    tracing::info!("Render thread starting");

    // CRITICAL: Set up a dedicated GMainContext for WebKit before any WebKit initialization.
    // This ensures WebKit attaches its GLib sources (IPC sockets, etc.) to this context,
    // not the default context. Only the render thread will dispatch events from this context,
    // preventing the Emacs main thread's xg_select from dispatching WebKit callbacks.
    #[cfg(all(feature = "wpe-webkit", wpe_platform_available))]
    let _webkit_main_context = unsafe {
        let ctx = plat::g_main_context_new();
        if !ctx.is_null() {
            // Acquire the context so we can dispatch on it
            plat::g_main_context_acquire(ctx);
            // Push as thread-default - WebKit will attach sources here
            plat::g_main_context_push_thread_default(ctx);
            tracing::info!("Created dedicated GMainContext for WebKit: {:?}", ctx);
        } else {
            tracing::warn!("Failed to create dedicated GMainContext for WebKit");
        }
        ctx
    };

    // Start with WaitUntil to avoid busy-polling; about_to_wait() adjusts dynamically
    event_loop.set_control_flow(ControlFlow::WaitUntil(
        std::time::Instant::now() + std::time::Duration::from_millis(16),
    ));

    let mut app = RenderApp::new_with_startup_mode(
        comms,
        width,
        height,
        title,
        image_metadata,
        shared_monitors,
        poll_when_idle,
        startup_mode,
        #[cfg(feature = "neo-term")]
        shared_terminals,
    );

    tracing::info!("Render thread entering winit event loop");
    let result = event_loop.run_app(&mut app);
    if let Err(ref e) = result {
        tracing::error!("Event loop error: {:?}", e);
    }

    // Notify Emacs that the render thread is exiting so it can shut down gracefully.
    // This handles cases like Wayland connection loss (ExitFailure(1)) where the
    // window disappears without an explicit close request.
    tracing::info!("Render thread exiting, sending WindowClose to Emacs");
    app.comms
        .send_input(InputEvent::WindowClose { emacs_frame_id: 0 });

    result.map_err(|err| format!("Event loop error: {err}"))
}

/// Run the render loop on the current OS thread. Product GUI startup uses
/// this path so winit/AppKit/Windows ownership stays on the process main
/// thread; evaluator-to-render traffic must wake it via EventLoopProxy.
pub fn run_render_loop_current_thread(
    event_loop: EventLoop<RenderUserEvent>,
    comms: RenderComms,
    width: u32,
    height: u32,
    title: String,
    image_metadata: SharedImageMetadata,
    shared_monitors: SharedMonitorInfo,
    startup_mode: RenderStartupMode,
) -> Result<(), String> {
    #[cfg(feature = "neo-term")]
    let shared_terminals = crate::terminal::new_shared_terminals();
    #[cfg(feature = "neo-term")]
    return run_render_loop_current_thread_with_terminals(
        event_loop,
        comms,
        width,
        height,
        title,
        image_metadata,
        shared_monitors,
        shared_terminals,
        startup_mode,
    );
    #[cfg(not(feature = "neo-term"))]
    run_render_loop_with_event_loop(
        event_loop,
        comms,
        width,
        height,
        title,
        image_metadata,
        shared_monitors,
        false,
        startup_mode,
        #[cfg(feature = "neo-term")]
        shared_terminals,
    )
}

/// Run the product render loop with a caller-owned terminal registry so the
/// evaluator can synchronously inspect terminal text while the renderer owns
/// PTYs and VT state.
#[cfg(feature = "neo-term")]
pub fn run_render_loop_current_thread_with_terminals(
    event_loop: EventLoop<RenderUserEvent>,
    comms: RenderComms,
    width: u32,
    height: u32,
    title: String,
    image_metadata: SharedImageMetadata,
    shared_monitors: SharedMonitorInfo,
    shared_terminals: crate::terminal::SharedTerminals,
    startup_mode: RenderStartupMode,
) -> Result<(), String> {
    run_render_loop_with_event_loop(
        event_loop,
        comms,
        width,
        height,
        title,
        image_metadata,
        shared_monitors,
        false,
        startup_mode,
        shared_terminals,
    )
}

/// Build the render event loop and run it on the render thread.
pub fn run_render_loop(
    comms: RenderComms,
    width: u32,
    height: u32,
    title: String,
    image_metadata: SharedImageMetadata,
    shared_monitors: SharedMonitorInfo,
    startup_mode: RenderStartupMode,
) -> Result<(), String> {
    #[cfg(feature = "neo-term")]
    let shared_terminals = crate::terminal::new_shared_terminals();
    let event_loop = build_render_event_loop()?;
    run_render_loop_with_event_loop(
        event_loop,
        comms,
        width,
        height,
        title,
        image_metadata,
        shared_monitors,
        false,
        startup_mode,
        #[cfg(feature = "neo-term")]
        shared_terminals,
    )
}
