use std::fmt::Write as _;

use fm_model::{
    InputKind, RestartPolicy, Rotation, SimulatedAudio, SimulatedVideo, SourceRef, StartupPolicy,
};
use fm_types::{
    Channel, ChromaLocation, ColorPrimaries, MatrixCoefficients, PixelFormat, SampleFormat,
    ScanMode, SignalRange, TransferFunction,
};

use crate::{
    CURRENT_SCHEMA_VERSION, FadeToBlackState, IdempotencyReceipt, ManualTransitionKind,
    ManualTransitionState, ReceiptOutcome, RuntimeOverlayBorder, RuntimeOverlayChannel,
    RuntimeOverlayPosition, RuntimeOverlayTransition, StoredProject,
};

pub(crate) fn encode(stored: &StoredProject) -> String {
    let mut output = String::new();
    write_project(&mut output, stored.project());
    write_runtime(&mut output, stored);
    output.push_str("\n}\n");
    output
}

fn write_project(output: &mut String, project: &fm_model::Project) {
    let settings = project.settings();
    write!(
        output,
        "{{\n  \"schema_version\": {CURRENT_SCHEMA_VERSION},\n  \"project\": {{\n    \"id\": "
    )
    .expect("writing to a string cannot fail");
    write!(output, "{}", project.id()).expect("writing to a string cannot fail");
    output.push_str(",\n    \"name\": \"");
    escape_string(output, project.name());
    output.push_str("\",\n    \"settings\": {\n      \"frame_rate\": ");
    write_frame_rate(output, settings.frame_rate);
    output.push_str(",\n      \"video\": {");
    write!(
        output,
        "\n        \"width\": {},\n        \"height\": {},\n        \"frame_rate\": ",
        settings.video.dimensions.width(),
        settings.video.dimensions.height()
    )
    .expect("writing to a string cannot fail");
    write_frame_rate(output, settings.video.frame_rate);
    write!(
        output,
        ",\n        \"pixel_format\": \"{}\",\n        \"scan\": \"{}\",\n        \"color\": {{\n          \"primaries\": \"{}\",\n          \"transfer\": \"{}\",\n          \"matrix\": \"{}\",\n          \"range\": \"{}\",\n          \"chroma_location\": \"{}\"\n        }}\n      }},\n      \"audio\": {{\n        \"sample_rate_hz\": {},\n        \"sample_format\": \"{}\",\n        \"channels\": [",
        pixel_format(settings.video.pixel_format),
        scan_mode(settings.video.scan),
        color_primaries(settings.video.color.primaries),
        transfer_function(settings.video.color.transfer),
        matrix_coefficients(settings.video.color.matrix),
        signal_range(settings.video.color.range),
        chroma_location(settings.video.color.chroma_location),
        settings.audio.sample_rate.hertz(),
        sample_format(settings.audio.sample_format),
    ).expect("writing to a string cannot fail");
    for (index, channel) in settings.audio.channels.channels().iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        write!(output, "\"{}\"", channel_name(*channel)).expect("writing to a string cannot fail");
    }
    output.push_str("]\n      }\n    },");
    write_project_collections(output, project);
    output.push_str("\n    \"main_mix\": ");
    if let Some(main_mix) = project.main_mix() {
        write!(
            output,
            "{{\"desired_program\": {}, \"desired_preview\": {}}}",
            main_mix.desired_program, main_mix.desired_preview
        )
        .expect("writing to a string cannot fail");
    } else {
        output.push_str("null");
    }
    output.push_str(",\n    \"restart_policy\": ");
    match project.restart_policy() {
        RestartPolicy::Never => output.push_str("{\"type\": \"never\"}"),
        RestartPolicy::Always => output.push_str("{\"type\": \"always\"}"),
        RestartPolicy::OnFailure { max_attempts } => {
            write!(
                output,
                "{{\"type\": \"on_failure\", \"max_attempts\": {max_attempts}}}"
            )
            .expect("writing to a string cannot fail");
        }
    }
    output.push_str("\n  },");
}

fn write_runtime(output: &mut String, stored: &StoredProject) {
    output.push_str("\n  \"runtime\": {\n    \"routing\": {\n");
    let routing = stored.runtime_routing();
    write_optional_id(
        output,
        "desired_program_id",
        routing.desired_program_id,
        true,
    );
    write_optional_id(
        output,
        "realized_program_id",
        routing.realized_program_id,
        true,
    );
    write_optional_id(
        output,
        "desired_preview_id",
        routing.desired_preview_id,
        true,
    );
    write_optional_id(
        output,
        "realized_preview_id",
        routing.realized_preview_id,
        false,
    );
    let manual = stored.runtime_manual_transitions();
    output.push_str("    },\n    \"manual_transitions\": {\n      \"desired\": ");
    write_manual_transition(output, manual.desired);
    output.push_str(",\n      \"realized\": ");
    write_manual_transition(output, manual.realized);
    output.push_str("\n    },\n    \"fade_to_black\": {\n      \"desired\": ");
    write_fade_to_black(output, stored.runtime_fade_to_black().desired);
    output.push_str(",\n      \"realized\": ");
    write_fade_to_black(output, stored.runtime_fade_to_black().realized);
    output.push_str("\n    },\n    \"overlays\": {\n      \"desired\": [");
    write_overlays(output, &stored.runtime_overlays().desired);
    output.push_str(",\n      \"realized\": [");
    write_overlays(output, &stored.runtime_overlays().realized);
    output.push_str("\n    },\n");
    let position = stored.position();
    write!(
        output,
        "    \"position\": {{\n      \"revision\": {},\n      \"state_epoch\": {},\n      \"event_sequence\": {},\n      \"frames_rendered\": {},\n      \"runtime_generation\": {},\n      \"clock_time_nanos\": {}\n    }},\n    \"idempotency_receipts\": [",
        position.revision,
        position.state_epoch,
        position.event_sequence,
        position.frames_rendered,
        position.runtime_generation,
        position.clock_time_nanos,
    ).expect("writing to a string cannot fail");
    for (index, receipt) in stored.idempotency_receipts().iter().enumerate() {
        item_prefix(output, index, 6);
        write_receipt(output, receipt);
    }
    array_end(output, stored.idempotency_receipts().is_empty(), 4);
    output.push_str("\n  }");
}

fn write_overlays(output: &mut String, overlays: &[RuntimeOverlayChannel; 8]) {
    for (index, overlay) in overlays.iter().enumerate() {
        item_prefix(output, index, 8);
        output.push_str("{\"source\": ");
        if let Some(source) = overlay.source {
            write!(output, "{source}").expect("writing to a string cannot fail");
        } else {
            output.push_str("null");
        }
        write!(
            output,
            ", \"active\": {}, \"transition\": \"{}\", \"duration_frames\": {}, \"position\": \"{}\", \"border\": \"{}\", \"queued_sources\": [",
            overlay.active,
            match overlay.transition {
                RuntimeOverlayTransition::Cut => "cut",
                RuntimeOverlayTransition::Fade => "fade",
            },
            overlay.duration_frames,
            match overlay.position {
                RuntimeOverlayPosition::FullFrame => "full_frame",
                RuntimeOverlayPosition::TopLeft => "top_left",
                RuntimeOverlayPosition::TopRight => "top_right",
                RuntimeOverlayPosition::BottomLeft => "bottom_left",
                RuntimeOverlayPosition::BottomRight => "bottom_right",
            },
            match overlay.border {
                RuntimeOverlayBorder::None => "none",
                RuntimeOverlayBorder::ThinWhite => "thin_white",
                RuntimeOverlayBorder::ThickWhite => "thick_white",
            },
        )
        .expect("writing to a string cannot fail");
        for (queue_index, source) in overlay.queued_sources.iter().enumerate() {
            if queue_index != 0 {
                output.push_str(", ");
            }
            write!(output, "{source}").expect("writing to a string cannot fail");
        }
        output.push_str("], \"included_outputs\": [");
        for (output_index, output_id) in overlay.included_outputs.iter().enumerate() {
            if output_index != 0 {
                output.push_str(", ");
            }
            write!(output, "{output_id}").expect("writing to a string cannot fail");
        }
        output.push_str("]}");
    }
    array_end(output, false, 6);
}

fn write_fade_to_black(output: &mut String, state: FadeToBlackState) {
    write!(
        output,
        "{{\"target_active\": {}, \"position_numerator\": {}}}",
        state.target_active, state.position_numerator,
    )
    .expect("writing to a string cannot fail");
}

fn write_manual_transition(output: &mut String, transition: Option<ManualTransitionState>) {
    let Some(transition) = transition else {
        output.push_str("null");
        return;
    };
    let kind = match transition.kind {
        ManualTransitionKind::Fade => "fade",
        ManualTransitionKind::Wipe => "wipe",
        ManualTransitionKind::AlphaFade => "alpha_fade",
    };
    write!(
        output,
        "{{\"kind\": \"{kind}\", \"from_id\": {}, \"to_id\": {}, \"interval_start_basis_points\": {}, \"position_basis_points\": {}}}",
        transition.from_id,
        transition.to_id,
        transition.interval_start_basis_points,
        transition.position_basis_points,
    )
    .expect("writing to a string cannot fail");
}

fn write_project_collections(output: &mut String, project: &fm_model::Project) {
    output.push_str("\n    \"inputs\": [");
    for (index, input) in project.inputs().iter().enumerate() {
        item_prefix(output, index, 6);
        output.push_str("{\n        \"id\": ");
        write!(output, "{}", input.id).expect("writing to a string cannot fail");
        output.push_str(",\n        \"name\": \"");
        escape_string(output, &input.name);
        output.push_str("\",\n        \"kind\": ");
        write_input_kind(output, &input.kind);
        output.push_str(",\n        \"required_capabilities\": ");
        write_strings(output, &input.required_capabilities);
        output.push_str("\n      }");
    }
    array_end(output, project.inputs().is_empty(), 4);
    output.push_str(",\n    \"input_audio_strips\": [");
    for (index, strip) in project.input_audio_strips().iter().enumerate() {
        item_prefix(output, index, 6);
        write!(
            output,
            "{{\"input\": {}, \"gain_milli_db\": {}, \"balance_basis_points\": {}, \"delay_samples\": {}, \"muted\": {}, \"follow_video\": {}}}",
            strip.input,
            strip.state.gain.get(),
            strip.state.balance.get(),
            strip.state.delay_samples.get(),
            strip.state.muted,
            strip.state.follow_video,
        )
        .expect("writing to a string cannot fail");
    }
    array_end(output, project.input_audio_strips().is_empty(), 4);
    write_scenes(output, project);
    output.push_str(",\n    \"audio_buses\": [");
    for (index, bus) in project.audio_buses().iter().enumerate() {
        item_prefix(output, index, 6);
        write!(
            output,
            "{{\n        \"id\": {},\n        \"name\": \"",
            bus.id
        )
        .expect("writing to a string cannot fail");
        escape_string(output, &bus.name);
        output.push_str("\",\n        \"sends\": [");
        for (send_index, send) in bus.sends.iter().enumerate() {
            if send_index != 0 {
                output.push_str(", ");
            }
            write!(output, "{{\"destination\": {}}}", send.destination)
                .expect("writing to a string cannot fail");
        }
        output.push_str("]\n      }");
    }
    array_end(output, project.audio_buses().is_empty(), 4);
    output.push_str(",\n    \"outputs\": [");
    for (index, destination) in project.outputs().iter().enumerate() {
        item_prefix(output, index, 6);
        write!(
            output,
            "{{\n        \"id\": {},\n        \"name\": \"",
            destination.id
        )
        .expect("writing to a string cannot fail");
        escape_string(output, &destination.name);
        write!(
            output,
            "\",\n        \"video_source\": {},\n        \"audio_source\": {},\n        \"startup\": \"{}\",\n        \"required_capabilities\": ",
            destination.video_source,
            destination.audio_source,
            startup_policy(destination.startup),
        ).expect("writing to a string cannot fail");
        write_strings(output, &destination.required_capabilities);
        output.push_str("\n      }");
    }
    array_end(output, project.outputs().is_empty(), 4);
    output.push_str(",\n    \"stingers\": [");
    for (index, stinger) in project.stingers().iter().enumerate() {
        item_prefix(output, index, 6);
        let audio_policy = match stinger.audio_policy {
            fm_model::StingerAudioPolicy::Muted => "muted",
            fm_model::StingerAudioPolicy::StingerOnly => "stinger_only",
            fm_model::StingerAudioPolicy::MixWithProgram => "mix_with_program",
        };
        let fallback = match stinger.missing_media_fallback {
            fm_model::StingerMissingMediaFallback::Cut => "cut",
            fm_model::StingerMissingMediaFallback::Fade => "fade",
            fm_model::StingerMissingMediaFallback::KeepProgram => "keep_program",
        };
        write!(
            output,
            "{{\"slot\": {}, \"media_input\": {}, \"preload\": {}, \"cut_point_frames\": {}, \"audio_policy\": \"{}\", \"missing_media_fallback\": \"{}\"}}",
            stinger.slot.number(),
            stinger.media_input,
            stinger.preload,
            stinger.cut_point_frames,
            audio_policy,
            fallback,
        )
        .expect("writing to a string cannot fail");
    }
    array_end(output, project.stingers().is_empty(), 4);
    output.push(',');
}

fn write_scenes(output: &mut String, project: &fm_model::Project) {
    output.push_str(",\n    \"scenes\": [");
    for (index, scene) in project.scenes().iter().enumerate() {
        item_prefix(output, index, 6);
        write!(
            output,
            "{{\n        \"id\": {},\n        \"name\": \"",
            scene.id
        )
        .expect("writing to a string cannot fail");
        escape_string(output, &scene.name);
        write!(
            output,
            "\",\n        \"background\": {{\"red\": {}, \"green\": {}, \"blue\": {}, \"alpha\": {}}},\n        \"layers\": [",
            scene.background.red,
            scene.background.green,
            scene.background.blue,
            scene.background.alpha,
        )
        .expect("writing to a string cannot fail");
        for (layer_index, layer) in scene.layers.iter().enumerate() {
            item_prefix(output, layer_index, 10);
            output.push_str("{\n            \"name\": \"");
            escape_string(output, &layer.name);
            output.push_str("\",\n            \"source\": ");
            match layer.source {
                SourceRef::Input(id) => write!(output, "{{\"type\": \"input\", \"id\": {id}}}"),
                SourceRef::Scene(id) => write!(output, "{{\"type\": \"scene\", \"id\": {id}}}"),
            }
            .expect("writing to a string cannot fail");
            write!(
                output,
                ",\n            \"enabled\": {},\n            \"geometry\": {{\"translation_x\": {}, \"translation_y\": {}, \"width\": {}, \"height\": {}, \"rotation\": \"{}\"}},\n            \"crop\": ",
                layer.enabled,
                layer.geometry.translation_x,
                layer.geometry.translation_y,
                layer.geometry.width,
                layer.geometry.height,
                rotation(layer.geometry.rotation),
            )
            .expect("writing to a string cannot fail");
            if let Some(crop) = layer.crop {
                write!(
                    output,
                    "{{\"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}}}",
                    crop.x, crop.y, crop.width, crop.height,
                )
                .expect("writing to a string cannot fail");
            } else {
                output.push_str("null");
            }
            output.push_str(",\n            \"mask\": ");
            if let Some(mask) = layer.mask {
                write!(
                    output,
                    "{{\"x\": {}, \"y\": {}, \"width\": {}, \"height\": {}, \"invert\": {}}}",
                    mask.x, mask.y, mask.width, mask.height, mask.invert,
                )
                .expect("writing to a string cannot fail");
            } else {
                output.push_str("null");
            }
            write!(
                output,
                ",\n            \"opacity\": {},\n            \"z_order\": {}\n          }}",
                layer.opacity, layer.z_order,
            )
            .expect("writing to a string cannot fail");
        }
        array_end(output, scene.layers.is_empty(), 8);
        output.push_str("\n      }");
    }
    array_end(output, project.scenes().is_empty(), 4);
}

fn write_frame_rate(output: &mut String, rate: fm_types::FrameRate) {
    write!(
        output,
        "{{\"numerator\": {}, \"denominator\": {}}}",
        rate.numerator(),
        rate.denominator()
    )
    .expect("writing to a string cannot fail");
}

fn write_input_kind(output: &mut String, kind: &InputKind) {
    match kind {
        InputKind::Color => {
            output.push_str("{\"type\": \"color\"}");
        }
        InputKind::Media { asset_uri } => {
            write_string_variant(output, "media", "asset_uri", asset_uri);
        }
        InputKind::Device { stable_key } => {
            write_string_variant(output, "device", "stable_key", stable_key);
        }
        InputKind::Network { endpoint } => {
            write_string_variant(output, "network", "endpoint", endpoint);
        }
        InputKind::Scene {
            scene_id,
            audio_source,
        } => {
            write!(
                output,
                "{{\"type\": \"scene\", \"scene_id\": {scene_id}, \"audio_source\": "
            )
            .expect("writing to a string cannot fail");
            if let Some(audio_source) = audio_source {
                write!(output, "{audio_source}").expect("writing to a string cannot fail");
            } else {
                output.push_str("null");
            }
            output.push('}');
        }
        InputKind::Simulated(simulated) => {
            output.push_str("{\"type\": \"simulated\", \"video\": ");
            match simulated.video {
                SimulatedVideo::Bars => {
                    output.push_str("{\"type\": \"bars\"}");
                }
                SimulatedVideo::Solid(color) => {
                    write!(output, "{{\"type\": \"solid\", \"red\": {}, \"green\": {}, \"blue\": {}, \"alpha\": {}}}",
                        color.red, color.green, color.blue, color.alpha)
                        .expect("writing to a string cannot fail");
                }
            }
            output.push_str(", \"audio\": ");
            match simulated.audio {
                SimulatedAudio::Silence => {
                    output.push_str("{\"type\": \"silence\"}");
                }
                SimulatedAudio::Sine { frequency_hz } => {
                    write!(
                        output,
                        "{{\"type\": \"sine\", \"frequency_hz\": {frequency_hz}}}"
                    )
                    .expect("writing to a string cannot fail");
                }
            }
            output.push('}');
        }
    }
}

fn write_string_variant(output: &mut String, kind: &str, field: &str, value: &str) {
    write!(output, "{{\"type\": \"{kind}\", \"{field}\": \"")
        .expect("writing to a string cannot fail");
    escape_string(output, value);
    output.push_str("\"}");
}

fn write_strings(output: &mut String, values: &[String]) {
    output.push('[');
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            output.push_str(", ");
        }
        output.push('"');
        escape_string(output, value);
        output.push('"');
    }
    output.push(']');
}

fn write_optional_id(
    output: &mut String,
    name: &str,
    value: Option<fm_types::InputId>,
    comma: bool,
) {
    write!(output, "      \"{name}\": ").expect("writing to a string cannot fail");
    if let Some(value) = value {
        write!(output, "{value}").expect("writing to a string cannot fail");
    } else {
        output.push_str("null");
    }
    if comma {
        output.push(',');
    }
    output.push('\n');
}

fn write_receipt(output: &mut String, receipt: &IdempotencyReceipt) {
    output.push_str("{\n        \"key\": \"");
    escape_string(output, receipt.key());
    output.push_str("\",\n        \"command_id\": \"");
    escape_string(output, receipt.command_id());
    match receipt.outcome() {
        ReceiptOutcome::Accepted {
            revision,
            target_frame,
        } => {
            write!(output, "\",\n        \"outcome\": \"accepted\",\n        \"revision\": {revision},\n        \"target_frame\": {target_frame}\n      }}")
                .expect("writing to a string cannot fail");
        }
        ReceiptOutcome::Rejected {
            current_revision,
            code,
            message,
            retryable,
        } => {
            write!(output, "\",\n        \"outcome\": \"rejected\",\n        \"current_revision\": {current_revision},\n        \"code\": \"")
                .expect("writing to a string cannot fail");
            escape_string(output, code);
            output.push_str("\",\n        \"message\": \"");
            escape_string(output, message);
            write!(output, "\",\n        \"retryable\": {retryable}\n      }}")
                .expect("writing to a string cannot fail");
        }
    }
}

fn item_prefix(output: &mut String, index: usize, indent: usize) {
    if index != 0 {
        output.push(',');
    }
    output.push('\n');
    for _ in 0..indent {
        output.push(' ');
    }
}

fn array_end(output: &mut String, empty: bool, indent: usize) {
    if !empty {
        output.push('\n');
        for _ in 0..indent {
            output.push(' ');
        }
    }
    output.push(']');
}

const fn pixel_format(value: PixelFormat) -> &'static str {
    match value {
        PixelFormat::Rgba8 => "rgba8",
        PixelFormat::Bgra8 => "bgra8",
        PixelFormat::Rgba16Float => "rgba16_float",
        PixelFormat::Nv12 => "nv12",
        PixelFormat::P010 => "p010",
        PixelFormat::Yuv422 => "yuv422",
    }
}

const fn scan_mode(value: ScanMode) -> &'static str {
    match value {
        ScanMode::Progressive => "progressive",
        ScanMode::InterlacedTopFieldFirst => "interlaced_top_field_first",
        ScanMode::InterlacedBottomFieldFirst => "interlaced_bottom_field_first",
    }
}

const fn color_primaries(value: ColorPrimaries) -> &'static str {
    match value {
        ColorPrimaries::Bt601 => "bt601",
        ColorPrimaries::Bt709 => "bt709",
        ColorPrimaries::Bt2020 => "bt2020",
        ColorPrimaries::DisplayP3 => "display_p3",
    }
}

const fn transfer_function(value: TransferFunction) -> &'static str {
    match value {
        TransferFunction::Linear => "linear",
        TransferFunction::Srgb => "srgb",
        TransferFunction::Bt709 => "bt709",
        TransferFunction::Bt1886 => "bt1886",
        TransferFunction::Hlg => "hlg",
        TransferFunction::Pq => "pq",
    }
}

const fn matrix_coefficients(value: MatrixCoefficients) -> &'static str {
    match value {
        MatrixCoefficients::Identity => "identity",
        MatrixCoefficients::Bt601 => "bt601",
        MatrixCoefficients::Bt709 => "bt709",
        MatrixCoefficients::Bt2020NonConstant => "bt2020_non_constant",
    }
}

const fn signal_range(value: SignalRange) -> &'static str {
    match value {
        SignalRange::Full => "full",
        SignalRange::Limited => "limited",
    }
}

const fn chroma_location(value: ChromaLocation) -> &'static str {
    match value {
        ChromaLocation::Left => "left",
        ChromaLocation::Center => "center",
        ChromaLocation::TopLeft => "top_left",
    }
}

const fn sample_format(value: SampleFormat) -> &'static str {
    match value {
        SampleFormat::I16 => "i16",
        SampleFormat::I24 => "i24",
        SampleFormat::I32 => "i32",
        SampleFormat::F32 => "f32",
        SampleFormat::F64 => "f64",
    }
}

const fn channel_name(value: Channel) -> &'static str {
    match value {
        Channel::Mono => "mono",
        Channel::Left => "left",
        Channel::Right => "right",
        Channel::Center => "center",
        Channel::LowFrequency => "low_frequency",
        Channel::LeftSurround => "left_surround",
        Channel::RightSurround => "right_surround",
    }
}

const fn startup_policy(value: StartupPolicy) -> &'static str {
    match value {
        StartupPolicy::Stopped => "stopped",
        StartupPolicy::ReconcileDesiredState => "reconcile_desired_state",
    }
}

const fn rotation(value: Rotation) -> &'static str {
    match value {
        Rotation::Deg0 => "deg0",
        Rotation::Deg90 => "deg90",
        Rotation::Deg180 => "deg180",
        Rotation::Deg270 => "deg270",
    }
}

fn escape_string(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character <= '\u{1f}' => {
                write!(output, "\\u{:04x}", u32::from(character))
                    .expect("writing to a string cannot fail");
            }
            character => output.push(character),
        }
    }
}
