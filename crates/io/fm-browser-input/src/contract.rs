use core::{fmt, num::NonZeroU128};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use fm_frame::{AudioBlock, CpuVideoFrame};

macro_rules! stable_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU128);

        impl $name {
            #[must_use]
            pub const fn new(value: NonZeroU128) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(self) -> NonZeroU128 {
                self.0
            }
        }

        impl From<NonZeroU128> for $name {
            fn from(value: NonZeroU128) -> Self {
                Self::new(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

stable_id!(BrowserId);
stable_id!(ProfileId);
stable_id!(SurfaceId);

/// A capability which relaxes one default-deny navigation rule.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NavigationGrant {
    File,
    PrivateNetwork,
    Scheme(String),
}

/// Why a navigation was denied before reaching a renderer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavigationBlockReason {
    MalformedUrl,
    UnsupportedScheme { scheme: String },
    FileAccess,
    PrivateNetwork { host: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationBlocked {
    pub url: String,
    pub reason: NavigationBlockReason,
}

impl fmt::Display for NavigationBlocked {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "navigation to {} blocked: ", self.url)?;
        match &self.reason {
            NavigationBlockReason::MalformedUrl => formatter.write_str("malformed URL"),
            NavigationBlockReason::UnsupportedScheme { scheme } => {
                write!(formatter, "unsupported scheme {scheme}")
            }
            NavigationBlockReason::FileAccess => formatter.write_str("file access is not granted"),
            NavigationBlockReason::PrivateNetwork { host } => {
                write!(formatter, "private-network access to {host} is not granted")
            }
        }
    }
}

impl std::error::Error for NavigationBlocked {}

/// Default-deny navigation policy. HTTP and HTTPS are the only schemes enabled
/// by default, and private hosts remain denied for those schemes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationPolicy {
    grants: BTreeSet<NavigationGrant>,
}

impl Default for NavigationPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl NavigationPolicy {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            grants: BTreeSet::new(),
        }
    }

    pub fn grant(&mut self, grant: NavigationGrant) {
        let grant = match grant {
            NavigationGrant::Scheme(scheme) => NavigationGrant::Scheme(scheme.to_ascii_lowercase()),
            other => other,
        };
        self.grants.insert(grant);
    }

    pub fn revoke(&mut self, grant: &NavigationGrant) -> bool {
        self.grants.remove(grant)
    }

    #[must_use]
    pub fn grants(&self) -> &BTreeSet<NavigationGrant> {
        &self.grants
    }

    /// Applies scheme, file, and private-network checks to a URL.
    ///
    /// # Errors
    ///
    /// Returns the first policy rule which denies the URL.
    pub fn check(&self, url: &str) -> Result<(), NavigationBlocked> {
        let (scheme, remainder) =
            split_scheme(url).ok_or_else(|| blocked(url, NavigationBlockReason::MalformedUrl))?;
        let scheme = scheme.to_ascii_lowercase();

        if scheme == "file" {
            if !self.grants.contains(&NavigationGrant::File) {
                return Err(blocked(url, NavigationBlockReason::FileAccess));
            }
            return Ok(());
        }

        let built_in = matches!(scheme.as_str(), "http" | "https");
        if !built_in
            && !self
                .grants
                .contains(&NavigationGrant::Scheme(scheme.clone()))
        {
            return Err(blocked(
                url,
                NavigationBlockReason::UnsupportedScheme { scheme },
            ));
        }

        let host = authority_host(remainder);
        if built_in && host.is_none() {
            return Err(blocked(url, NavigationBlockReason::MalformedUrl));
        }
        if let Some(host) = host
            && is_private_host(&host)
            && !self.grants.contains(&NavigationGrant::PrivateNetwork)
        {
            return Err(blocked(url, NavigationBlockReason::PrivateNetwork { host }));
        }
        Ok(())
    }

    /// Rechecks a policy-approved URL against all addresses returned by DNS.
    /// Adapters should call this after every resolution, including redirects,
    /// and before opening a socket.
    ///
    /// # Errors
    ///
    /// Returns a URL-policy or private-network denial.
    pub fn check_resolved(
        &self,
        url: &str,
        resolved_addresses: &[IpAddr],
    ) -> Result<(), NavigationBlocked> {
        self.check(url)?;
        if self.grants.contains(&NavigationGrant::PrivateNetwork) {
            return Ok(());
        }
        if let Some(address) = resolved_addresses
            .iter()
            .copied()
            .find(|address| is_private_ip(*address))
        {
            return Err(blocked(
                url,
                NavigationBlockReason::PrivateNetwork {
                    host: address.to_string(),
                },
            ));
        }
        Ok(())
    }
}

fn blocked(url: &str, reason: NavigationBlockReason) -> NavigationBlocked {
    NavigationBlocked {
        url: url.to_owned(),
        reason,
    }
}

fn split_scheme(url: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = url.split_once(':')?;
    if scheme.is_empty()
        || !scheme.as_bytes()[0].is_ascii_alphabetic()
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return None;
    }
    Some((scheme, remainder))
}

fn authority_host(remainder: &str) -> Option<String> {
    let authority = remainder
        .strip_prefix("//")?
        .split(['/', '?', '#'])
        .next()?;
    if authority.is_empty() || authority.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, value)| value);
    let host = if let Some(value) = host_port.strip_prefix('[') {
        let end = value.find(']')?;
        if !value[end + 1..].is_empty() && !value[end + 1..].starts_with(':') {
            return None;
        }
        &value[..end]
    } else {
        host_port
            .split_once(':')
            .map_or(host_port, |(host, _)| host)
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty() && !host.contains('%')).then_some(host)
}

fn is_private_host(host: &str) -> bool {
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.strip_suffix(".local").is_some()
        || !host.contains('.')
    {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(ip);
    }
    parse_whatwg_ipv4(host).is_some_and(is_private_ipv4)
}

fn is_private_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_private_ipv4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_private_ipv4(mapped);
            }
            is_private_ipv6(address)
        }
    }
}

fn is_private_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, _, _] = address.octets();
    a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && (b == 0 || b == 168))
        || (a == 198 && (b == 18 || b == 19))
        || a >= 224
}

fn is_private_ipv6(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    address.is_unspecified()
        || address.is_loopback()
        || first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
        || first & 0xff00 == 0xff00
}

// Browsers accept legacy integer, hexadecimal, octal, and shortened IPv4 forms.
fn parse_whatwg_ipv4(host: &str) -> Option<Ipv4Addr> {
    let components: Vec<_> = host.trim_end_matches('.').split('.').collect();
    if components.is_empty() || components.len() > 4 {
        return None;
    }
    let values: Vec<u64> = components
        .iter()
        .map(|component| parse_ipv4_number(component))
        .collect::<Option<_>>()?;
    let last_bits = 8 * (5 - values.len());
    if values[..values.len() - 1].iter().any(|value| *value > 255)
        || values[values.len() - 1] >= (1_u64 << last_bits)
    {
        return None;
    }
    let mut result = 0_u64;
    for value in &values[..values.len() - 1] {
        result = (result << 8) | value;
    }
    result = (result << last_bits) | values[values.len() - 1];
    u32::try_from(result).ok().map(Ipv4Addr::from)
}

fn parse_ipv4_number(component: &str) -> Option<u64> {
    if component.is_empty() {
        return None;
    }
    let (digits, radix) = if let Some(digits) = component
        .strip_prefix("0x")
        .or_else(|| component.strip_prefix("0X"))
    {
        (digits, 16)
    } else if component.len() > 1 && component.starts_with('0') {
        (&component[1..], 8)
    } else {
        (component, 10)
    };
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, radix).ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BrowserLimits {
    pub max_browsers: usize,
    pub max_profiles: usize,
    pub max_surfaces_per_browser: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub max_frame_bytes: usize,
    pub max_css_bytes: usize,
    pub max_html_bytes: usize,
    pub max_interaction_text_bytes: usize,
    pub max_audio_samples_per_channel: usize,
    pub max_cookies_per_profile: usize,
    pub max_cookie_bytes: usize,
}

impl Default for BrowserLimits {
    fn default() -> Self {
        Self {
            max_browsers: 8,
            max_profiles: 8,
            max_surfaces_per_browser: 16,
            max_width: 4096,
            max_height: 4096,
            max_frame_bytes: 64 * 1024 * 1024,
            max_css_bytes: 64 * 1024,
            max_html_bytes: 1024 * 1024,
            max_interaction_text_bytes: 16 * 1024,
            max_audio_samples_per_channel: 16_384,
            max_cookies_per_profile: 1024,
            max_cookie_bytes: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetworkLimits {
    pub max_requests_per_navigation: usize,
    pub max_response_bytes: usize,
    pub max_redirects: usize,
}

impl Default for NetworkLimits {
    fn default() -> Self {
        Self {
            max_requests_per_navigation: 64,
            max_response_bytes: 16 * 1024 * 1024,
            max_redirects: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetworkLimitError {
    RequestCount { requested: usize, maximum: usize },
    ResponseBytes { requested: usize, maximum: usize },
    RedirectCount { requested: usize, maximum: usize },
}

impl fmt::Display for NetworkLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RequestCount { requested, maximum } => {
                write!(formatter, "request count {requested} exceeds {maximum}")
            }
            Self::ResponseBytes { requested, maximum } => {
                write!(formatter, "response bytes {requested} exceeds {maximum}")
            }
            Self::RedirectCount { requested, maximum } => {
                write!(formatter, "redirect count {requested} exceeds {maximum}")
            }
        }
    }
}

impl std::error::Error for NetworkLimitError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NetworkBudget {
    requests: usize,
    response_bytes: usize,
    redirects: usize,
}

impl NetworkBudget {
    #[must_use]
    pub const fn requests(self) -> usize {
        self.requests
    }

    #[must_use]
    pub const fn response_bytes(self) -> usize {
        self.response_bytes
    }

    #[must_use]
    pub const fn redirects(self) -> usize {
        self.redirects
    }

    /// Accounts for navigation work without partially consuming the budget.
    ///
    /// # Errors
    ///
    /// Returns the first exceeded network limit.
    pub fn consume(
        &mut self,
        limits: NetworkLimits,
        requests: usize,
        response_bytes: usize,
        redirects: usize,
    ) -> Result<(), NetworkLimitError> {
        let requests = self.requests.saturating_add(requests);
        let response_bytes = self.response_bytes.saturating_add(response_bytes);
        let redirects = self.redirects.saturating_add(redirects);
        if requests > limits.max_requests_per_navigation {
            return Err(NetworkLimitError::RequestCount {
                requested: requests,
                maximum: limits.max_requests_per_navigation,
            });
        }
        if response_bytes > limits.max_response_bytes {
            return Err(NetworkLimitError::ResponseBytes {
                requested: response_bytes,
                maximum: limits.max_response_bytes,
            });
        }
        if redirects > limits.max_redirects {
            return Err(NetworkLimitError::RedirectCount {
                requested: redirects,
                maximum: limits.max_redirects,
            });
        }
        self.requests = requests;
        self.response_bytes = response_bytes;
        self.redirects = redirects;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Zoom(u16);

impl Zoom {
    pub const MIN_PERCENT: u16 = 25;
    pub const MAX_PERCENT: u16 = 500;
    pub const ONE: Self = Self(100);

    /// Creates a bounded zoom percentage.
    ///
    /// # Errors
    ///
    /// Returns [`ZoomError`] outside 25 through 500 percent.
    pub const fn new(percent: u16) -> Result<Self, ZoomError> {
        if percent < Self::MIN_PERCENT || percent > Self::MAX_PERCENT {
            Err(ZoomError { percent })
        } else {
            Ok(Self(percent))
        }
    }

    #[must_use]
    pub const fn percent(self) -> u16 {
        self.0
    }
}

impl Default for Zoom {
    fn default() -> Self {
        Self::ONE
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ZoomError {
    pub percent: u16,
}

impl fmt::Display for ZoomError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "zoom {}% is outside 25% through 500%",
            self.percent
        )
    }
}

impl std::error::Error for ZoomError {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InteractionMode {
    Disabled,
    Enabled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceConfig {
    pub dimensions: fm_frame::VideoDimensions,
    pub transparent_background: bool,
    pub zoom: Zoom,
    pub refresh_on_activate: bool,
    pub interaction: InteractionMode,
    custom_css: Option<String>,
}

impl SurfaceConfig {
    #[must_use]
    pub const fn new(dimensions: fm_frame::VideoDimensions) -> Self {
        Self {
            dimensions,
            transparent_background: false,
            zoom: Zoom::ONE,
            refresh_on_activate: false,
            interaction: InteractionMode::Disabled,
            custom_css: None,
        }
    }

    #[must_use]
    pub fn custom_css(&self) -> Option<&str> {
        self.custom_css.as_deref()
    }

    /// Sets CSS after enforcing the configured byte bound.
    ///
    /// # Errors
    ///
    /// Returns a resource-limit error when the CSS is too large.
    pub fn set_custom_css(
        &mut self,
        css: Option<String>,
        limits: BrowserLimits,
    ) -> Result<(), BrowserError> {
        if let Some(css) = &css
            && css.len() > limits.max_css_bytes
        {
            return Err(BrowserError::ResourceLimit {
                resource: "custom CSS bytes",
                requested: css.len(),
                maximum: limits.max_css_bytes,
            });
        }
        self.custom_css = css;
        Ok(())
    }

    pub(crate) fn validate(&self, limits: BrowserLimits) -> Result<(), BrowserError> {
        if self.dimensions.width() > limits.max_width {
            return Err(resource_limit(
                "surface width",
                self.dimensions.width() as usize,
                limits.max_width as usize,
            ));
        }
        if self.dimensions.height() > limits.max_height {
            return Err(resource_limit(
                "surface height",
                self.dimensions.height() as usize,
                limits.max_height as usize,
            ));
        }
        let bytes = (self.dimensions.width() as usize)
            .saturating_mul(self.dimensions.height() as usize)
            .saturating_mul(4);
        if bytes > limits.max_frame_bytes {
            return Err(resource_limit(
                "video frame bytes",
                bytes,
                limits.max_frame_bytes,
            ));
        }
        if let Some(css) = &self.custom_css
            && css.len() > limits.max_css_bytes
        {
            return Err(resource_limit(
                "custom CSS bytes",
                css.len(),
                limits.max_css_bytes,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MouseButton {
    Primary,
    Auxiliary,
    Secondary,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeyState {
    Pressed,
    Released,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InteractionKind {
    PointerMove {
        x: u32,
        y: u32,
    },
    PointerButton {
        x: u32,
        y: u32,
        button: MouseButton,
        state: KeyState,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        code: String,
        state: KeyState,
    },
    Text(String),
    Focus(bool),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionIntent {
    pub surface_id: SurfaceId,
    pub kind: InteractionKind,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ChildLifecycle {
    Stopped,
    Running,
    Crashed,
    RestartPending,
    Quarantined { until_nanos: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    pub max_restarts: usize,
    pub window_nanos: u64,
    pub quarantine_nanos: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            window_nanos: 60_000_000_000,
            quarantine_nanos: 300_000_000_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashDisposition {
    RestartPending,
    Quarantined { until_nanos: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartError {
    NotPending { state: ChildLifecycle },
    Quarantined { until_nanos: u64 },
}

impl fmt::Display for RestartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPending { state } => write!(formatter, "restart is not pending in {state:?}"),
            Self::Quarantined { until_nanos } => {
                write!(formatter, "child is quarantined until {until_nanos} ns")
            }
        }
    }
}

impl std::error::Error for RestartError {}

#[derive(Clone, Debug)]
pub struct ChildSupervisor {
    policy: RestartPolicy,
    state: ChildLifecycle,
    crashes: VecDeque<u64>,
}

impl ChildSupervisor {
    #[must_use]
    pub const fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            state: ChildLifecycle::Stopped,
            crashes: VecDeque::new(),
        }
    }

    #[must_use]
    pub const fn state(&self) -> ChildLifecycle {
        self.state
    }

    pub fn start(&mut self) {
        self.state = ChildLifecycle::Running;
    }

    pub fn stop(&mut self) {
        self.state = ChildLifecycle::Stopped;
        self.crashes.clear();
    }

    pub fn crash(&mut self, now_nanos: u64) -> CrashDisposition {
        self.state = ChildLifecycle::Crashed;
        let earliest = now_nanos.saturating_sub(self.policy.window_nanos);
        while self.crashes.front().is_some_and(|time| *time < earliest) {
            self.crashes.pop_front();
        }
        self.crashes.push_back(now_nanos);
        if self.crashes.len() > self.policy.max_restarts {
            let until_nanos = now_nanos.saturating_add(self.policy.quarantine_nanos);
            self.state = ChildLifecycle::Quarantined { until_nanos };
            CrashDisposition::Quarantined { until_nanos }
        } else {
            self.state = ChildLifecycle::RestartPending;
            CrashDisposition::RestartPending
        }
    }

    /// Restarts a pending child, or releases an expired quarantine and restarts it.
    ///
    /// # Errors
    ///
    /// Returns an error if no restart is pending or quarantine has not expired.
    pub fn restart(&mut self, now_nanos: u64) -> Result<(), RestartError> {
        match self.state {
            ChildLifecycle::RestartPending => {
                self.state = ChildLifecycle::Running;
                Ok(())
            }
            ChildLifecycle::Quarantined { until_nanos } if now_nanos >= until_nanos => {
                self.crashes.clear();
                self.state = ChildLifecycle::Running;
                Ok(())
            }
            ChildLifecycle::Quarantined { until_nanos } => {
                Err(RestartError::Quarantined { until_nanos })
            }
            state => Err(RestartError::NotPending { state }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CookieError {
    InvalidName,
    TooLarge { actual: usize, maximum: usize },
    ProfileFull { maximum: usize },
}

impl fmt::Display for CookieError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => formatter.write_str("cookie name is invalid"),
            Self::TooLarge { actual, maximum } => {
                write!(formatter, "cookie size {actual} exceeds {maximum}")
            }
            Self::ProfileFull { maximum } => {
                write!(formatter, "profile cookie count exceeds {maximum}")
            }
        }
    }
}

impl std::error::Error for CookieError {}

#[derive(Clone, Debug, Default)]
pub struct CookieJar {
    values: BTreeMap<(ProfileId, String, String), String>,
}

impl CookieJar {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    /// Stores a cookie in exactly one profile and host partition.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed or over-limit cookies.
    pub fn set(
        &mut self,
        profile_id: ProfileId,
        host: &str,
        name: &str,
        value: &str,
        limits: BrowserLimits,
    ) -> Result<(), CookieError> {
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'=' | b';' | b','))
        {
            return Err(CookieError::InvalidName);
        }
        let actual = name.len().saturating_add(value.len());
        if actual > limits.max_cookie_bytes {
            return Err(CookieError::TooLarge {
                actual,
                maximum: limits.max_cookie_bytes,
            });
        }
        let key = (
            profile_id,
            host.trim_end_matches('.').to_ascii_lowercase(),
            name.to_owned(),
        );
        let is_new = !self.values.contains_key(&key);
        if is_new
            && self
                .values
                .keys()
                .filter(|(profile, _, _)| *profile == profile_id)
                .count()
                >= limits.max_cookies_per_profile
        {
            return Err(CookieError::ProfileFull {
                maximum: limits.max_cookies_per_profile,
            });
        }
        self.values.insert(key, value.to_owned());
        Ok(())
    }

    #[must_use]
    pub fn get(&self, profile_id: ProfileId, host: &str, name: &str) -> Option<&str> {
        self.values
            .get(&(
                profile_id,
                host.trim_end_matches('.').to_ascii_lowercase(),
                name.to_owned(),
            ))
            .map(String::as_str)
    }

    pub fn clear_profile(&mut self, profile_id: ProfileId) {
        self.values
            .retain(|(profile, _, _), _| *profile != profile_id);
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BrowserState {
    Active,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceSnapshot {
    pub browser_id: BrowserId,
    pub profile_id: ProfileId,
    pub config: SurfaceConfig,
    pub current_url: Option<String>,
    pub navigation_generation: u64,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderOutput {
    pub video: CpuVideoFrame,
    pub audio: Option<AudioBlock>,
    pub navigation_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BrowserError {
    DuplicateBrowser(BrowserId),
    DuplicateSurface(SurfaceId),
    UnknownBrowser(BrowserId),
    UnknownSurface(SurfaceId),
    UnknownFixture(String),
    Navigation(NavigationBlocked),
    InteractionDisabled(SurfaceId),
    InteractionOutOfBounds {
        surface_id: SurfaceId,
        x: u32,
        y: u32,
    },
    ChildUnavailable(ChildLifecycle),
    ResourceLimit {
        resource: &'static str,
        requested: usize,
        maximum: usize,
    },
    NetworkLimit(NetworkLimitError),
    Cookie(CookieError),
    InvalidFixture(String),
    Media(String),
}

impl fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBrowser(id) => write!(formatter, "browser {id} already exists"),
            Self::DuplicateSurface(id) => write!(formatter, "surface {id} already exists"),
            Self::UnknownBrowser(id) => write!(formatter, "browser {id} does not exist"),
            Self::UnknownSurface(id) => write!(formatter, "surface {id} does not exist"),
            Self::UnknownFixture(url) => write!(formatter, "no fake fixture registered for {url}"),
            Self::Navigation(error) => error.fmt(formatter),
            Self::InteractionDisabled(id) => {
                write!(formatter, "interaction is disabled for surface {id}")
            }
            Self::InteractionOutOfBounds { surface_id, x, y } => {
                write!(
                    formatter,
                    "interaction ({x}, {y}) is outside surface {surface_id}"
                )
            }
            Self::ChildUnavailable(state) => write!(formatter, "renderer child is {state:?}"),
            Self::ResourceLimit {
                resource,
                requested,
                maximum,
            } => write!(formatter, "{resource} {requested} exceeds {maximum}"),
            Self::NetworkLimit(error) => error.fmt(formatter),
            Self::Cookie(error) => error.fmt(formatter),
            Self::InvalidFixture(detail) | Self::Media(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for BrowserError {}

impl From<NavigationBlocked> for BrowserError {
    fn from(value: NavigationBlocked) -> Self {
        Self::Navigation(value)
    }
}

impl From<NetworkLimitError> for BrowserError {
    fn from(value: NetworkLimitError) -> Self {
        Self::NetworkLimit(value)
    }
}

pub(crate) fn resource_limit(
    resource: &'static str,
    requested: usize,
    maximum: usize,
) -> BrowserError {
    BrowserError::ResourceLimit {
        resource,
        requested,
        maximum,
    }
}

/// Portable browser-input operations. Implementations must preserve profile
/// partitioning and apply navigation checks before network access.
pub trait BrowserRenderer {
    fn child_lifecycle(&self) -> ChildLifecycle;

    /// Creates an isolated browser child associated with a cookie profile.
    ///
    /// # Errors
    ///
    /// Returns a duplicate-ID, child-state, or resource-limit error.
    fn create_browser(
        &mut self,
        browser_id: BrowserId,
        profile_id: ProfileId,
    ) -> Result<(), BrowserError>;

    /// Creates a bounded offscreen surface.
    ///
    /// # Errors
    ///
    /// Returns a browser, duplicate-ID, child-state, or resource-limit error.
    fn create_surface(
        &mut self,
        browser_id: BrowserId,
        surface_id: SurfaceId,
        config: SurfaceConfig,
    ) -> Result<(), BrowserError>;

    /// Applies policy and navigates one surface.
    ///
    /// # Errors
    ///
    /// Returns a policy, fixture, network-limit, state, or identifier error.
    fn navigate(&mut self, surface_id: SurfaceId, url: &str) -> Result<(), BrowserError>;

    /// Activates a surface, refreshing it when configured to do so.
    ///
    /// # Errors
    ///
    /// Returns a state or identifier error.
    fn activate(&mut self, surface_id: SurfaceId) -> Result<(), BrowserError>;

    /// Delivers a validated interaction intent.
    ///
    /// # Errors
    ///
    /// Returns a state, disabled-interaction, bounds, or identifier error.
    fn interact(&mut self, intent: InteractionIntent) -> Result<(), BrowserError>;

    /// Produces deterministic timed video and optional audio at a presentation time.
    ///
    /// # Errors
    ///
    /// Returns a state, media-construction, fixture, or identifier error.
    fn render(
        &mut self,
        surface_id: SurfaceId,
        pts_nanos: i64,
    ) -> Result<RenderOutput, BrowserError>;
}
