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
use fm_scheduler::FrameNumber;
use fm_sim::{SimulatedVideoSource, SourcePattern};
use fm_switcher::ProgramFrame;
use fm_types::{FrameRate, InputId};
use freemixd::native_media::{
    NativeMediaRuntime, NativeResolvedSource, NativeSourceLimits, NativeSourcePlayback,
    NativeSourceRegistry,
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
    for numerator in [0_u32, 1, 2] {
        let plan = TransitionPlan::compile(TransitionKind::Fade, numerator, 2).unwrap();
        let output = runtime
            .render_cut_or_fade(plan, &preroll.video()[0], &preroll.video()[1])
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
        frame: FrameNumber::new(10),
        deadline,
        program: ProgramFrame {
            primary: input,
            secondary: None,
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
fn local_h264_file_reaches_metal_normalization_and_fade() {
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
