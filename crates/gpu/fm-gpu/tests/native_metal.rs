#![cfg(all(feature = "native-wgpu", target_os = "macos"))]

use std::{
    future::Future,
    pin::pin,
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread,
};

use fm_gpu::{
    NativeBackend, NativeContext, NativeFullscreenBlend, NativeFullscreenDraw,
    NativeFullscreenLoadOp, NativeFullscreenPipelineOptions, NativeFullscreenTimingSupport,
    ShaderDescriptor, ShaderLanguage, ShaderSource, ShaderStage,
};

const SAMPLE_FRAGMENT_SHADER: &str = r"
@group(0) @binding(0) var source: texture_2d<f32>;

@fragment
fn fragment_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(source, vec2<i32>(position.xy), 0);
}
";

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

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_draws_and_reads_exact_pixels() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        assert_eq!(context.adapter_info().backend, NativeBackend::Metal);
        assert!(!context.adapter_info().name.trim().is_empty());

        let image = context.diagnostic_readback().await.unwrap();
        assert_eq!((image.width, image.height, image.stride), (2, 2, 8));
        assert_eq!(
            image.rgba,
            [
                255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
            ]
        );
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_blends_two_draws_source_over() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let bottom = context
            .upload_rgba8(1, 1, 4, &[0, 0, 255, 255])
            .await
            .unwrap();
        let top = context
            .upload_rgba8(1, 1, 4, &[128, 0, 0, 128])
            .await
            .unwrap();
        let target = context.create_rgba8_render_target(1, 1).await.unwrap();
        let pipeline = context
            .create_fullscreen_pipeline_with_options(
                ShaderDescriptor::new(
                    "source-over oracle",
                    ShaderStage::Fragment,
                    ShaderLanguage::Wgsl,
                    "fragment_main",
                    ShaderSource::Text(SAMPLE_FRAGMENT_SHADER.to_owned()),
                ),
                NativeFullscreenPipelineOptions {
                    blend: NativeFullscreenBlend::PremultipliedSourceOver,
                    ..NativeFullscreenPipelineOptions::default()
                },
            )
            .await
            .unwrap();
        let uniform = [0; 16];
        let draws = [
            NativeFullscreenDraw::new(&pipeline, &bottom, &bottom, &uniform),
            NativeFullscreenDraw::new(&pipeline, &top, &top, &uniform),
        ];

        context
            .submit_fullscreen_pass(&target, NativeFullscreenLoadOp::ClearTransparent, &draws)
            .await
            .unwrap();

        assert_eq!(
            context.readback_rgba8(&target).await.unwrap().rgba,
            [128, 0, 127, 255]
        );
    });
}

#[test]
#[ignore = "requires a native macOS Metal adapter"]
fn native_metal_eventually_reports_fullscreen_gpu_timing() {
    block_on(async {
        let context = NativeContext::new([NativeBackend::Metal]).await.unwrap();
        let initial = context.take_fullscreen_timing_telemetry();
        if initial.support == NativeFullscreenTimingSupport::Unsupported {
            assert!(initial.completed_samples.is_empty());
            assert_eq!(initial.dropped_samples, 0);
            assert_eq!(initial.unavailable_samples, 0);
            return;
        }

        assert_eq!(initial.support, NativeFullscreenTimingSupport::Supported);
        for _ in 0..8 {
            context.diagnostic_readback().await.unwrap();
            let telemetry = context.take_fullscreen_timing_telemetry();
            if telemetry.completed_samples.iter().any(|sample| {
                sample.duration_nanoseconds().is_finite() && sample.duration_nanoseconds() > 0.0
            }) {
                return;
            }
        }
        panic!("timestamp-query support produced no finite positive fullscreen timing sample");
    });
}
