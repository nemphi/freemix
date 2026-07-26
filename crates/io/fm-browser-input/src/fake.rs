//! Deterministic in-memory renderer for policy and orchestration tests.

use std::collections::{BTreeMap, BTreeSet};

use fm_frame::{
    AudioBlock, Channel, ChannelLayout, ClockDomainId, CpuVideoFrame, CpuVideoPayload,
    CpuVideoPlane, MediaTimestamp, MediaTiming, NormalizedDuration, NormalizedTimestamp,
    OriginalTimestamp, PixelFormat, SampleRate, SequenceNumber, TimeBase,
};

use crate::{
    BrowserError, BrowserId, BrowserLimits, BrowserRenderer, BrowserState, ChildLifecycle,
    ChildSupervisor, CookieJar, CrashDisposition, InteractionIntent, InteractionKind,
    InteractionMode, NavigationPolicy, NetworkBudget, NetworkLimits, ProfileId, RenderOutput,
    RestartError, RestartPolicy, SurfaceConfig, SurfaceId, SurfaceSnapshot,
};

const DEFAULT_FRAME_DURATION_NANOS: u64 = 33_333_333;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;

/// A tiny HTML-like fixture. `data-color="#RRGGBB[AA]"` selects a solid
/// frame, while `data-audio="0.0,0.5,-0.5"` supplies a repeating mono waveform.
#[derive(Clone, Debug, PartialEq)]
pub struct HtmlFixture {
    html: String,
    color: Option<[u8; 4]>,
    audio: Vec<f32>,
    frame_duration_nanos: u64,
    sample_rate_hz: u32,
    requests: usize,
    response_bytes: usize,
    redirects: usize,
}

impl HtmlFixture {
    /// Parses deterministic rendering attributes from an HTML-like string.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed color, audio, duration, or sample-rate attributes.
    pub fn new(html: impl Into<String>) -> Result<Self, BrowserError> {
        let html = html.into();
        let color = attribute(&html, "data-color")
            .map(parse_color)
            .transpose()?;
        let audio = attribute(&html, "data-audio")
            .map(parse_audio)
            .transpose()?
            .unwrap_or_default();
        let frame_duration_nanos = attribute(&html, "data-frame-nanos")
            .map(|value| parse_positive(value, "data-frame-nanos"))
            .transpose()?
            .unwrap_or(DEFAULT_FRAME_DURATION_NANOS);
        let sample_rate_hz = attribute(&html, "data-sample-rate")
            .map(|value| parse_positive(value, "data-sample-rate"))
            .transpose()?
            .map_or(DEFAULT_SAMPLE_RATE, |value| {
                u32::try_from(value).unwrap_or(u32::MAX)
            });
        if SampleRate::new(sample_rate_hz).is_none() {
            return Err(BrowserError::InvalidFixture(
                "data-sample-rate must be a positive u32".to_owned(),
            ));
        }
        let response_bytes = html.len();
        Ok(Self {
            html,
            color,
            audio,
            frame_duration_nanos,
            sample_rate_hz,
            requests: 1,
            response_bytes,
            redirects: 0,
        })
    }

    #[must_use]
    pub fn html(&self) -> &str {
        &self.html
    }

    #[must_use]
    pub const fn frame_duration_nanos(&self) -> u64 {
        self.frame_duration_nanos
    }

    #[must_use]
    pub fn with_network_cost(
        mut self,
        requests: usize,
        response_bytes: usize,
        redirects: usize,
    ) -> Self {
        self.requests = requests;
        self.response_bytes = response_bytes;
        self.redirects = redirects;
        self
    }
}

#[derive(Clone, Debug)]
struct BrowserRecord {
    profile_id: ProfileId,
    state: BrowserState,
}

#[derive(Clone, Debug)]
struct SurfaceRecord {
    browser_id: BrowserId,
    config: SurfaceConfig,
    current_url: Option<String>,
    navigation_generation: u64,
    active: bool,
    budget: NetworkBudget,
    interactions: Vec<InteractionIntent>,
}

/// A deterministic fake which performs no DNS, network, HTML, or JavaScript work.
#[derive(Clone, Debug)]
pub struct FakeRenderer {
    policy: NavigationPolicy,
    browser_limits: BrowserLimits,
    network_limits: NetworkLimits,
    supervisor: ChildSupervisor,
    clock_domain: ClockDomainId,
    browsers: BTreeMap<BrowserId, BrowserRecord>,
    surfaces: BTreeMap<SurfaceId, SurfaceRecord>,
    fixtures: BTreeMap<String, HtmlFixture>,
    cookies: CookieJar,
}

impl FakeRenderer {
    #[must_use]
    pub fn new(clock_domain: ClockDomainId) -> Self {
        Self::with_limits(
            clock_domain,
            NavigationPolicy::new(),
            BrowserLimits::default(),
            NetworkLimits::default(),
            RestartPolicy::default(),
        )
    }

    #[must_use]
    pub fn with_limits(
        clock_domain: ClockDomainId,
        policy: NavigationPolicy,
        browser_limits: BrowserLimits,
        network_limits: NetworkLimits,
        restart_policy: RestartPolicy,
    ) -> Self {
        let mut supervisor = ChildSupervisor::new(restart_policy);
        supervisor.start();
        Self {
            policy,
            browser_limits,
            network_limits,
            supervisor,
            clock_domain,
            browsers: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            fixtures: BTreeMap::new(),
            cookies: CookieJar::new(),
        }
    }

    #[must_use]
    pub const fn policy(&self) -> &NavigationPolicy {
        &self.policy
    }

    pub const fn policy_mut(&mut self) -> &mut NavigationPolicy {
        &mut self.policy
    }

    #[must_use]
    pub const fn cookies(&self) -> &CookieJar {
        &self.cookies
    }

    pub const fn cookies_mut(&mut self) -> &mut CookieJar {
        &mut self.cookies
    }

    /// Registers a bounded fixture at an exact URL.
    ///
    /// # Errors
    ///
    /// Returns an error if the fixture source exceeds the HTML byte limit.
    pub fn register_fixture(
        &mut self,
        url: impl Into<String>,
        fixture: HtmlFixture,
    ) -> Result<(), BrowserError> {
        if fixture.html.len() > self.browser_limits.max_html_bytes {
            return Err(crate::contract::resource_limit(
                "fixture HTML bytes",
                fixture.html.len(),
                self.browser_limits.max_html_bytes,
            ));
        }
        self.fixtures.insert(url.into(), fixture);
        Ok(())
    }

    #[must_use]
    pub fn surface(&self, surface_id: SurfaceId) -> Option<SurfaceSnapshot> {
        let surface = self.surfaces.get(&surface_id)?;
        let browser = self.browsers.get(&surface.browser_id)?;
        Some(SurfaceSnapshot {
            browser_id: surface.browser_id,
            profile_id: browser.profile_id,
            config: surface.config.clone(),
            current_url: surface.current_url.clone(),
            navigation_generation: surface.navigation_generation,
            active: surface.active,
        })
    }

    #[must_use]
    pub fn network_budget(&self, surface_id: SurfaceId) -> Option<NetworkBudget> {
        self.surfaces.get(&surface_id).map(|surface| surface.budget)
    }

    pub fn take_interactions(&mut self, surface_id: SurfaceId) -> Vec<InteractionIntent> {
        self.surfaces
            .get_mut(&surface_id)
            .map(|surface| std::mem::take(&mut surface.interactions))
            .unwrap_or_default()
    }

    /// Sets a cookie through a browser's profile partition.
    ///
    /// # Errors
    ///
    /// Returns an unknown-browser or cookie validation error.
    pub fn set_cookie(
        &mut self,
        browser_id: BrowserId,
        host: &str,
        name: &str,
        value: &str,
    ) -> Result<(), BrowserError> {
        let profile_id = self
            .browsers
            .get(&browser_id)
            .ok_or(BrowserError::UnknownBrowser(browser_id))?
            .profile_id;
        self.cookies
            .set(profile_id, host, name, value, self.browser_limits)
            .map_err(BrowserError::Cookie)
    }

    #[must_use]
    pub fn cookie(&self, browser_id: BrowserId, host: &str, name: &str) -> Option<&str> {
        let profile_id = self.browsers.get(&browser_id)?.profile_id;
        self.cookies.get(profile_id, host, name)
    }

    /// Closes a browser and all of its surfaces without affecting its shared profile.
    ///
    /// # Errors
    ///
    /// Returns an unknown-browser error.
    pub fn close_browser(&mut self, browser_id: BrowserId) -> Result<(), BrowserError> {
        let browser = self
            .browsers
            .get_mut(&browser_id)
            .ok_or(BrowserError::UnknownBrowser(browser_id))?;
        browser.state = BrowserState::Closed;
        self.surfaces
            .retain(|_, surface| surface.browser_id != browser_id);
        Ok(())
    }

    pub fn crash_child(&mut self, now_nanos: u64) -> CrashDisposition {
        self.supervisor.crash(now_nanos)
    }

    /// Restarts the renderer child according to its restart policy.
    ///
    /// # Errors
    ///
    /// Returns an error when restart is not pending or quarantine has not expired.
    pub fn restart_child(&mut self, now_nanos: u64) -> Result<(), RestartError> {
        self.supervisor.restart(now_nanos)
    }

    fn ensure_running(&self) -> Result<(), BrowserError> {
        let state = self.supervisor.state();
        if state == ChildLifecycle::Running {
            Ok(())
        } else {
            Err(BrowserError::ChildUnavailable(state))
        }
    }

    fn timing(
        &self,
        pts_nanos: i64,
        duration_nanos: u64,
        sequence: u64,
    ) -> Result<MediaTiming, BrowserError> {
        let base = TimeBase::new(1, 1_000_000_000).expect("a nanosecond time base is valid");
        let duration = NormalizedDuration::from_nanos(duration_nanos)
            .map_err(|error| BrowserError::Media(error.to_string()))?;
        MediaTiming::new(
            OriginalTimestamp::new(MediaTimestamp::new(pts_nanos), base),
            NormalizedTimestamp::from_nanos(pts_nanos),
            duration,
            self.clock_domain,
            SequenceNumber::new(sequence),
        )
        .map_err(|error| BrowserError::Media(error.to_string()))
    }
}

impl BrowserRenderer for FakeRenderer {
    fn child_lifecycle(&self) -> ChildLifecycle {
        self.supervisor.state()
    }

    fn create_browser(
        &mut self,
        browser_id: BrowserId,
        profile_id: ProfileId,
    ) -> Result<(), BrowserError> {
        self.ensure_running()?;
        if self.browsers.contains_key(&browser_id) {
            return Err(BrowserError::DuplicateBrowser(browser_id));
        }
        if self.browsers.len() >= self.browser_limits.max_browsers {
            return Err(crate::contract::resource_limit(
                "browser count",
                self.browsers.len().saturating_add(1),
                self.browser_limits.max_browsers,
            ));
        }
        let profiles: BTreeSet<_> = self
            .browsers
            .values()
            .filter(|browser| browser.state == BrowserState::Active)
            .map(|browser| browser.profile_id)
            .collect();
        if !profiles.contains(&profile_id) && profiles.len() >= self.browser_limits.max_profiles {
            return Err(crate::contract::resource_limit(
                "profile count",
                profiles.len().saturating_add(1),
                self.browser_limits.max_profiles,
            ));
        }
        self.browsers.insert(
            browser_id,
            BrowserRecord {
                profile_id,
                state: BrowserState::Active,
            },
        );
        Ok(())
    }

    fn create_surface(
        &mut self,
        browser_id: BrowserId,
        surface_id: SurfaceId,
        config: SurfaceConfig,
    ) -> Result<(), BrowserError> {
        self.ensure_running()?;
        let browser = self
            .browsers
            .get(&browser_id)
            .ok_or(BrowserError::UnknownBrowser(browser_id))?;
        if browser.state != BrowserState::Active {
            return Err(BrowserError::UnknownBrowser(browser_id));
        }
        if self.surfaces.contains_key(&surface_id) {
            return Err(BrowserError::DuplicateSurface(surface_id));
        }
        let count = self
            .surfaces
            .values()
            .filter(|surface| surface.browser_id == browser_id)
            .count();
        if count >= self.browser_limits.max_surfaces_per_browser {
            return Err(crate::contract::resource_limit(
                "surface count",
                count.saturating_add(1),
                self.browser_limits.max_surfaces_per_browser,
            ));
        }
        config.validate(self.browser_limits)?;
        self.surfaces.insert(
            surface_id,
            SurfaceRecord {
                browser_id,
                config,
                current_url: None,
                navigation_generation: 0,
                active: false,
                budget: NetworkBudget::default(),
                interactions: Vec::new(),
            },
        );
        Ok(())
    }

    fn navigate(&mut self, surface_id: SurfaceId, url: &str) -> Result<(), BrowserError> {
        self.ensure_running()?;
        self.policy.check(url)?;
        let fixture = self
            .fixtures
            .get(url)
            .ok_or_else(|| BrowserError::UnknownFixture(url.to_owned()))?;
        let mut budget = NetworkBudget::default();
        budget.consume(
            self.network_limits,
            fixture.requests,
            fixture.response_bytes,
            fixture.redirects,
        )?;
        let surface = self
            .surfaces
            .get_mut(&surface_id)
            .ok_or(BrowserError::UnknownSurface(surface_id))?;
        surface.current_url = Some(url.to_owned());
        surface.navigation_generation = surface.navigation_generation.saturating_add(1);
        surface.budget = budget;
        Ok(())
    }

    fn activate(&mut self, surface_id: SurfaceId) -> Result<(), BrowserError> {
        self.ensure_running()?;
        let surface = self
            .surfaces
            .get_mut(&surface_id)
            .ok_or(BrowserError::UnknownSurface(surface_id))?;
        surface.active = true;
        if surface.config.refresh_on_activate && surface.current_url.is_some() {
            surface.navigation_generation = surface.navigation_generation.saturating_add(1);
        }
        Ok(())
    }

    fn interact(&mut self, intent: InteractionIntent) -> Result<(), BrowserError> {
        self.ensure_running()?;
        let surface = self
            .surfaces
            .get_mut(&intent.surface_id)
            .ok_or(BrowserError::UnknownSurface(intent.surface_id))?;
        if surface.config.interaction != InteractionMode::Enabled {
            return Err(BrowserError::InteractionDisabled(intent.surface_id));
        }
        let point = match &intent.kind {
            InteractionKind::PointerMove { x, y } | InteractionKind::PointerButton { x, y, .. } => {
                Some((*x, *y))
            }
            _ => None,
        };
        if let Some((x, y)) = point
            && (x >= surface.config.dimensions.width() || y >= surface.config.dimensions.height())
        {
            return Err(BrowserError::InteractionOutOfBounds {
                surface_id: intent.surface_id,
                x,
                y,
            });
        }
        let text_bytes = match &intent.kind {
            InteractionKind::Text(text) | InteractionKind::Key { code: text, .. } => text.len(),
            _ => 0,
        };
        if text_bytes > self.browser_limits.max_interaction_text_bytes {
            return Err(crate::contract::resource_limit(
                "interaction text bytes",
                text_bytes,
                self.browser_limits.max_interaction_text_bytes,
            ));
        }
        surface.interactions.push(intent);
        Ok(())
    }

    fn render(
        &mut self,
        surface_id: SurfaceId,
        pts_nanos: i64,
    ) -> Result<RenderOutput, BrowserError> {
        self.ensure_running()?;
        let surface = self
            .surfaces
            .get(&surface_id)
            .ok_or(BrowserError::UnknownSurface(surface_id))?;
        let url = surface
            .current_url
            .as_ref()
            .ok_or_else(|| BrowserError::UnknownFixture("surface has not navigated".to_owned()))?;
        let fixture = self
            .fixtures
            .get(url)
            .ok_or_else(|| BrowserError::UnknownFixture(url.clone()))?;
        let duration = fixture.frame_duration_nanos;
        let duration_i64 = i64::try_from(duration).unwrap_or(i64::MAX);
        let sequence = u64::try_from(pts_nanos.div_euclid(duration_i64)).unwrap_or(0);
        let timing = self.timing(pts_nanos, duration, sequence)?;

        let color = css_background(surface.config.custom_css())
            .or(fixture.color)
            .unwrap_or_else(|| generated_color(fixture, &surface.config));
        let pixel_count = (surface.config.dimensions.width() as usize)
            .saturating_mul(surface.config.dimensions.height() as usize);
        let mut bytes = Vec::with_capacity(pixel_count.saturating_mul(4));
        for _ in 0..pixel_count {
            bytes.extend_from_slice(&color);
        }
        let stride = (surface.config.dimensions.width() as usize).saturating_mul(4);
        let plane = CpuVideoPlane::new(stride, bytes)
            .map_err(|error| BrowserError::Media(error.to_string()))?;
        let payload =
            CpuVideoPayload::new(PixelFormat::Rgba8, surface.config.dimensions, vec![plane])
                .map_err(|error| BrowserError::Media(error.to_string()))?;
        let video = CpuVideoFrame::new(timing, payload);

        let audio = if fixture.audio.is_empty() {
            None
        } else {
            let sample_count_u128 = u128::from(duration)
                .saturating_mul(u128::from(fixture.sample_rate_hz))
                / 1_000_000_000;
            let sample_count = usize::try_from(sample_count_u128.max(1)).unwrap_or(usize::MAX);
            if sample_count > self.browser_limits.max_audio_samples_per_channel {
                return Err(crate::contract::resource_limit(
                    "audio samples per channel",
                    sample_count,
                    self.browser_limits.max_audio_samples_per_channel,
                ));
            }
            let offset = audio_offset(pts_nanos, fixture.sample_rate_hz, fixture.audio.len());
            let samples = (0..sample_count)
                .map(|index| fixture.audio[(offset + index) % fixture.audio.len()])
                .collect();
            let sample_rate = SampleRate::new(fixture.sample_rate_hz)
                .expect("fixture construction validates the sample rate");
            let layout =
                ChannelLayout::new(vec![Channel::Mono]).expect("a mono channel layout is valid");
            Some(
                AudioBlock::new(timing, sample_rate, layout, vec![samples])
                    .map_err(|error| BrowserError::Media(error.to_string()))?,
            )
        };

        Ok(RenderOutput {
            video,
            audio,
            navigation_generation: surface.navigation_generation,
        })
    }
}

fn attribute<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let start = source.find(name)? + name.len();
    let remainder = source[start..].trim_start();
    let remainder = remainder.strip_prefix('=')?.trim_start();
    let quote = remainder.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let value = &remainder[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some(&value[..end])
}

fn parse_color(value: &str) -> Result<[u8; 4], BrowserError> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    if !matches!(digits.len(), 6 | 8) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BrowserError::InvalidFixture(
            "data-color must be #RRGGBB or #RRGGBBAA".to_owned(),
        ));
    }
    let component = |start| u8::from_str_radix(&digits[start..start + 2], 16);
    Ok([
        component(0).expect("hex was validated"),
        component(2).expect("hex was validated"),
        component(4).expect("hex was validated"),
        if digits.len() == 8 {
            component(6).expect("hex was validated")
        } else {
            255
        },
    ])
}

fn parse_audio(value: &str) -> Result<Vec<f32>, BrowserError> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|sample| {
            let sample = sample.trim().parse::<f32>().map_err(|_| {
                BrowserError::InvalidFixture("data-audio contains a non-number".to_owned())
            })?;
            if sample.is_finite() && (-1.0..=1.0).contains(&sample) {
                Ok(sample)
            } else {
                Err(BrowserError::InvalidFixture(
                    "data-audio samples must be finite and between -1 and 1".to_owned(),
                ))
            }
        })
        .collect()
}

fn parse_positive(value: &str, name: &str) -> Result<u64, BrowserError> {
    let value = value
        .parse::<u64>()
        .map_err(|_| BrowserError::InvalidFixture(format!("{name} must be a positive integer")))?;
    if value == 0 {
        return Err(BrowserError::InvalidFixture(format!(
            "{name} must be a positive integer"
        )));
    }
    Ok(value)
}

fn css_background(css: Option<&str>) -> Option<[u8; 4]> {
    let css = css?;
    let background = css
        .find("background-color")
        .or_else(|| css.find("background"))?;
    let hash = css[background..].find('#')? + background;
    let digits: String = css[hash + 1..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .collect();
    parse_color(&digits).ok()
}

fn generated_color(fixture: &HtmlFixture, config: &SurfaceConfig) -> [u8; 4] {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in fixture
        .html
        .bytes()
        .chain(config.custom_css().unwrap_or_default().bytes())
        .chain(config.zoom.percent().to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let bytes = hash.to_le_bytes();
    [
        bytes[0],
        bytes[1],
        bytes[2],
        if config.transparent_background {
            0
        } else {
            255
        },
    ]
}

fn audio_offset(pts_nanos: i64, sample_rate_hz: u32, pattern_len: usize) -> usize {
    let sample = i128::from(pts_nanos)
        .saturating_mul(i128::from(sample_rate_hz))
        .div_euclid(1_000_000_000);
    let pattern_len = i128::try_from(pattern_len).unwrap_or(i128::MAX);
    usize::try_from(sample.rem_euclid(pattern_len)).unwrap_or(0)
}
