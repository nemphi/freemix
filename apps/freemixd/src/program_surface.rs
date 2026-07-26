//! Opt-in macOS Program surface and its platform-neutral policies.

use std::{
    cmp::Ordering,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    time::Duration,
};

#[cfg(target_os = "macos")]
use std::{
    future::Future,
    net::SocketAddr,
    path::PathBuf,
    pin::pin,
    task::{Context, Poll, Wake, Waker},
    thread,
    time::Instant,
};

#[cfg(target_os = "macos")]
use fm_color::NativeSdrOutputTransform;
#[cfg(any(target_os = "macos", test))]
use fm_gpu::{FrameDecision, FrameGeneration, PresentationFrame};
#[cfg(target_os = "macos")]
use fm_gpu::{
    NativeBackend, NativeContext, NativeGpuError, NativeSurface, NativeSurfaceAcquire,
    NativeSurfaceFactory, NativeTexture,
};
use fm_gpu::{
    PresentationAction, PresentationLifecycle, PresentationState, ResizeGeneration,
    SurfaceAcquisition,
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct MonitorDescriptor {
    x: i32,
    y: i32,
    name: String,
    width: u32,
    height: u32,
    refresh_millihertz: u32,
    scale_bits: u64,
}

impl Ord for MonitorDescriptor {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.x,
            self.y,
            &self.name,
            self.width,
            self.height,
            self.refresh_millihertz,
            self.scale_bits,
        )
            .cmp(&(
                other.x,
                other.y,
                &other.name,
                other.width,
                other.height,
                other.refresh_millihertz,
                other.scale_bits,
            ))
    }
}

impl PartialOrd for MonitorDescriptor {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MonitorChange {
    Unchanged,
    Disconnected,
    Reconnected(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MonitorSelectionError {
    Unavailable,
    AmbiguousDescriptors,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MonitorEntry<I> {
    descriptor: MonitorDescriptor,
    identity: I,
}

fn sort_monitor_entries<I>(inventory: &mut [MonitorEntry<I>]) {
    inventory.sort_by(|left, right| left.descriptor.cmp(&right.descriptor));
}

struct MonitorPolicy<I> {
    selected: I,
    connected: bool,
}

impl<I: Clone + Eq> MonitorPolicy<I> {
    fn select(
        inventory: &mut [MonitorEntry<I>],
        index: usize,
    ) -> Result<(Self, usize), MonitorSelectionError> {
        sort_monitor_entries(inventory);
        if inventory
            .windows(2)
            .any(|pair| pair[0].descriptor == pair[1].descriptor)
        {
            return Err(MonitorSelectionError::AmbiguousDescriptors);
        }
        let selected = inventory
            .get(index)
            .ok_or(MonitorSelectionError::Unavailable)?;
        Ok((
            Self {
                selected: selected.identity.clone(),
                connected: true,
            },
            index,
        ))
    }

    fn update(&mut self, inventory: &mut [MonitorEntry<I>]) -> MonitorChange {
        sort_monitor_entries(inventory);
        let selected = inventory
            .iter()
            .position(|monitor| monitor.identity == self.selected);
        match (self.connected, selected) {
            (true, None) => {
                self.connected = false;
                MonitorChange::Disconnected
            }
            (false, Some(index)) => {
                self.connected = true;
                MonitorChange::Reconnected(index)
            }
            _ => MonitorChange::Unchanged,
        }
    }

    fn selected_index(&self, inventory: &[MonitorEntry<I>]) -> Option<usize> {
        inventory
            .iter()
            .position(|monitor| monitor.identity == self.selected)
    }

    fn accepts_resize(&self, current: Option<&I>) -> bool {
        self.connected && current == Some(&self.selected)
    }
}

#[derive(Clone)]
pub(super) struct ShutdownSignal(Arc<AtomicBool>);

impl ShutdownSignal {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    fn request(&self) -> bool {
        !self.0.swap(true, AtomicOrdering::AcqRel)
    }

    pub(super) fn requested(&self) -> bool {
        self.0.load(AtomicOrdering::Acquire)
    }
}

fn begin_shutdown<T>(signal: &ShutdownSignal, deadline: &mut Option<T>, value: T) -> bool {
    if !signal.request() {
        return false;
    }
    *deadline = Some(value);
    true
}

fn program_shutdown_timeout(recorder: bool) -> Duration {
    if recorder {
        super::PROGRAM_RECORDER_STOP_TIMEOUT
            + super::PROGRAM_RECORDER_KILL_TIMEOUT
            + super::PROGRAM_CHECKPOINT_MARGIN
    } else {
        Duration::from_secs(5)
    }
}

const fn accepts_escape_press(pressed: bool, repeat: bool, escape: bool) -> bool {
    pressed && !repeat && escape
}

struct CoalescingSender<T> {
    slot: Arc<Mutex<Option<T>>>,
    wake: SyncSender<()>,
}

pub(super) struct CoalescingReceiver<T> {
    slot: Arc<Mutex<Option<T>>>,
    wake: Receiver<()>,
}

fn coalescing_channel<T>() -> (CoalescingSender<T>, CoalescingReceiver<T>) {
    let slot = Arc::new(Mutex::new(None));
    let (wake, receiver) = mpsc::sync_channel(1);
    (
        CoalescingSender {
            slot: Arc::clone(&slot),
            wake,
        },
        CoalescingReceiver {
            slot,
            wake: receiver,
        },
    )
}

impl<T> CoalescingSender<T> {
    fn send(&self, value: T) -> Result<(), ()> {
        *self.slot.lock().expect("coalescing slot poisoned") = Some(value);
        match self.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => Err(()),
        }
    }
}

#[derive(Debug)]
struct Correlated<T> {
    generation: ResizeGeneration,
    value: T,
}

struct RecreationCoordinator<T> {
    pending: Option<ResizeGeneration>,
    ready: Option<Correlated<T>>,
}

impl<T> RecreationCoordinator<T> {
    const fn new() -> Self {
        Self {
            pending: None,
            ready: None,
        }
    }

    fn request(&mut self, generation: ResizeGeneration) {
        if self.pending.is_none_or(|pending| generation > pending) {
            self.pending = Some(generation);
        }
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| Some(ready.generation) != self.pending)
        {
            self.ready = None;
        }
    }

    fn pending(&self) -> Option<ResizeGeneration> {
        self.pending
    }

    fn prepare(&mut self, create: impl FnOnce(ResizeGeneration) -> T) {
        if self.ready.is_none()
            && let Some(generation) = self.pending
        {
            self.ready = Some(Correlated {
                generation,
                value: create(generation),
            });
        }
    }

    fn take_ready(&mut self) -> Option<Correlated<T>> {
        self.ready.take()
    }

    fn pressure(&mut self, response: Correlated<T>) {
        if self.pending == Some(response.generation) {
            self.ready = Some(response);
        }
    }

    fn sent(&mut self, generation: ResizeGeneration) {
        if self.pending == Some(generation) {
            self.pending = None;
        }
    }
}

impl<T> CoalescingReceiver<T> {
    pub(super) fn take_latest(&self) -> Option<T> {
        match self.wake.try_recv() {
            Ok(()) => self.slot.lock().expect("coalescing slot poisoned").take(),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) struct ProgramWorkerChannels {
    sizes: CoalescingReceiver<(u32, u32)>,
    replacements: Receiver<Correlated<Result<NativeSurface, String>>>,
    recreate: CoalescingSender<ResizeGeneration>,
    wake: winit::event_loop::EventLoopProxy<UserEvent>,
    shutdown: ShutdownSignal,
}

#[cfg(target_os = "macos")]
impl ProgramWorkerChannels {
    fn request_recreation(&self, generation: ResizeGeneration) -> Result<(), String> {
        self.recreate
            .send(generation)
            .map_err(|()| "Program surface recreation channel is unavailable".to_owned())?;
        self.wake
            .send_event(UserEvent::Wake)
            .map_err(|_| "Program surface event loop is unavailable".to_owned())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplacementDisposition {
    Install,
    Discard,
    DiscardAndRetry(ResizeGeneration),
}

fn replacement_disposition(
    state: PresentationState,
    response: ResizeGeneration,
) -> ReplacementDisposition {
    match state {
        PresentationState::Recreate {
            resize_generation, ..
        } if resize_generation == response => ReplacementDisposition::Install,
        PresentationState::Recreate {
            resize_generation, ..
        } if resize_generation > response => {
            ReplacementDisposition::DiscardAndRetry(resize_generation)
        }
        _ => ReplacementDisposition::Discard,
    }
}

#[cfg(target_os = "macos")]
pub(super) struct ProgramPresentation {
    surface: Option<NativeSurface>,
    transform: Option<NativeSdrOutputTransform>,
    lifecycle: PresentationLifecycle,
    frame_generation: u64,
    channels: ProgramWorkerChannels,
}

#[cfg(target_os = "macos")]
impl ProgramPresentation {
    pub(super) fn new(surface: NativeSurface, channels: ProgramWorkerChannels) -> Self {
        Self {
            surface: Some(surface),
            transform: None,
            lifecycle: PresentationLifecycle::new(),
            frame_generation: 0,
            channels,
        }
    }

    pub(super) fn shutdown_requested(&self) -> bool {
        self.channels.shutdown.requested()
    }

    pub(super) fn service_control(&mut self, context: &NativeContext) -> Result<(), String> {
        if let Some((width, height)) = self.channels.sizes.take_latest() {
            let action = self.lifecycle.resize(width, height);
            self.apply_configuration_action(context, action)?;
        }
        match self.channels.replacements.try_recv() {
            Ok(response) => {
                match replacement_disposition(self.lifecycle.state(), response.generation) {
                    ReplacementDisposition::Install => match response.value {
                        Ok(surface) => {
                            self.surface = Some(surface);
                            let action =
                                self.lifecycle.finish_recreation(response.generation, true);
                            self.apply_configuration_action(context, action)?;
                        }
                        Err(error) => {
                            let _ = self.lifecycle.finish_recreation(response.generation, false);
                            return Err(error);
                        }
                    },
                    ReplacementDisposition::Discard => {}
                    ReplacementDisposition::DiscardAndRetry(generation) => {
                        self.channels.request_recreation(generation)?;
                    }
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                return Err("Program surface replacement channel disconnected".to_owned());
            }
        }
        Ok(())
    }

    pub(super) fn present_latest(
        &mut self,
        context: &NativeContext,
        texture: &NativeTexture,
    ) -> Result<(), String> {
        self.service_control(context)?;
        let Some(resize_generation) = self.lifecycle.resize_generation() else {
            return Ok(());
        };
        self.frame_generation = self
            .frame_generation
            .checked_add(1)
            .ok_or_else(|| "Program frame generation exhausted".to_owned())?;
        let frame = PresentationFrame::new(
            FrameGeneration::new(self.frame_generation),
            resize_generation,
        );
        if matches!(
            self.lifecycle.submit_frame(frame),
            FrameDecision::Rejected(_)
        ) {
            return Ok(());
        }

        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        let follow_up =
            match block_on(surface.acquire(context)).map_err(|error| surface_error(&error))? {
                NativeSurfaceAcquire::Success(surface_frame) => {
                    let action = self
                        .lifecycle
                        .handle_acquisition(SurfaceAcquisition::Success);
                    submit_acquired(
                        context,
                        &mut self.lifecycle,
                        self.transform.as_ref(),
                        texture,
                        surface_frame,
                        action,
                    )?;
                    None
                }
                NativeSurfaceAcquire::Suboptimal(surface_frame) => {
                    let action = self
                        .lifecycle
                        .handle_acquisition(SurfaceAcquisition::Suboptimal);
                    submit_acquired(
                        context,
                        &mut self.lifecycle,
                        self.transform.as_ref(),
                        texture,
                        surface_frame,
                        action,
                    )?;
                    Some(action)
                }
                NativeSurfaceAcquire::Timeout | NativeSurfaceAcquire::Occluded => {
                    let _ = self
                        .lifecycle
                        .handle_acquisition(SurfaceAcquisition::Timeout);
                    None
                }
                NativeSurfaceAcquire::Outdated => {
                    let action = self
                        .lifecycle
                        .handle_acquisition(SurfaceAcquisition::Outdated);
                    Some(action)
                }
                NativeSurfaceAcquire::Lost => {
                    let action = self.lifecycle.handle_acquisition(SurfaceAcquisition::Lost);
                    Some(action)
                }
                NativeSurfaceAcquire::Validation(detail) => {
                    return Err(surface_validation_error(&detail));
                }
            };
        if let Some(action) = follow_up {
            self.apply_configuration_action(context, action)?;
        }
        Ok(())
    }

    pub(super) fn telemetry(&self) -> fm_gpu::PresentationTelemetry {
        self.lifecycle.telemetry()
    }

    pub(super) fn telemetry_for_shutdown(&mut self) -> fm_gpu::PresentationTelemetry {
        let _ = self.lifecycle.resize(0, 0);
        self.lifecycle.telemetry()
    }

    fn apply_configuration_action(
        &mut self,
        context: &NativeContext,
        action: PresentationAction,
    ) -> Result<(), String> {
        let (extent, generation) = match action {
            PresentationAction::Configure {
                extent,
                resize_generation,
            }
            | PresentationAction::Reconfigure {
                extent,
                resize_generation,
            }
            | PresentationAction::PresentAndReconfigure {
                extent,
                resize_generation,
                ..
            } => (extent, resize_generation),
            PresentationAction::Recreate {
                resize_generation, ..
            } => {
                self.surface.take();
                self.transform.take();
                return self.channels.request_recreation(resize_generation);
            }
            PresentationAction::Fail(reason) => {
                return Err(format!("Program presentation failed: {reason:?}"));
            }
            _ => return Ok(()),
        };
        let surface = self
            .surface
            .as_mut()
            .ok_or_else(|| "Program surface is unavailable during configuration".to_owned())?;
        let configuration = surface
            .select_opaque_sdr_configuration(extent.width().get(), extent.height().get())
            .map_err(|error| surface_error(&error))?;
        if self.transform.is_none() {
            self.transform = Some(
                block_on(NativeSdrOutputTransform::new_for_format(
                    context,
                    configuration.format,
                ))
                .map_err(|error| format!("Program output transform failed: {error}"))?,
            );
        }
        match block_on(surface.configure(context, configuration)) {
            Ok(()) => {
                let _ = self.lifecycle.finish_configuration(generation, true);
                Ok(())
            }
            Err(error) => {
                let result = self.lifecycle.finish_configuration(generation, false);
                Err(format!(
                    "Program surface configuration failed ({result:?}): {error}"
                ))
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn submit_acquired(
    context: &NativeContext,
    lifecycle: &mut PresentationLifecycle,
    transform: Option<&NativeSdrOutputTransform>,
    texture: &NativeTexture,
    surface_frame: fm_gpu::NativeSurfaceFrame<'_>,
    action: PresentationAction,
) -> Result<(), String> {
    let (PresentationAction::Present(presented)
    | PresentationAction::PresentAndReconfigure {
        frame: presented, ..
    }) = action
    else {
        return Ok(());
    };
    let transform =
        transform.ok_or_else(|| "Program output transform is not configured".to_owned())?;
    let submitted = match block_on(transform.transform_to_surface(context, texture, surface_frame))
    {
        Ok(submitted) => submitted,
        Err(error) => {
            let _ = lifecycle.finish_present(presented, false);
            return Err(surface_error(&error));
        }
    };
    if let Err(error) = block_on(context.present(submitted)) {
        let _ = lifecycle.finish_present(presented, false);
        return Err(surface_error(&error));
    }
    let _ = lifecycle.finish_present(presented, true);
    Ok(())
}

#[cfg(target_os = "macos")]
fn surface_error(error: &NativeGpuError) -> String {
    format!("Program surface GPU failure: {error}")
}

fn surface_validation_error(detail: &str) -> String {
    format!("fatal Program surface validation failure: {detail}")
}

#[cfg(target_os = "macos")]
fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
enum UserEvent {
    Wake,
    Finished(Result<(), String>),
}

#[cfg(target_os = "macos")]
struct ProgramApp {
    project: PathBuf,
    listen: SocketAddr,
    once: bool,
    display_index: usize,
    record_program: Option<PathBuf>,
    camera_helper: Option<PathBuf>,
    window: Option<Arc<winit::window::Window>>,
    factory: Option<NativeSurfaceFactory>,
    monitor_policy: Option<MonitorPolicy<winit::monitor::MonitorHandle>>,
    size_sender: CoalescingSender<(u32, u32)>,
    size_receiver: Option<CoalescingReceiver<(u32, u32)>>,
    replacement_sender: SyncSender<Correlated<Result<NativeSurface, String>>>,
    replacement_receiver: Option<Receiver<Correlated<Result<NativeSurface, String>>>>,
    recreate_sender: Option<CoalescingSender<ResizeGeneration>>,
    recreate_receiver: CoalescingReceiver<ResizeGeneration>,
    shutdown: ShutdownSignal,
    shutdown_timeout: Duration,
    recorder_requested: bool,
    shutdown_deadline: Option<Instant>,
    worker: Option<thread::JoinHandle<()>>,
    result: Option<Result<(), String>>,
    recreations: RecreationCoordinator<Result<NativeSurface, String>>,
    shutdown_timed_out: bool,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
}

#[cfg(target_os = "macos")]
impl ProgramApp {
    fn new(
        project: PathBuf,
        listen: SocketAddr,
        once: bool,
        display_index: usize,
        record_program: Option<PathBuf>,
        camera_helper: Option<PathBuf>,
        proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    ) -> Self {
        let (size_sender, size_receiver) = coalescing_channel();
        let (replacement_sender, replacement_receiver) = mpsc::sync_channel(1);
        let (recreate_sender, recreate_receiver) = coalescing_channel();
        let recorder_requested = record_program.is_some();
        let shutdown_timeout = program_shutdown_timeout(recorder_requested);
        Self {
            project,
            listen,
            once,
            display_index,
            record_program,
            camera_helper,
            window: None,
            factory: None,
            monitor_policy: None,
            size_sender,
            size_receiver: Some(size_receiver),
            replacement_sender,
            replacement_receiver: Some(replacement_receiver),
            recreate_sender: Some(recreate_sender),
            recreate_receiver,
            shutdown: ShutdownSignal::new(),
            shutdown_timeout,
            recorder_requested,
            shutdown_deadline: None,
            worker: None,
            result: None,
            recreations: RecreationCoordinator::new(),
            shutdown_timed_out: false,
            proxy,
        }
    }

    fn fail(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, error: impl Into<String>) {
        self.result = Some(Err(error.into()));
        let _ = self.shutdown.request();
        event_loop.exit();
    }

    fn request_shutdown(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if !begin_shutdown(
            &self.shutdown,
            &mut self.shutdown_deadline,
            Instant::now() + self.shutdown_timeout,
        ) {
            return;
        }
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(20),
        ));
    }

    fn poll_monitors(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let mut monitors = ordered_monitors(event_loop);
        let Some(policy) = self.monitor_policy.as_mut() else {
            return;
        };
        match policy.update(&mut monitors) {
            MonitorChange::Disconnected => self.suspend_output(),
            MonitorChange::Reconnected(_) | MonitorChange::Unchanged => {
                self.synchronize_window_target(&monitors);
            }
        }
        self.recreate_if_pending();
    }

    fn synchronize_window_target(&self, monitors: &[MonitorEntry<winit::monitor::MonitorHandle>]) {
        let (Some(window), Some(policy)) = (&self.window, &self.monitor_policy) else {
            return;
        };
        let Some(index) = policy.selected_index(monitors) else {
            self.suspend_output();
            return;
        };
        if !policy.accepts_resize(window.current_monitor().as_ref()) {
            window.set_visible(false);
            let _ = self.size_sender.send((0, 0));
            window.set_fullscreen(Some(winit::window::Fullscreen::Borderless(Some(
                monitors[index].identity.clone(),
            ))));
            return;
        }
        window.set_visible(true);
        let size = window.inner_size();
        let _ = self.size_sender.send((size.width, size.height));
    }

    fn suspend_output(&self) {
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        let _ = self.size_sender.send((0, 0));
    }

    fn handle_resize(&self, size: winit::dpi::PhysicalSize<u32>) {
        let (Some(window), Some(policy)) = (&self.window, &self.monitor_policy) else {
            return;
        };
        if policy.accepts_resize(window.current_monitor().as_ref()) {
            window.set_visible(true);
            let _ = self.size_sender.send((size.width, size.height));
        } else {
            self.suspend_output();
        }
    }

    fn receive_recreation_request(&mut self) {
        if let Some(generation) = self.recreate_receiver.take_latest() {
            self.recreations.request(generation);
        }
    }

    fn recreate_if_pending(&mut self) {
        let (Some(factory), Some(window), Some(policy)) =
            (&self.factory, &self.window, &self.monitor_policy)
        else {
            return;
        };
        if !policy.connected || self.recreations.pending().is_none() {
            return;
        }
        self.recreations.prepare(|_| {
            factory
                .create_surface(Arc::clone(window))
                .map_err(|error| format!("Program surface recreation failed: {error}"))
        });
        let Some(response) = self.recreations.take_ready() else {
            return;
        };
        let generation = response.generation;
        match self.replacement_sender.try_send(response) {
            Ok(()) => self.recreations.sent(generation),
            Err(TrySendError::Full(response)) => self.recreations.pressure(response),
            Err(TrySendError::Disconnected(_)) => {
                self.result = Some(Err("Program surface worker disconnected".to_owned()));
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl winit::application::ApplicationHandler<UserEvent> for ProgramApp {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let mut monitors = ordered_monitors(event_loop);
        let (policy, selected_index) =
            match MonitorPolicy::select(&mut monitors, self.display_index) {
                Ok(selected) => selected,
                Err(MonitorSelectionError::Unavailable) => {
                    self.fail(
                        event_loop,
                        format!(
                            "fullscreen display index {} is unavailable; {} display(s) detected",
                            self.display_index,
                            monitors.len()
                        ),
                    );
                    return;
                }
                Err(MonitorSelectionError::AmbiguousDescriptors) => {
                    self.fail(
                    event_loop,
                    "fullscreen display inventory has indistinguishable deterministic descriptors",
                );
                    return;
                }
            };
        let monitor = monitors[selected_index].identity.clone();
        let attributes = winit::window::Window::default_attributes()
            .with_title("FreeMix Program")
            .with_decorations(false)
            .with_visible(false)
            .with_fullscreen(Some(winit::window::Fullscreen::Borderless(Some(
                monitor.clone(),
            ))));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(error) => {
                self.fail(
                    event_loop,
                    format!("Program window creation failed: {error}"),
                );
                return;
            }
        };
        let (context, surface) = match block_on(NativeContext::new_with_surface(
            [NativeBackend::Metal],
            Arc::clone(&window),
        )) {
            Ok(pair) => pair,
            Err(error) => {
                self.fail(event_loop, format!("Program GPU setup failed: {error}"));
                return;
            }
        };
        let factory = context.surface_factory();
        self.monitor_policy = Some(policy);
        self.factory = Some(factory);
        self.window = Some(window);
        self.synchronize_window_target(&monitors);

        let channels = ProgramWorkerChannels {
            sizes: self.size_receiver.take().expect("worker starts once"),
            replacements: self
                .replacement_receiver
                .take()
                .expect("worker starts once"),
            recreate: self.recreate_sender.take().expect("worker starts once"),
            wake: self.proxy.clone(),
            shutdown: self.shutdown.clone(),
        };
        let project = self.project.clone();
        let listen = self.listen;
        let once = self.once;
        let record_program = self.record_program.clone();
        let camera_helper = self.camera_helper.clone();
        let proxy = self.proxy.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("freemixd-program-worker".to_owned())
                .spawn(move || {
                    let result = super::serve_program_worker(
                        &project,
                        listen,
                        once,
                        context,
                        surface,
                        channels,
                        camera_helper,
                        record_program,
                    )
                    .map_err(|error| error.to_string());
                    let _ = proxy.send_event(UserEvent::Finished(result));
                })
                .expect("Program worker thread creation failed"),
        );
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(500),
        ));
    }

    fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => {
                self.receive_recreation_request();
                self.poll_monitors(event_loop);
            }
            UserEvent::Finished(result) => {
                self.result = Some(result);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        if self
            .window
            .as_ref()
            .is_none_or(|window| window.id() != window_id)
        {
            return;
        }
        match event {
            winit::event::WindowEvent::CloseRequested => self.request_shutdown(event_loop),
            winit::event::WindowEvent::KeyboardInput { event, .. }
                if accepts_escape_press(
                    event.state == winit::event::ElementState::Pressed,
                    event.repeat,
                    event.logical_key
                        == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Escape),
                ) =>
            {
                self.request_shutdown(event_loop);
            }
            winit::event::WindowEvent::Resized(size) => {
                self.poll_monitors(event_loop);
                self.handle_resize(size);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.shutdown.requested() {
            if self
                .shutdown_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.shutdown_timed_out = true;
                self.result = Some(Err(if self.recorder_requested {
                    format!(
                        "forced Program shutdown after {} seconds; checkpoint and recorder cleanup completion were not confirmed",
                        self.shutdown_timeout.as_secs()
                    )
                } else {
                    "forced Program shutdown after 5 seconds; checkpoint completion was not confirmed"
                        .to_owned()
                }));
                event_loop.exit();
                return;
            }
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(20),
            ));
            return;
        }
        self.receive_recreation_request();
        self.poll_monitors(event_loop);
        if self.result.is_some() {
            event_loop.exit();
        } else {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(500),
            ));
        }
    }
}

#[cfg(target_os = "macos")]
fn ordered_monitors(
    event_loop: &winit::event_loop::ActiveEventLoop,
) -> Vec<MonitorEntry<winit::monitor::MonitorHandle>> {
    let mut monitors = event_loop
        .available_monitors()
        .map(|monitor| {
            let position = monitor.position();
            let size = monitor.size();
            let descriptor = MonitorDescriptor {
                x: position.x,
                y: position.y,
                name: monitor.name().unwrap_or_default(),
                width: size.width,
                height: size.height,
                refresh_millihertz: monitor.refresh_rate_millihertz().unwrap_or_default(),
                scale_bits: monitor.scale_factor().to_bits(),
            };
            MonitorEntry {
                descriptor,
                identity: monitor,
            }
        })
        .collect::<Vec<_>>();
    sort_monitor_entries(&mut monitors);
    monitors
}

#[cfg(target_os = "macos")]
pub(super) fn run(
    project: PathBuf,
    listen: SocketAddr,
    once: bool,
    display_index: usize,
    camera_helper: Option<PathBuf>,
    record_program: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = winit::event_loop::EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = ProgramApp::new(
        project,
        listen,
        once,
        display_index,
        record_program,
        camera_helper,
        proxy,
    );
    event_loop.run_app(&mut app)?;
    if !app.shutdown_timed_out
        && let Some(worker) = app.worker.take()
    {
        worker
            .join()
            .map_err(|_| "Program worker panicked".to_owned())?;
    }
    app.result
        .unwrap_or_else(|| Err("Program event loop exited without a worker result".to_owned()))
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: u32, x: i32, name: &str) -> MonitorEntry<u32> {
        MonitorEntry {
            descriptor: MonitorDescriptor {
                x,
                y: 0,
                name: name.to_owned(),
                width: 1920,
                height: 1080,
                refresh_millihertz: 60_000,
                scale_bits: 1.0_f64.to_bits(),
            },
            identity: id,
        }
    }

    #[test]
    fn monitor_inventory_uses_position_then_stable_descriptive_fields() {
        let mut inventory = [
            monitor(3, 0, "B"),
            monitor(2, -1920, "C"),
            monitor(1, 0, "A"),
        ];
        sort_monitor_entries(&mut inventory);
        assert_eq!(
            inventory
                .iter()
                .map(|item| item.identity)
                .collect::<Vec<_>>(),
            vec![2, 1, 3]
        );
    }

    #[test]
    fn ambiguous_descriptors_are_rejected_instead_of_using_identity_as_a_sort_key() {
        let mut inventory = [monitor(1, 0, "same"), monitor(2, 0, "same")];
        assert!(matches!(
            MonitorPolicy::select(&mut inventory, 0),
            Err(MonitorSelectionError::AmbiguousDescriptors)
        ));
    }

    #[test]
    fn selected_display_disconnect_does_not_jump_and_reconnects_by_identity() {
        let mut initial = vec![monitor(10, 0, "A"), monitor(20, 1920, "B")];
        let (mut policy, _) = MonitorPolicy::select(&mut initial, 1).unwrap();
        let mut disconnected = vec![monitor(10, 0, "A"), monitor(30, 1920, "C")];
        assert_eq!(
            policy.update(&mut disconnected),
            MonitorChange::Disconnected
        );
        assert_eq!(policy.update(&mut disconnected), MonitorChange::Unchanged);
        let mut reconnected = vec![monitor(20, -1920, "B"), monitor(10, 0, "A")];
        assert_eq!(
            policy.update(&mut reconnected),
            MonitorChange::Reconnected(0)
        );
    }

    #[test]
    fn coalescing_channel_keeps_only_latest_value() {
        let (sender, receiver) = coalescing_channel();
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        sender.send(3).unwrap();
        assert_eq!(receiver.take_latest(), Some(3));
        assert_eq!(receiver.take_latest(), None);
    }

    #[test]
    fn recreation_requests_coalesce_to_latest_resize_generation() {
        let (sender, receiver) = coalescing_channel();
        sender.send(ResizeGeneration::new(7)).unwrap();
        sender.send(ResizeGeneration::new(8)).unwrap();
        assert_eq!(receiver.take_latest(), Some(ResizeGeneration::new(8)));
        assert_eq!(receiver.take_latest(), None);
    }

    #[test]
    fn shutdown_signal_is_shared_and_idempotent() {
        let shutdown = ShutdownSignal::new();
        let worker = shutdown.clone();
        assert!(!worker.requested());
        assert!(shutdown.request());
        assert!(!shutdown.request());
        assert!(worker.requested());
    }

    #[test]
    fn shutdown_deadline_is_set_once_and_repeated_escape_is_ignored() {
        let shutdown = ShutdownSignal::new();
        let mut deadline = None;
        assert!(begin_shutdown(&shutdown, &mut deadline, 100));
        assert!(!begin_shutdown(&shutdown, &mut deadline, 200));
        assert_eq!(deadline, Some(100));
        assert!(accepts_escape_press(true, false, true));
        assert!(!accepts_escape_press(true, true, true));
    }

    #[test]
    fn recorder_shutdown_deadline_covers_stop_kill_and_checkpoint_margin() {
        assert_eq!(program_shutdown_timeout(false), Duration::from_secs(5));
        assert_eq!(
            program_shutdown_timeout(true),
            super::super::PROGRAM_RECORDER_STOP_TIMEOUT
                + super::super::PROGRAM_RECORDER_KILL_TIMEOUT
                + super::super::PROGRAM_CHECKPOINT_MARGIN
        );
        assert!(program_shutdown_timeout(true) > program_shutdown_timeout(false));
    }

    #[test]
    fn resize_is_rejected_when_selected_identity_is_absent_or_window_is_rehomed() {
        let mut inventory = vec![monitor(10, 0, "A"), monitor(20, 1920, "B")];
        let (mut policy, _) = MonitorPolicy::select(&mut inventory, 1).unwrap();
        assert!(policy.accepts_resize(Some(&20)));
        assert!(!policy.accepts_resize(Some(&10)));
        assert!(!policy.accepts_resize(None));
        let mut disconnected = vec![monitor(10, 0, "A")];
        assert_eq!(
            policy.update(&mut disconnected),
            MonitorChange::Disconnected
        );
        assert!(!policy.accepts_resize(Some(&20)));
    }

    #[test]
    fn stale_recreation_response_is_discarded_and_new_generation_retried() {
        let mut lifecycle = PresentationLifecycle::new();
        let PresentationAction::Configure {
            resize_generation: first,
            ..
        } = lifecycle.resize(1920, 1080)
        else {
            unreachable!()
        };
        lifecycle.finish_configuration(first, true);
        assert!(matches!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Lost),
            PresentationAction::Recreate { .. }
        ));
        let PresentationAction::Recreate {
            resize_generation: second,
            ..
        } = lifecycle.resize(1280, 720)
        else {
            unreachable!()
        };
        assert!(second > first);
        assert_eq!(
            replacement_disposition(lifecycle.state(), first),
            ReplacementDisposition::DiscardAndRetry(second)
        );
        assert_eq!(
            replacement_disposition(lifecycle.state(), second),
            ReplacementDisposition::Install
        );
    }

    #[test]
    fn response_pressure_preserves_and_retries_newer_generation() {
        let first = ResizeGeneration::new(1);
        let second = ResizeGeneration::new(2);
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut coordinator = RecreationCoordinator::new();

        coordinator.request(first);
        coordinator.prepare(ResizeGeneration::get);
        let response = coordinator.take_ready().unwrap();
        sender.try_send(response).unwrap();
        coordinator.sent(first);

        coordinator.request(second);
        coordinator.prepare(ResizeGeneration::get);
        let response = coordinator.take_ready().unwrap();
        let TrySendError::Full(response) = sender.try_send(response).unwrap_err() else {
            unreachable!()
        };
        coordinator.pressure(response);
        assert_eq!(coordinator.pending(), Some(second));

        assert_eq!(receiver.try_recv().unwrap().generation, first);
        let response = coordinator.take_ready().unwrap();
        assert_eq!(response.generation, second);
        sender.try_send(response).unwrap();
        coordinator.sent(second);
        assert_eq!(coordinator.pending(), None);
        assert_eq!(receiver.try_recv().unwrap().generation, second);
    }

    #[test]
    fn surface_validation_failure_does_not_record_out_of_memory() {
        let lifecycle = PresentationLifecycle::new();
        let before = lifecycle.telemetry().out_of_memory_failures;
        assert_eq!(
            surface_validation_error("injected"),
            "fatal Program surface validation failure: injected"
        );
        assert_eq!(lifecycle.telemetry().out_of_memory_failures, before);
    }

    #[test]
    fn acquisition_policy_classifies_retry_reconfigure_recreate_and_fatal() {
        let mut lifecycle = PresentationLifecycle::new();
        let action = lifecycle.resize(1920, 1080);
        let PresentationAction::Configure {
            resize_generation, ..
        } = action
        else {
            panic!("expected configure");
        };
        assert_eq!(
            lifecycle.finish_configuration(resize_generation, true),
            PresentationAction::None
        );
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Timeout),
            PresentationAction::Retry
        );
        assert!(matches!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Outdated),
            PresentationAction::Reconfigure { .. }
        ));
        assert_eq!(lifecycle.telemetry().timeouts, 1);
        assert_eq!(lifecycle.telemetry().outdated_acquisitions, 1);

        let mut lost = PresentationLifecycle::new();
        let PresentationAction::Configure {
            resize_generation, ..
        } = lost.resize(1, 1)
        else {
            unreachable!()
        };
        lost.finish_configuration(resize_generation, true);
        assert!(matches!(
            lost.handle_acquisition(SurfaceAcquisition::Lost),
            PresentationAction::Recreate { .. }
        ));
        assert_eq!(lost.telemetry().surface_losses, 1);
    }

    #[test]
    fn lifecycle_replaces_latest_frame_and_acknowledges_the_presented_generation() {
        let mut lifecycle = PresentationLifecycle::new();
        let PresentationAction::Configure {
            resize_generation, ..
        } = lifecycle.resize(1920, 1080)
        else {
            unreachable!()
        };
        lifecycle.finish_configuration(resize_generation, true);
        let first = PresentationFrame::new(FrameGeneration::new(1), resize_generation);
        let latest = PresentationFrame::new(FrameGeneration::new(2), resize_generation);
        assert_eq!(lifecycle.submit_frame(first), FrameDecision::Queued);
        assert_eq!(
            lifecycle.submit_frame(latest),
            FrameDecision::Replaced { dropped: first }
        );
        assert_eq!(
            lifecycle.handle_acquisition(SurfaceAcquisition::Success),
            PresentationAction::Present(latest)
        );
        assert_eq!(
            lifecycle.finish_present(latest, true),
            PresentationAction::None
        );
        let telemetry = lifecycle.telemetry();
        assert_eq!(telemetry.frames_replaced, 1);
        assert_eq!(telemetry.frames_dropped, 1);
        assert_eq!(telemetry.frames_presented, 1);
    }
}
