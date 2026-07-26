use std::{net::IpAddr, num::NonZeroU128};

use fm_browser_input::fake::{FakeRenderer, HtmlFixture};
use fm_browser_input::{
    BrowserError, BrowserId, BrowserLimits, BrowserRenderer, ChildLifecycle, ClockDomainId,
    CrashDisposition, InteractionIntent, InteractionKind, InteractionMode, KeyState,
    NavigationBlockReason, NavigationGrant, NavigationPolicy, NetworkLimitError, NetworkLimits,
    ProfileId, RestartError, RestartPolicy, SurfaceConfig, SurfaceId, VideoDimensions, Zoom,
};

fn nonzero(value: u128) -> NonZeroU128 {
    NonZeroU128::new(value).unwrap()
}

fn browser(value: u128) -> BrowserId {
    BrowserId::new(nonzero(value))
}

fn profile(value: u128) -> ProfileId {
    ProfileId::new(nonzero(value))
}

fn surface(value: u128) -> SurfaceId {
    SurfaceId::new(nonzero(value))
}

fn renderer() -> FakeRenderer {
    FakeRenderer::new(ClockDomainId::new(nonzero(99)))
}

fn config() -> SurfaceConfig {
    SurfaceConfig::new(VideoDimensions::new(2, 2).unwrap())
}

fn configured_renderer(config: SurfaceConfig) -> FakeRenderer {
    let mut renderer = renderer();
    renderer.create_browser(browser(1), profile(1)).unwrap();
    renderer
        .create_surface(browser(1), surface(1), config)
        .unwrap();
    renderer
}

#[test]
fn navigation_blocks_file_ssrf_and_unsupported_schemes_by_default() {
    let policy = NavigationPolicy::new();
    assert!(matches!(
        policy.check("file:///etc/passwd").unwrap_err().reason,
        NavigationBlockReason::FileAccess
    ));
    for url in [
        "http://127.0.0.1/admin",
        "http://2130706433/admin",
        "http://0x7f000001/admin",
        "http://0177.0.0.1/admin",
        "http://169.254.169.254/latest/meta-data",
        "https://[::1]/",
        "https://[::ffff:192.168.1.1]/",
        "http://intranet/",
    ] {
        assert!(
            matches!(
                policy.check(url).unwrap_err().reason,
                NavigationBlockReason::PrivateNetwork { .. }
            ),
            "{url}"
        );
    }
    assert!(matches!(
        policy.check("ftp://example.com/file").unwrap_err().reason,
        NavigationBlockReason::UnsupportedScheme { .. }
    ));
    assert!(matches!(
        policy
            .check_resolved("https://public.example/", &[IpAddr::from([10, 20, 30, 40])],)
            .unwrap_err()
            .reason,
        NavigationBlockReason::PrivateNetwork { .. }
    ));
    policy.check("https://example.com/live").unwrap();

    let mut granted = policy;
    granted.grant(NavigationGrant::File);
    granted.grant(NavigationGrant::Scheme("ftp".to_owned()));
    granted.check("file:///tmp/fixture.html").unwrap();
    assert!(matches!(
        granted.check("ftp://192.168.1.1/file").unwrap_err().reason,
        NavigationBlockReason::PrivateNetwork { .. }
    ));
    granted.grant(NavigationGrant::PrivateNetwork);
    granted.check("http://10.0.0.1/").unwrap();
    granted.check("ftp://example.com/file").unwrap();
}

#[test]
fn cookies_are_partitioned_by_profile_and_shared_only_by_profile_id() {
    let mut renderer = renderer();
    renderer.create_browser(browser(1), profile(10)).unwrap();
    renderer.create_browser(browser(2), profile(20)).unwrap();
    renderer.create_browser(browser(3), profile(10)).unwrap();
    renderer
        .set_cookie(browser(1), "EXAMPLE.com", "session", "alpha")
        .unwrap();

    assert_eq!(
        renderer.cookie(browser(1), "example.com", "session"),
        Some("alpha")
    );
    assert_eq!(renderer.cookie(browser(2), "example.com", "session"), None);
    assert_eq!(
        renderer.cookie(browser(3), "example.com", "session"),
        Some("alpha")
    );
}

#[test]
fn bounded_css_zoom_transparency_and_activation_are_applied() {
    assert!(Zoom::new(24).is_err());
    assert!(Zoom::new(501).is_err());

    let mut surface_config = config();
    surface_config.zoom = Zoom::new(150).unwrap();
    surface_config.transparent_background = true;
    surface_config.refresh_on_activate = true;
    surface_config
        .set_custom_css(
            Some("body { background: #10203040; }".to_owned()),
            BrowserLimits::default(),
        )
        .unwrap();
    let mut renderer = configured_renderer(surface_config);
    renderer
        .register_fixture(
            "https://example.com/css",
            HtmlFixture::new("<main>fixture</main>").unwrap(),
        )
        .unwrap();
    renderer
        .navigate(surface(1), "https://example.com/css")
        .unwrap();
    let output = renderer.render(surface(1), 0).unwrap();
    assert_eq!(
        output.video.payload().plane(0).unwrap().bytes()[..4],
        [0x10, 0x20, 0x30, 0x40]
    );
    assert_eq!(
        renderer.surface(surface(1)).unwrap().config.zoom.percent(),
        150
    );
    assert_eq!(output.navigation_generation, 1);
    renderer.activate(surface(1)).unwrap();
    assert_eq!(
        renderer.surface(surface(1)).unwrap().navigation_generation,
        2
    );

    let mut oversized = config();
    let limits = BrowserLimits {
        max_css_bytes: 3,
        ..BrowserLimits::default()
    };
    assert!(matches!(
        oversized.set_custom_css(Some("four".to_owned()), limits),
        Err(BrowserError::ResourceLimit {
            resource: "custom CSS bytes",
            ..
        })
    ));
}

#[test]
fn interaction_intents_are_enabled_bounded_and_recorded() {
    let disabled = configured_renderer(config());
    let intent = InteractionIntent {
        surface_id: surface(1),
        kind: InteractionKind::PointerMove { x: 1, y: 1 },
    };
    let mut disabled = disabled;
    assert!(matches!(
        disabled.interact(intent.clone()),
        Err(BrowserError::InteractionDisabled(id)) if id == surface(1)
    ));

    let mut enabled_config = config();
    enabled_config.interaction = InteractionMode::Enabled;
    let mut enabled = configured_renderer(enabled_config);
    enabled.interact(intent.clone()).unwrap();
    enabled
        .interact(InteractionIntent {
            surface_id: surface(1),
            kind: InteractionKind::Key {
                code: "Enter".to_owned(),
                state: KeyState::Pressed,
            },
        })
        .unwrap();
    assert!(matches!(
        enabled.interact(InteractionIntent {
            surface_id: surface(1),
            kind: InteractionKind::PointerMove { x: 2, y: 0 },
        }),
        Err(BrowserError::InteractionOutOfBounds { .. })
    ));
    assert_eq!(enabled.take_interactions(surface(1)).len(), 2);
}

#[test]
fn fake_fixture_produces_repeatable_timed_video_and_audio() {
    let mut renderer = configured_renderer(config());
    renderer
        .register_fixture(
            "https://example.com/av",
            HtmlFixture::new(
                r##"<fixture data-color="#01020304" data-audio="0.25,-0.5" data-frame-nanos="100000000" data-sample-rate="10" />"##,
            )
            .unwrap(),
        )
        .unwrap();
    renderer
        .navigate(surface(1), "https://example.com/av")
        .unwrap();

    let first = renderer.render(surface(1), 0).unwrap();
    let repeated = renderer.render(surface(1), 0).unwrap();
    let second = renderer.render(surface(1), 100_000_000).unwrap();
    assert_eq!(first, repeated);
    assert_eq!(first.video.timing().presentation_timestamp().as_nanos(), 0);
    assert_eq!(first.video.timing().duration().as_nanos(), 100_000_000);
    assert_eq!(second.video.timing().sequence().get(), 1);
    assert_eq!(first.audio.unwrap().sample(0, 0), Some(0.25));
    assert_eq!(second.audio.unwrap().sample(0, 0), Some(-0.5));
}

#[test]
fn crashes_restart_then_quarantine_deterministically() {
    let restart = RestartPolicy {
        max_restarts: 1,
        window_nanos: 100,
        quarantine_nanos: 50,
    };
    let mut renderer = FakeRenderer::with_limits(
        ClockDomainId::new(nonzero(1)),
        NavigationPolicy::new(),
        BrowserLimits::default(),
        NetworkLimits::default(),
        restart,
    );
    assert_eq!(renderer.crash_child(10), CrashDisposition::RestartPending);
    assert_eq!(renderer.child_lifecycle(), ChildLifecycle::RestartPending);
    assert!(matches!(
        renderer.create_browser(browser(1), profile(1)),
        Err(BrowserError::ChildUnavailable(
            ChildLifecycle::RestartPending
        ))
    ));
    renderer.restart_child(11).unwrap();
    assert_eq!(
        renderer.crash_child(20),
        CrashDisposition::Quarantined { until_nanos: 70 }
    );
    assert_eq!(
        renderer.restart_child(69),
        Err(RestartError::Quarantined { until_nanos: 70 })
    );
    renderer.restart_child(70).unwrap();
    assert_eq!(renderer.child_lifecycle(), ChildLifecycle::Running);
}

#[test]
fn resource_and_network_limits_fail_before_state_is_committed() {
    let browser_limits = BrowserLimits {
        max_browsers: 1,
        max_surfaces_per_browser: 1,
        max_width: 2,
        max_frame_bytes: 16,
        ..BrowserLimits::default()
    };
    let network_limits = NetworkLimits {
        max_requests_per_navigation: 2,
        max_response_bytes: 100,
        max_redirects: 1,
    };
    let mut renderer = FakeRenderer::with_limits(
        ClockDomainId::new(nonzero(1)),
        NavigationPolicy::new(),
        browser_limits,
        network_limits,
        RestartPolicy::default(),
    );
    renderer.create_browser(browser(1), profile(1)).unwrap();
    assert!(matches!(
        renderer.create_browser(browser(2), profile(2)),
        Err(BrowserError::ResourceLimit {
            resource: "browser count",
            ..
        })
    ));
    renderer
        .create_surface(browser(1), surface(1), config())
        .unwrap();
    assert!(matches!(
        renderer.create_surface(browser(1), surface(2), config()),
        Err(BrowserError::ResourceLimit {
            resource: "surface count",
            ..
        })
    ));
    renderer
        .register_fixture(
            "https://example.com/heavy",
            HtmlFixture::new("<p>heavy</p>")
                .unwrap()
                .with_network_cost(3, 12, 0),
        )
        .unwrap();
    assert!(matches!(
        renderer.navigate(surface(1), "https://example.com/heavy"),
        Err(BrowserError::NetworkLimit(
            NetworkLimitError::RequestCount { .. }
        ))
    ));
    assert_eq!(renderer.surface(surface(1)).unwrap().current_url, None);
}
