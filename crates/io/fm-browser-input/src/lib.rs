//! Isolated browser-input policy and deterministic renderer contracts.
//!
//! This crate does not embed or control a browser. Platform implementations can
//! implement [`BrowserRenderer`], while tests can use [`fake::FakeRenderer`].

mod contract;
pub mod fake;

pub use contract::{
    BrowserError, BrowserId, BrowserLimits, BrowserRenderer, BrowserState, ChildLifecycle,
    ChildSupervisor, CookieError, CookieJar, CrashDisposition, InteractionIntent, InteractionKind,
    InteractionMode, KeyState, MouseButton, NavigationBlockReason, NavigationBlocked,
    NavigationGrant, NavigationPolicy, NetworkBudget, NetworkLimitError, NetworkLimits, ProfileId,
    RenderOutput, RestartError, RestartPolicy, SurfaceConfig, SurfaceId, SurfaceSnapshot, Zoom,
    ZoomError,
};

pub use fm_frame::{AudioBlock, ClockDomainId, CpuVideoFrame, MediaTiming, VideoDimensions};
