use fm_protocol::{OverlayBorderPreset, OverlayPositionPreset};
use fm_ui_model::OverlayStatus;

pub(super) fn format(desired: Option<&OverlayStatus>, realized: Option<&OverlayStatus>) -> String {
    format!(
        "DESIRED: {} | LAST DAEMON CONFIRMATION (NOT OUTPUT-VIDEO PROOF): {}",
        state(desired),
        state(realized),
    )
}

pub(super) const fn position_label(position: OverlayPositionPreset) -> &'static str {
    match position {
        OverlayPositionPreset::FullFrame => "FULL",
        OverlayPositionPreset::TopLeft => "TL",
        OverlayPositionPreset::TopRight => "TR",
        OverlayPositionPreset::BottomLeft => "BL",
        OverlayPositionPreset::BottomRight => "BR",
    }
}

pub(super) const fn border_label(border: OverlayBorderPreset) -> &'static str {
    match border {
        OverlayBorderPreset::None => "NO BORDER",
        OverlayBorderPreset::ThinWhite => "THIN",
        OverlayBorderPreset::ThickWhite => "THICK",
    }
}

fn state(status: Option<&OverlayStatus>) -> String {
    status.map_or_else(
        || "UNAVAILABLE".to_owned(),
        |status| {
            format!(
                "SRC {} {} OP {} Q {} POS {} BORDER {}",
                status
                    .source
                    .map_or_else(|| "NONE".to_owned(), |source| source.to_string()),
                if status.active { "ACTIVE" } else { "INACTIVE" },
                status.opacity,
                status.queued_sources.len(),
                position_label(status.position),
                border_label(status.border),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU128;

    use fm_protocol::{OverlayBorderPreset, OverlayPositionPreset, OverlayTransitionKind};
    use fm_types::InputId;

    use super::*;

    fn overlay(
        source: Option<u128>,
        active: bool,
        opacity: u8,
        position: OverlayPositionPreset,
        border: OverlayBorderPreset,
        queued_sources: Vec<InputId>,
    ) -> OverlayStatus {
        OverlayStatus {
            channel: 1,
            source: source.map(|source| InputId::new(NonZeroU128::new(source).unwrap())),
            active,
            opacity,
            transition: OverlayTransitionKind::Cut,
            duration_frames: 30,
            position,
            border,
            queued_sources,
            included_outputs: Vec::new(),
        }
    }

    #[test]
    fn desired_take_is_distinct_from_last_daemon_confirmation() {
        let desired = overlay(
            Some(2),
            true,
            u8::MAX,
            OverlayPositionPreset::TopLeft,
            OverlayBorderPreset::ThinWhite,
            vec![InputId::new(NonZeroU128::new(3).unwrap())],
        );
        let realized = overlay(
            None,
            false,
            0,
            OverlayPositionPreset::FullFrame,
            OverlayBorderPreset::None,
            Vec::new(),
        );

        assert_eq!(
            format(Some(&desired), Some(&realized)),
            "DESIRED: SRC 2 ACTIVE OP 255 Q 1 POS TL BORDER THIN | LAST DAEMON CONFIRMATION (NOT OUTPUT-VIDEO PROOF): SRC NONE INACTIVE OP 0 Q 0 POS FULL BORDER NO BORDER"
        );
    }
}
