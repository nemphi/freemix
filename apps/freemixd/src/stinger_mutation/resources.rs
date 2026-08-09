use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
};

use fm_codec_ffmpeg::{Adapter, Config as FfmpegConfig, ToolAvailability};
use fm_control::ControlService;
use fm_protocol::{RuntimeEventMessage, ServerIdentity};
use freemixd::native_media::{
    NativeAudioLimits, NativeMediaRuntime, NativeProjectLimits, NativeProjectPlan,
    NativeResolvedSource, NativeSourcePlayback, NativeStingerAudioRuntime,
};

use super::super::{
    AppFailure, AppResult, NATIVE_IO_POLL_INTERVAL, NativeDaemon, Policy, ProcessShutdown,
    native_clock_domain, preflight_native_stinger_video, requested_daemon_shutdown,
    validate_native_stinger_sources,
};
use fm_persistence::StoredProject;

const RETIREMENT_WORKERS: usize = 2;
const RETIREMENT_QUEUE: usize = 2;
const RETIREMENT_LIMIT: usize = RETIREMENT_WORKERS + RETIREMENT_QUEUE;

pub(crate) struct NativeStingerMutation {
    pub(crate) project_plan: NativeProjectPlan,
    pub(crate) stingers: NativeSourcePlayback,
    pub(crate) audio: NativeStingerAudioRuntime,
    pub(crate) ordinary_video_limit: u64,
}

struct RetiredResources {
    stingers: NativeSourcePlayback,
    audio: NativeStingerAudioRuntime,
}

pub(crate) struct NativeStingerRetirements {
    sender: Option<SyncSender<Box<RetiredResources>>>,
    workers: Vec<thread::JoinHandle<()>>,
    pending: Arc<AtomicUsize>,
}

struct NativeStingerPreflight {
    result: Receiver<Result<NativeStingerMutation, ()>>,
    worker: thread::JoinHandle<()>,
}

impl NativeStingerRetirements {
    pub(crate) fn start() -> AppResult<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Box<RetiredResources>>(RETIREMENT_QUEUE);
        let receiver = Arc::new(Mutex::new(receiver));
        let pending = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::with_capacity(RETIREMENT_WORKERS);
        for index in 0..RETIREMENT_WORKERS {
            let receiver = Arc::clone(&receiver);
            let pending = Arc::clone(&pending);
            workers.push(
                thread::Builder::new()
                    .name(format!("freemix-native-stinger-retirement-{index}"))
                    .spawn(move || {
                        loop {
                            let retired = receiver
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .recv();
                            let Ok(retired) = retired else { break };
                            let RetiredResources { stingers, audio } = *retired;
                            drop(stingers);
                            drop(audio);
                            pending.fetch_sub(1, Ordering::AcqRel);
                        }
                    })?,
            );
        }
        Ok(Self {
            sender: Some(sender),
            workers,
            pending,
        })
    }

    pub(crate) fn can_accept(&self) -> bool {
        self.pending.load(Ordering::Acquire) < RETIREMENT_LIMIT
    }

    fn retire(&self, retired: Box<RetiredResources>) -> Result<(), Box<RetiredResources>> {
        self.pending.fetch_add(1, Ordering::AcqRel);
        let Some(sender) = self.sender.as_ref() else {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(retired);
        };
        match sender.try_send(retired) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(retired) | TrySendError::Disconnected(retired)) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Err(retired)
            }
        }
    }

    pub(crate) fn discard(&self, mutation: NativeStingerMutation) -> AppResult<()> {
        let NativeStingerMutation {
            project_plan,
            stingers,
            audio,
            ordinary_video_limit,
        } = mutation;
        match self.retire(Box::new(RetiredResources { stingers, audio })) {
            Ok(()) => Ok(()),
            Err(retired) => {
                let RetiredResources { stingers, audio } = *retired;
                drop(NativeStingerMutation {
                    project_plan,
                    stingers,
                    audio,
                    ordinary_video_limit,
                });
                Err(
                    AppFailure("native Stinger retirement capacity changed after preflight".into())
                        .into(),
                )
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_pending_for_test(&self, pending: usize) {
        self.pending.store(pending, Ordering::Release);
    }
}

impl Drop for NativeStingerRetirements {
    fn drop(&mut self) {
        self.sender.take();
        // Codec teardown remains owned, but daemon shutdown never joins a
        // potentially blocking decoder/process cleanup.
        self.workers.clear();
    }
}

impl NativeDaemon {
    pub(crate) fn preflight_stinger_mutation_with_ticks(
        &mut self,
        candidate: StoredProject,
        control: &mut ControlService<Policy>,
        server: &ServerIdentity,
        process_shutdown: Option<&ProcessShutdown>,
    ) -> AppResult<Option<(Result<NativeStingerMutation, ()>, Vec<RuntimeEventMessage>)>> {
        if !self.stinger_retirements.can_accept() {
            return Ok(Some((Err(()), Vec::new())));
        }
        let runtime = Arc::clone(&self.runtime);
        let sources = Arc::clone(&self.resolved_sources);
        let assets_root = self.assets_root.clone();
        let (sender, result) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("freemix-native-stinger-preflight".to_owned())
            .spawn(move || {
                let prepared =
                    preflight_native_stinger_mutation(&runtime, &sources, assets_root, &candidate)
                        .map_err(|_| ());
                let _ = sender.send(prepared);
            })?;
        let preflight = NativeStingerPreflight { result, worker };
        let mut runtime_events = Vec::new();
        loop {
            match preflight.result.try_recv() {
                Ok(Ok(mutation)) => {
                    let _ = preflight.worker.join();
                    if self
                        .playback
                        .validate_retained_byte_limit(mutation.ordinary_video_limit)
                        .is_err()
                    {
                        self.stinger_retirements.discard(mutation)?;
                        return Ok(Some((Err(()), runtime_events)));
                    }
                    return Ok(Some((Ok(mutation), runtime_events)));
                }
                Ok(Err(())) | Err(TryRecvError::Disconnected) => {
                    let _ = preflight.worker.join();
                    return Ok(Some((Err(()), runtime_events)));
                }
                Err(TryRecvError::Empty) => {}
            }
            if requested_daemon_shutdown(Some(&*self), process_shutdown).is_some() {
                return Ok(None);
            }
            if let Some(events) = self.tick_if_due_collect(control, server)? {
                runtime_events.extend(events);
            }
            thread::sleep(NATIVE_IO_POLL_INTERVAL);
        }
    }

    pub(crate) fn stage_stinger_mutation(&mut self, mutation: NativeStingerMutation) {
        debug_assert!(self.pending_stinger_mutation.is_none());
        self.pending_stinger_mutation = Some(mutation);
    }

    pub(crate) fn install_stinger_mutation(&mut self) -> AppResult<()> {
        let Some(mutation) = self.pending_stinger_mutation.take() else {
            return Ok(());
        };
        self.playback
            .set_retained_byte_limit(mutation.ordinary_video_limit);
        let old_stingers = core::mem::replace(&mut self.stingers, mutation.stingers);
        let old_audio = self.master.replace_stinger_audio(mutation.audio);
        self.project_plan = mutation.project_plan;
        match self.stinger_retirements.retire(Box::new(RetiredResources {
            stingers: old_stingers,
            audio: old_audio,
        })) {
            Ok(()) => Ok(()),
            Err(retired) => {
                drop(retired);
                Err(AppFailure(
                    "native Stinger retirement capacity changed during installation".into(),
                )
                .into())
            }
        }
    }
}

fn preflight_native_stinger_mutation(
    runtime: &NativeMediaRuntime,
    sources: &[NativeResolvedSource],
    assets_root: PathBuf,
    stored: &StoredProject,
) -> AppResult<NativeStingerMutation> {
    let project_plan =
        NativeProjectPlan::compile(stored.project(), NativeProjectLimits::default())?;
    validate_native_stinger_sources(stored, sources)?;
    let requires_ffmpeg = native_stinger_requires_ffmpeg(stored, sources);
    let adapter = requires_ffmpeg
        .then(|| {
            Adapter::new(FfmpegConfig {
                allowed_root: Some(assets_root),
                ..FfmpegConfig::default()
            })
        })
        .transpose()?;
    if let Some(adapter) = &adapter {
        let capabilities = adapter.capabilities();
        if !matches!(capabilities.ffmpeg, ToolAvailability::Available { .. })
            || !matches!(capabilities.ffprobe, ToolAvailability::Available { .. })
        {
            return Err(AppFailure(
                "native Stinger preflight requires available codec capabilities".into(),
            )
            .into());
        }
    }
    let (stingers, ordinary_video_limit) =
        preflight_native_stinger_video(runtime, adapter.as_ref(), sources, stored)?;
    let settings = stored.project().settings();
    let audio = NativeStingerAudioRuntime::preflight_project_local_blocking(
        adapter.as_ref(),
        sources,
        &project_plan,
        &settings.audio,
        settings.frame_rate,
        native_clock_domain(),
        NativeAudioLimits::default(),
    )?;
    Ok(NativeStingerMutation {
        project_plan,
        stingers,
        audio,
        ordinary_video_limit,
    })
}

pub(crate) fn native_stinger_requires_ffmpeg(
    stored: &StoredProject,
    sources: &[NativeResolvedSource],
) -> bool {
    sources.iter().any(|source| {
        let NativeResolvedSource::LocalVideo { input, .. } = source else {
            return false;
        };
        stored
            .project()
            .stingers()
            .iter()
            .any(|config| config.preload && config.media_input == *input)
    })
}

#[cfg(test)]
pub(crate) const fn retirement_limit_for_test() -> usize {
    RETIREMENT_LIMIT
}
