#![cfg(all(feature = "native-media", target_os = "macos"))]

use std::{
    future::Future,
    io::Read,
    num::{NonZeroU32, NonZeroU128},
    path::Path,
    pin::pin,
    process::{Command, Stdio},
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
    time::{Duration, Instant},
};

use fm_clock::ClockTime;
use fm_codec_ffmpeg::{
    Adapter, Config, DecodeRequest, DecodedSequence, SequenceRequest, StreamSelector,
    ToolAvailability,
};
use fm_color::{ColorPipeline, LinearFrame, LinearRgba, working_color_metadata};
use fm_command::{Revision, RuntimeGeneration};
use fm_compositor::{TransitionKind, TransitionPlan};
use fm_engine::FrameResult;
use fm_frame::{
    AlphaMode, ChromaLocation, ClockDomainId, ColorMetadata, ColorPrimaries, CpuVideoFrame,
    MatrixCoefficients, SignalRange, TransferFunction, VideoFrameMetadata,
};
use fm_gpu::{NativeBackend, TextureFormat};
use fm_model::{
    CropRect, Input, InputKind, Layer, LayerGeometry, Project, ProjectSettings,
    Rgba8 as ModelRgba8, Rotation, Scene, SimulatedAudio, SimulatedInput, SimulatedVideo,
    SourceRef,
};
use fm_scheduler::FrameNumber;
use fm_sim::{Rgba8 as SimRgba8, SimulatedVideoSource, SourcePattern};
use fm_switcher::{ProgramFrame, SwitcherState, TransitionKind as SwitcherTransitionKind};
use fm_types::{
    AudioFormat, ChannelLayout, ColorMetadata as ModelColorMetadata, FrameRate, InputId,
    PixelFormat, ProjectId, SampleFormat, SampleRate, ScanMode, SceneId, VideoDimensions,
    VideoFormat,
};
use freemixd::native_media::{
    NativeMediaRuntime, NativeProjectLimits, NativeProjectPlan, NativeResolvedSource,
    NativeSourceLimits, NativeSourcePlayback, NativeSourceRegistry,
};
use half::f16;
use tempfile::tempdir;

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
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

fn required() -> bool {
    std::env::var("FM_REQUIRE_NATIVE_MEDIA").as_deref() == Ok("1")
}

fn report_unavailable(message: &str) {
    assert!(!required(), "FM_REQUIRE_NATIVE_MEDIA=1: {message}");
    eprintln!("native-media integration skipped: {message}");
}

fn expected_metadata() -> VideoFrameMetadata {
    VideoFrameMetadata::new(
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            matrix: MatrixCoefficients::Identity,
            range: SignalRange::Full,
            chroma_location: ChromaLocation::Center,
        },
        Some(AlphaMode::Straight),
    )
}

fn request(clock_domain: ClockDomainId) -> DecodeRequest {
    DecodeRequest {
        clock_domain,
        video: Some(SequenceRequest {
            selector: StreamSelector::Best,
            count: NonZeroU32::new(3).unwrap(),
        }),
        audio: None,
    }
}

fn component(pixel: LinearRgba, index: usize) -> f32 {
    [pixel.r, pixel.g, pixel.b, pixel.a][index]
}

fn runtime_tools_available(adapter: &Adapter) -> bool {
    let capabilities = adapter.capabilities();
    if !matches!(capabilities.ffmpeg, ToolAvailability::Available { .. })
        || !matches!(capabilities.ffprobe, ToolAvailability::Available { .. })
    {
        report_unavailable("FFmpeg and ffprobe runtime capabilities are required");
        return false;
    }

    true
}

fn generate_asset(path: &Path) -> Result<(), String> {
    let mut command = Command::new("ffmpeg");
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=5",
            "-vf",
            "setparams=range=limited:color_primaries=bt709:color_trc=bt709:colorspace=bt709",
            "-frames:v",
            "12",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "0",
            "-x264-params",
            "colorprim=bt709:transfer=bt709:colormatrix=bt709:fullrange=off",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
            "-colorspace",
            "bt709",
            "-color_range",
            "tv",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C");
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot spawn FFmpeg fixture generator: {error}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing FFmpeg stderr".to_owned())?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot poll FFmpeg fixture generator: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err("FFmpeg fixture generation timed out".to_owned());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stderr = reader
        .join()
        .map_err(|_| "FFmpeg stderr reader panicked".to_owned())?
        .map_err(|error| format!("cannot read FFmpeg stderr: {error}"))?;
    if stderr.len() > 64 * 1024 {
        return Err("FFmpeg fixture stderr exceeded 64 KiB".to_owned());
    }
    if !status.success() {
        return Err(format!(
            "libx264 fixture generation failed: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    Ok(())
}

fn assert_decoded(decoded: &DecodedSequence, clock_domain: ClockDomainId) {
    assert_eq!(decoded.video.len(), 3);
    assert!(decoded.audio.is_empty());
    for (sequence, frame) in decoded.video.iter().enumerate() {
        assert_eq!(frame.metadata(), Some(expected_metadata()));
        assert_eq!(frame.timing().clock_domain(), clock_domain);
        assert_eq!(
            frame.timing().sequence().get(),
            u64::try_from(sequence).unwrap()
        );
        assert_eq!(
            frame.timing().presentation_timestamp().as_nanos(),
            i64::try_from(sequence).unwrap() * 200_000_000
        );
        assert_eq!(frame.timing().duration().as_nanos(), 200_000_000);
    }
}

fn cpu_oracles(decoded: &DecodedSequence) -> Vec<LinearFrame> {
    let cpu_pipeline = ColorPipeline::new(expected_metadata().color(), working_color_metadata());
    decoded
        .video
        .iter()
        .map(|frame| {
            cpu_pipeline
                .decode_cpu_payload(frame.payload())
                .expect("decode CPU color oracle")
                .frame
        })
        .collect()
}

async fn exercise_gpu_slice(
    adapter: &Adapter,
    path: &Path,
    decode_request: DecodeRequest,
    decoded: &DecodedSequence,
    cpu_frames: &[LinearFrame],
) {
    let runtime = NativeMediaRuntime::new([NativeBackend::Metal])
        .await
        .expect("create one Metal native-media runtime");
    assert_eq!(
        runtime.diagnostic_adapter_info().backend,
        NativeBackend::Metal
    );

    // This test is already running off the daemon path; the subprocess portion
    // is deliberately blocking before async GPU normalization.
    let preroll = runtime
        .preroll_local_blocking(adapter, path, decode_request)
        .await
        .expect("blocking decode and async GPU normalization");
    assert_eq!(preroll.video().len(), 3);
    assert!(preroll.audio().is_empty());
    for (working, source) in preroll.video().iter().zip(&decoded.video) {
        assert_eq!(working.timing(), source.timing());
        assert_eq!(working.texture().format(), TextureFormat::Rgba16Float);
    }

    let points = [(0_u32, 0_u32), (32, 24), (63, 47)];
    let from_readback = runtime
        .diagnostic_readback(preroll.video()[0].texture())
        .await
        .expect("read Wipe source endpoint");
    let to_readback = runtime
        .diagnostic_readback(preroll.video()[1].texture())
        .await
        .expect("read Wipe target endpoint");
    for numerator in [0_u32, 1, 2] {
        let plan = TransitionPlan::compile(TransitionKind::Fade, numerator, 2).unwrap();
        let output = runtime
            .render_transition(plan, &preroll.video()[0], &preroll.video()[1])
            .await
            .expect("render GPU-resident Fade");
        assert_eq!(output.format(), TextureFormat::Rgba16Float);

        // Production methods above do not expose CPU pixels. Readback is a
        // separately named diagnostic used only by this integration test.
        let actual = runtime
            .diagnostic_readback(&output)
            .await
            .expect("diagnostic half-float readback");
        assert_eq!((actual.width, actual.height, actual.stride), (64, 48, 512));
        assert_eq!(actual.format, TextureFormat::Rgba16Float);

        for (x, y) in points {
            let pixel_index = usize::try_from(y * 64 + x).unwrap();
            for component_index in 0..4 {
                let from = f16::from_f32(component(
                    cpu_frames[0].pixel(x, y).unwrap(),
                    component_index,
                ))
                .to_f32();
                let to = f16::from_f32(component(
                    cpu_frames[1].pixel(x, y).unwrap(),
                    component_index,
                ))
                .to_f32();
                let progress = f32::from(u16::try_from(numerator).unwrap()) / 2.0;
                let expected = f16::from_f32((to - from).mul_add(progress, from)).to_f32();
                let offset = pixel_index * 8 + component_index * 2;
                let value = f16::from_bits(u16::from_le_bytes([
                    actual.bytes[offset],
                    actual.bytes[offset + 1],
                ]))
                .to_f32();
                assert!(
                    (value - expected).abs() <= 0.002,
                    "fade {numerator}/2 at ({x},{y}) component {component_index}: GPU {value}, CPU half {expected}"
                );
            }
        }
    }

    for numerator in [0_u32, 1, 2] {
        let plan = TransitionPlan::compile(TransitionKind::Wipe, numerator, 2).unwrap();
        let output = runtime
            .render_transition(plan, &preroll.video()[0], &preroll.video()[1])
            .await
            .expect("render GPU-resident Wipe");
        let actual = runtime
            .diagnostic_readback(&output)
            .await
            .expect("diagnostic Wipe readback");
        let boundary = 64 * numerator / 2;

        for x in [0_u32, 31, 32, 63] {
            let expected = if x < boundary {
                &to_readback
            } else {
                &from_readback
            };
            let pixel_index = usize::try_from(24 * 64 + x).unwrap();
            let offset = pixel_index * 8;
            assert_eq!(
                &actual.bytes[offset..offset + 8],
                &expected.bytes[offset..offset + 8],
                "wipe {numerator}/2 at ({x},24)"
            );
        }
    }

    exercise_source_refill(&runtime, adapter, path, decode_request).await;
}

#[allow(clippy::too_many_lines)]
async fn exercise_source_refill(
    runtime: &NativeMediaRuntime,
    adapter: &Adapter,
    path: &Path,
    decode_request: DecodeRequest,
) {
    let decoded = adapter
        .decode_local(
            path,
            DecodeRequest {
                video: Some(SequenceRequest {
                    selector: StreamSelector::Best,
                    count: NonZeroU32::new(12).unwrap(),
                }),
                ..decode_request
            },
        )
        .expect("decode complete CPU refill oracle");
    let cpu_frames = cpu_oracles(&decoded);
    let input = InputId::new(NonZeroU128::new(1).unwrap());
    let bars_input = InputId::new(NonZeroU128::new(2).unwrap());
    let live_input = InputId::new(NonZeroU128::new(3).unwrap());
    let (bars_frame, bars_oracle) = bars_fixture(decode_request.clock_domain);
    let live_clock = ClockDomainId::new(NonZeroU128::new(78).unwrap());
    let (live_seed, live_update, live_oracle) = live_fixture(live_clock);
    let mut playback = runtime
        .preflight_resolved_source_playback_mixed_local_blocking(
            Some(adapter),
            [
                NativeResolvedSource::LocalVideo {
                    input,
                    path: path.to_owned(),
                },
                NativeResolvedSource::RetainedFrame {
                    input: bars_input,
                    frame: bars_frame,
                },
                NativeResolvedSource::LiveFrame {
                    input: live_input,
                    frame: live_seed,
                },
            ],
            decode_request.clock_domain,
            StreamSelector::Best,
            NativeSourceLimits::default(),
        )
        .await
        .expect("preflight bounded GPU-resident source playback");
    assert_eq!(playback.registry().len(), 3);
    assert_eq!(
        playback.registry().retained_rgba16f_bytes(),
        64 * 48 * 8 * 10
    );
    let retained_before_live_update = playback.registry().retained_rgba16f_bytes();
    runtime
        .ingest_live_video_frame(&mut playback, live_input, live_update.clone())
        .await
        .expect("replace live source frame");
    assert_eq!(
        playback.registry().retained_rgba16f_bytes(),
        retained_before_live_update
    );
    assert_eq!(
        playback
            .registry()
            .timing_at_deadline(live_input, ClockTime::from_nanos(u64::MAX)),
        Some(live_update.timing())
    );
    assert_initial_source_timing(playback.registry(), input);
    assert!(
        runtime
            .service_source_playback(&mut playback, ClockTime::from_nanos(800_000_000))
            .await
            .expect("schedule bounded CPU refill")
    );
    let target_deadline = ClockTime::from_nanos(2_000_000_000);
    pump_until_covered(runtime, &mut playback, target_deadline).await;
    assert_eq!(
        playback
            .registry()
            .timing_at_deadline(input, target_deadline)
            .unwrap()
            .presentation_timestamp()
            .as_nanos(),
        2_000_000_000
    );
    assert_eq!(
        playback
            .registry()
            .timing_at_deadline(input, ClockTime::from_nanos(u64::MAX))
            .unwrap()
            .presentation_timestamp()
            .as_nanos(),
        2_200_000_000
    );
    assert_eq!(
        playback
            .registry()
            .timing_at_deadline(bars_input, ClockTime::from_nanos(u64::MAX))
            .unwrap()
            .presentation_timestamp()
            .as_nanos(),
        0
    );
    assert!(playback.registry().retained_rgba16f_bytes() <= 64 * 48 * 8 * 10);

    assert_deadline_frame(
        runtime,
        playback.registry(),
        input,
        target_deadline,
        &cpu_frames[10],
    )
    .await;
    assert_deadline_frame(
        runtime,
        playback.registry(),
        live_input,
        ClockTime::from_nanos(u64::MAX),
        &live_oracle,
    )
    .await;
    assert_deadline_frame(
        runtime,
        playback.registry(),
        bars_input,
        ClockTime::from_nanos(u64::MAX),
        &bars_oracle,
    )
    .await;
}

fn assert_initial_source_timing(registry: &NativeSourceRegistry, input: InputId) {
    for (deadline, expected_pts) in [
        (0, 0),
        (199_999_999, 0),
        (200_000_000, 200_000_000),
        (1_399_999_999, 1_200_000_000),
        (1_400_000_000, 1_400_000_000),
    ] {
        assert_eq!(
            registry
                .timing_at_deadline(input, ClockTime::from_nanos(deadline))
                .expect("selected source timing")
                .presentation_timestamp()
                .as_nanos(),
            expected_pts
        );
    }
}

fn bars_fixture(clock_domain: ClockDomainId) -> (CpuVideoFrame, LinearFrame) {
    let mut source = SimulatedVideoSource::new(
        64,
        48,
        FrameRate::new(5, 1).unwrap(),
        clock_domain,
        SourcePattern::Bars,
    )
    .unwrap();
    let frame = source.next_frame().unwrap().unwrap();
    let oracle = ColorPipeline::new(frame.metadata().unwrap().color(), working_color_metadata())
        .decode_cpu_payload(frame.payload())
        .unwrap()
        .frame;
    (frame, oracle)
}

fn live_fixture(clock_domain: ClockDomainId) -> (CpuVideoFrame, CpuVideoFrame, LinearFrame) {
    let mut source = SimulatedVideoSource::new(
        64,
        48,
        FrameRate::new(5, 1).unwrap(),
        clock_domain,
        SourcePattern::Bars,
    )
    .unwrap();
    let seed = source.next_frame().unwrap().unwrap();
    let update = source.next_frame().unwrap().unwrap();
    let oracle = ColorPipeline::new(update.metadata().unwrap().color(), working_color_metadata())
        .decode_cpu_payload(update.payload())
        .unwrap()
        .frame;
    (seed, update, oracle)
}

async fn pump_until_covered(
    runtime: &NativeMediaRuntime,
    playback: &mut NativeSourcePlayback,
    deadline: ClockTime,
) {
    let timeout = Instant::now() + Duration::from_secs(15);
    loop {
        if runtime
            .service_source_playback(playback, deadline)
            .await
            .expect("pump bounded CPU refill into Metal ring")
        {
            return;
        }
        assert!(Instant::now() < timeout, "native source refill timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

async fn assert_deadline_frame(
    runtime: &NativeMediaRuntime,
    registry: &NativeSourceRegistry,
    input: InputId,
    deadline: ClockTime,
    expected_frame: &LinearFrame,
) {
    let frame = FrameResult {
        fade_to_black: fm_switcher::FadeToBlackFrame::LIVE,
        frame: FrameNumber::new(10),
        deadline,
        program: ProgramFrame {
            primary: input,
            secondary: None,
            transition_kind: None,
            mix_numerator: 0,
            mix_denominator: 1,
            mix_start_numerator: 0,
            mix_end_numerator: 0,
        },
        events: Vec::new(),
        revision: Revision::new(0),
        runtime_generation: RuntimeGeneration::new(0),
    };
    let selected = runtime
        .render_frame_result(registry, &frame)
        .await
        .expect("render deadline-selected source frame");
    let actual = runtime
        .diagnostic_readback(&selected)
        .await
        .expect("read deadline-selected source frame");
    let x = 32_u32;
    let y = 24_u32;
    let pixel_index = usize::try_from(y * 64 + x).unwrap();
    for component_index in 0..4 {
        let expected = f16::from_f32(component(
            expected_frame.pixel(x, y).unwrap(),
            component_index,
        ))
        .to_f32();
        let offset = pixel_index * 8 + component_index * 2;
        let value = f16::from_bits(u16::from_le_bytes([
            actual.bytes[offset],
            actual.bytes[offset + 1],
        ]))
        .to_f32();
        assert!(
            (value - expected).abs() <= 0.002,
            "deadline-selected component {component_index}: GPU {value}, CPU half {expected}"
        );
    }
}

#[test]
#[ignore = "requires FFmpeg with libx264 and a native macOS Metal adapter"]
fn local_h264_file_reaches_metal_normalization_fade_and_wipe() {
    let adapter = Adapter::new(Config::default()).expect("construct FFmpeg adapter");
    if !runtime_tools_available(&adapter) {
        return;
    }
    let directory = tempdir().expect("create native-media test directory");
    let path = directory.path().join("bt709-x264.mkv");
    if let Err(error) = generate_asset(&path) {
        report_unavailable(&error);
        return;
    }

    let clock_domain = ClockDomainId::new(NonZeroU128::new(77).unwrap());
    let decode_request = request(clock_domain);
    let decoded = adapter
        .decode_local(&path, decode_request)
        .expect("decode generated Matroska through fm-codec-ffmpeg");
    assert_decoded(&decoded, clock_domain);
    let cpu_frames = cpu_oracles(&decoded);

    block_on(exercise_gpu_slice(
        &adapter,
        &path,
        decode_request,
        &decoded,
        &cpu_frames,
    ));
}

#[test]
#[allow(clippy::float_cmp, clippy::too_many_lines)]
fn native_metal_renders_scenes_before_exact_fade_and_wipe() {
    let runtime = block_on(NativeMediaRuntime::new([NativeBackend::Metal]))
        .expect("create Metal scene runtime");
    let clock_domain = ClockDomainId::new(NonZeroU128::new(91).unwrap());
    let frame_rate = FrameRate::new(30, 1).unwrap();
    let red_leaf = InputId::new(NonZeroU128::new(1).unwrap());
    let blue_leaf = InputId::new(NonZeroU128::new(2).unwrap());
    let primary_input = InputId::new(NonZeroU128::new(3).unwrap());
    let secondary_input = InputId::new(NonZeroU128::new(4).unwrap());
    let retained = |input, color| {
        let mut source =
            SimulatedVideoSource::new(4, 2, frame_rate, clock_domain, SourcePattern::Solid(color))
                .unwrap();
        NativeResolvedSource::RetainedFrame {
            input,
            frame: source.next_frame().unwrap().unwrap(),
        }
    };
    let playback = runtime
        .preflight_resolved_source_playback_mixed_blocking(
            None,
            [
                retained(red_leaf, SimRgba8::new(255, 0, 0, 255)),
                retained(blue_leaf, SimRgba8::new(0, 0, 255, 255)),
            ],
            clock_domain,
            StreamSelector::Best,
            NativeSourceLimits::default(),
        )
        .expect("normalize timed physical source frames");
    let mut project = Project::new(
        ProjectId::new(NonZeroU128::new(90).unwrap()),
        "Metal shared nested scene transition",
        ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(4, 2).unwrap(),
                frame_rate,
                pixel_format: PixelFormat::Rgba8,
                scan: ScanMode::Progressive,
                color: ModelColorMetadata::default(),
            },
            audio: AudioFormat {
                sample_rate: SampleRate::new(48_000).unwrap(),
                sample_format: SampleFormat::F32,
                channels: ChannelLayout::stereo(),
            },
        },
    );
    for (id, color) in [
        (
            red_leaf,
            SimulatedVideo::Solid(fm_model::SolidColor::new(255, 0, 0, 255)),
        ),
        (
            blue_leaf,
            SimulatedVideo::Solid(fm_model::SolidColor::new(0, 0, 255, 255)),
        ),
    ] {
        project.add_input(Input {
            id,
            name: format!("physical {id}"),
            kind: InputKind::Simulated(SimulatedInput::new(color, SimulatedAudio::Silence)),
            required_capabilities: Vec::new(),
        });
    }
    let shared_scene = SceneId::new(NonZeroU128::new(11).unwrap());
    let primary_scene = SceneId::new(NonZeroU128::new(12).unwrap());
    let secondary_scene = SceneId::new(NonZeroU128::new(13).unwrap());
    for (id, scene_id) in [
        (primary_input, primary_scene),
        (secondary_input, secondary_scene),
    ] {
        project.add_input(Input {
            id,
            name: format!("scene {id}"),
            kind: InputKind::Scene {
                scene_id,
                audio_source: None,
            },
            required_capabilities: Vec::new(),
        });
    }
    project.add_scene(Scene {
        id: shared_scene,
        name: "shared red window".into(),
        background: ModelRgba8::OPAQUE_BLACK,
        layers: vec![metal_scene_layer(
            SourceRef::Input(red_leaf),
            LayerGeometry::new(1, 0, 2, 2, Rotation::Deg0),
            None,
            0,
        )],
    });
    project.add_scene(Scene {
        id: primary_scene,
        name: "cropped shared primary".into(),
        background: ModelRgba8::OPAQUE_BLACK,
        layers: vec![metal_scene_layer(
            SourceRef::Scene(shared_scene),
            LayerGeometry::new(0, 0, 2, 2, Rotation::Deg0),
            Some(CropRect::new(1, 0, 2, 2)),
            0,
        )],
    });
    project.add_scene(Scene {
        id: secondary_scene,
        name: "shared plus blue secondary".into(),
        background: ModelRgba8::OPAQUE_BLACK,
        layers: vec![
            metal_scene_layer(
                SourceRef::Scene(shared_scene),
                LayerGeometry::new(0, 0, 4, 2, Rotation::Deg0),
                None,
                0,
            ),
            metal_scene_layer(
                SourceRef::Input(blue_leaf),
                LayerGeometry::new(2, 0, 2, 2, Rotation::Deg0),
                None,
                1,
            ),
        ],
    });
    let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default())
        .expect("compile scene routes");
    assert_eq!(plan.peak_rgba16f_targets(), 6);
    let red = native_leaf_rgba16f(&runtime, playback.registry(), red_leaf);
    let blue = native_leaf_rgba16f(&runtime, playback.registry(), blue_leaf);
    let half = |value| f16::from_f32(value * 0.5).to_f32();

    for kind in [SwitcherTransitionKind::Fade, SwitcherTransitionKind::Wipe] {
        for (sequence, deadline) in [0, 33_333_333, 2_000_000_000].into_iter().enumerate() {
            let frame = FrameResult {
                fade_to_black: fm_switcher::FadeToBlackFrame::LIVE,
                frame: FrameNumber::new(u64::try_from(sequence).unwrap()),
                deadline: ClockTime::from_nanos(deadline),
                program: ProgramFrame {
                    primary: primary_input,
                    secondary: Some(secondary_input),
                    transition_kind: Some(kind),
                    mix_numerator: 1,
                    mix_denominator: 2,
                    mix_start_numerator: 0,
                    mix_end_numerator: 1,
                },
                events: Vec::new(),
                revision: Revision::new(0),
                runtime_generation: RuntimeGeneration::new(0),
            };
            let output =
                block_on(runtime.render_project_frame_result(playback.registry(), &plan, &frame))
                    .expect("render nested scenes before transition");
            let readback =
                block_on(runtime.diagnostic_readback(&output)).expect("read scene transition");
            for x in 0..4_u32 {
                let expected = match kind {
                    SwitcherTransitionKind::Fade => match x {
                        0 => [half(red[0]), half(red[1]), half(red[2]), 1.0],
                        1 => red,
                        2 | 3 => [half(blue[0]), half(blue[1]), half(blue[2]), 1.0],
                        _ => unreachable!(),
                    },
                    SwitcherTransitionKind::Wipe if x == 1 => red,
                    SwitcherTransitionKind::Wipe => [0.0, 0.0, 0.0, 1.0],
                    _ => unreachable!(),
                };
                assert_metal_rgba16f(&readback.bytes, 4, x, 0, expected, kind, deadline);
            }
        }
    }

    let mut switcher = SwitcherState::new(
        vec![primary_input, secondary_input],
        primary_input,
        secondary_input,
    )
    .unwrap();
    switcher.request_fade_to_black(true, 1).unwrap();
    let frame = FrameResult {
        fade_to_black: switcher.fade_to_black_frame(),
        frame: FrameNumber::new(3),
        deadline: ClockTime::ZERO,
        program: ProgramFrame {
            primary: primary_input,
            secondary: Some(secondary_input),
            transition_kind: Some(SwitcherTransitionKind::Fade),
            mix_numerator: 1,
            mix_denominator: 2,
            mix_start_numerator: 0,
            mix_end_numerator: 1,
        },
        events: Vec::new(),
        revision: Revision::new(0),
        runtime_generation: RuntimeGeneration::new(0),
    };
    let output = block_on(runtime.render_project_frame_result(playback.registry(), &plan, &frame))
        .expect("apply Fade-to-Black after scene and Program composition");
    let readback =
        block_on(runtime.diagnostic_readback(&output)).expect("read black Program output");
    for x in 0..4_u32 {
        assert_metal_rgba16f(
            &readback.bytes,
            4,
            x,
            0,
            [0.0, 0.0, 0.0, 1.0],
            SwitcherTransitionKind::Fade,
            0,
        );
    }
}

#[test]
fn native_metal_project_frames_use_one_completed_in_flight_slot_without_readback() {
    const FRAMES: u64 = 96;

    let runtime = block_on(NativeMediaRuntime::new([NativeBackend::Metal]))
        .expect("create Metal bounded-frame runtime");
    let clock_domain = ClockDomainId::new(NonZeroU128::new(92).unwrap());
    let playback = runtime
        .preflight_resolved_source_playback_mixed_blocking(
            None,
            Vec::<NativeResolvedSource>::new(),
            clock_domain,
            StreamSelector::Best,
            NativeSourceLimits::default(),
        )
        .expect("create empty physical registry");
    let frame_rate = FrameRate::new(30, 1).unwrap();
    let input = InputId::new(NonZeroU128::new(20).unwrap());
    let scene = SceneId::new(NonZeroU128::new(21).unwrap());
    let mut project = Project::new(
        ProjectId::new(NonZeroU128::new(91).unwrap()),
        "Metal bounded frame ring",
        ProjectSettings {
            frame_rate,
            video: VideoFormat {
                dimensions: VideoDimensions::new(4, 2).unwrap(),
                frame_rate,
                pixel_format: PixelFormat::Rgba8,
                scan: ScanMode::Progressive,
                color: ModelColorMetadata::default(),
            },
            audio: AudioFormat {
                sample_rate: SampleRate::new(48_000).unwrap(),
                sample_format: SampleFormat::F32,
                channels: ChannelLayout::stereo(),
            },
        },
    );
    project.add_input(Input {
        id: input,
        name: "bounded scene".into(),
        kind: InputKind::Scene {
            scene_id: scene,
            audio_source: None,
        },
        required_capabilities: Vec::new(),
    });
    project.add_scene(Scene {
        id: scene,
        name: "bounded background".into(),
        background: ModelRgba8::OPAQUE_BLACK,
        layers: Vec::new(),
    });
    let plan = NativeProjectPlan::compile(&project, NativeProjectLimits::default()).unwrap();
    assert_eq!(plan.peak_rgba16f_targets(), 4);

    let mut latest_program = None;
    for frame_number in 0..FRAMES {
        let frame = FrameResult {
            fade_to_black: fm_switcher::FadeToBlackFrame::LIVE,
            frame: FrameNumber::new(frame_number),
            deadline: ClockTime::from_nanos(frame_number * 33_333_333),
            program: ProgramFrame {
                primary: input,
                secondary: None,
                transition_kind: None,
                mix_numerator: 0,
                mix_denominator: 1,
                mix_start_numerator: 0,
                mix_end_numerator: 0,
            },
            events: Vec::new(),
            revision: Revision::new(0),
            runtime_generation: RuntimeGeneration::new(0),
        };
        let next =
            block_on(runtime.render_project_frame_result(playback.registry(), &plan, &frame))
                .expect("render bounded project frame without readback");
        latest_program = Some(next);
        let telemetry = runtime.project_frame_telemetry();
        assert_eq!(telemetry.frames_submitted, frame_number + 1);
        assert_eq!(telemetry.completion_waits, frame_number);
        assert_eq!(telemetry.in_flight_slots, 1);
        assert_eq!(telemetry.peak_in_flight_slots, 1);
    }
    assert!(latest_program.is_some());
    runtime
        .complete_project_frame_blocking()
        .expect("complete final bounded frame slot");
    assert_eq!(
        runtime.project_frame_telemetry(),
        freemixd::native_media::NativeProjectFrameTelemetry {
            frames_submitted: FRAMES,
            completion_waits: FRAMES,
            in_flight_slots: 0,
            peak_in_flight_slots: 1,
        }
    );
}

fn native_leaf_rgba16f(
    runtime: &NativeMediaRuntime,
    registry: &NativeSourceRegistry,
    input: InputId,
) -> [f32; 4] {
    let frame = FrameResult {
        fade_to_black: fm_switcher::FadeToBlackFrame::LIVE,
        frame: FrameNumber::new(0),
        deadline: ClockTime::from_nanos(0),
        program: ProgramFrame {
            primary: input,
            secondary: None,
            transition_kind: None,
            mix_numerator: 0,
            mix_denominator: 1,
            mix_start_numerator: 0,
            mix_end_numerator: 0,
        },
        events: Vec::new(),
        revision: Revision::new(0),
        runtime_generation: RuntimeGeneration::new(0),
    };
    let output =
        block_on(runtime.render_frame_result(registry, &frame)).expect("render timed leaf");
    let readback = block_on(runtime.diagnostic_readback(&output)).expect("read timed leaf");
    std::array::from_fn(|channel| {
        let offset = channel * 2;
        f16::from_bits(u16::from_le_bytes([
            readback.bytes[offset],
            readback.bytes[offset + 1],
        ]))
        .to_f32()
    })
}

fn metal_scene_layer(
    source: SourceRef,
    geometry: LayerGeometry,
    crop: Option<CropRect>,
    z_order: i32,
) -> Layer {
    Layer {
        name: "Metal geometry".into(),
        source,
        enabled: true,
        geometry,
        crop,
        mask: None,
        opacity: u8::MAX,
        z_order,
    }
}

#[allow(clippy::float_cmp)]
fn assert_metal_rgba16f(
    bytes: &[u8],
    width: u32,
    x: u32,
    y: u32,
    expected: [f32; 4],
    kind: SwitcherTransitionKind,
    deadline: u64,
) {
    let pixel = usize::try_from(y * width + x).unwrap();
    for (channel, expected) in expected.into_iter().enumerate() {
        let offset = pixel * 8 + channel * 2;
        let actual =
            f16::from_bits(u16::from_le_bytes([bytes[offset], bytes[offset + 1]])).to_f32();
        assert_eq!(
            actual, expected,
            "{kind:?} deadline={deadline} x={x} channel={channel}"
        );
    }
}
