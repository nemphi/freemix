//! Transport-free models for the `FreeMix` semantic web control surface.

use std::fmt;
use std::net::Ipv6Addr;

use fm_client::ClientConfig;
use fm_protocol::{ClientType, FADE_TO_BLACK_PROTOCOL_VERSION, ProtocolVersion, Role};
use fm_types::ProjectId;

mod fade_to_black;
mod manual_transition;
mod transition;

pub use fade_to_black::{
    FadeToBlackControl, FadeToBlackModel, FadeToBlackPresentation, FadeToBlackProjection,
};
pub use manual_transition::{
    ManualTransitionControl, ManualTransitionModel, ManualTransitionMotion,
    ManualTransitionPresentation, ManualTransitionProjection,
};
pub use transition::{TransitionControl, TransitionControlState, TransitionControls};

/// Protocol versions implemented by this control surface.
pub const SUPPORTED_PROTOCOL_VERSIONS: [ProtocolVersion; 1] = [FADE_TO_BLACK_PROTOCOL_VERSION];

/// A semantic panel that can appear in a role-scoped route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Panel {
    Program,
    Preview,
    Transition,
    Graphics,
    Audio,
    Replay,
    Administration,
}

impl Panel {
    /// Stable route segment for this panel.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Program => "program",
            Self::Preview => "preview",
            Self::Transition => "transition",
            Self::Graphics => "graphics",
            Self::Audio => "audio",
            Self::Replay => "replay",
            Self::Administration => "administration",
        }
    }

    /// Screen-reader label for the panel landmark.
    #[must_use]
    pub const fn accessibility_label(self) -> &'static str {
        match self {
            Self::Program => "Program output",
            Self::Preview => "Preview output",
            Self::Transition => "Transition controls",
            Self::Graphics => "Graphics controls",
            Self::Audio => "Audio controls",
            Self::Replay => "Replay controls",
            Self::Administration => "Administration controls",
        }
    }

    /// Whether a granted role may include this panel in its control surface.
    #[must_use]
    pub const fn is_available_to(self, role: Role) -> bool {
        if matches!(role, Role::Admin) {
            return true;
        }

        match self {
            Self::Program | Self::Preview => true,
            Self::Transition => matches!(role, Role::Operator),
            Self::Graphics => matches!(role, Role::Graphics),
            Self::Audio => matches!(role, Role::Audio),
            Self::Replay => matches!(role, Role::Replay),
            Self::Administration => false,
        }
    }

    fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "program" => Some(Self::Program),
            "preview" => Some(Self::Preview),
            "transition" => Some(Self::Transition),
            "graphics" => Some(Self::Graphics),
            "audio" => Some(Self::Audio),
            "replay" => Some(Self::Replay),
            "administration" => Some(Self::Administration),
            _ => None,
        }
    }
}

/// A role-scoped semantic route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Route {
    role: Role,
    panel: Panel,
}

impl Route {
    /// Creates a route when the panel is available to the role.
    ///
    /// # Errors
    ///
    /// Returns [`RouteError::PanelUnavailable`] when the role cannot use the panel.
    pub const fn new(role: Role, panel: Panel) -> Result<Self, RouteError> {
        if panel.is_available_to(role) {
            Ok(Self { role, panel })
        } else {
            Err(RouteError::PanelUnavailable)
        }
    }

    /// Parses an exact `/<role>/<panel>` route.
    ///
    /// # Errors
    ///
    /// Returns a route error for malformed or unauthorized routes.
    pub fn parse(path: &str) -> Result<Self, RouteError> {
        let path = path.strip_prefix('/').ok_or(RouteError::InvalidShape)?;
        let mut segments = path.split('/');
        let role = role_from_slug(segments.next().ok_or(RouteError::InvalidShape)?)
            .ok_or(RouteError::UnknownRole)?;
        let panel = Panel::from_slug(segments.next().ok_or(RouteError::InvalidShape)?)
            .ok_or(RouteError::UnknownPanel)?;
        if segments.next().is_some() {
            return Err(RouteError::InvalidShape);
        }
        Self::new(role, panel)
    }

    #[must_use]
    pub const fn role(self) -> Role {
        self.role
    }

    #[must_use]
    pub const fn panel(self) -> Panel {
        self.panel
    }

    /// Produces the canonical semantic path.
    #[must_use]
    pub fn path(self) -> String {
        format!("/{}/{}", role_slug(self.role), self.panel.slug())
    }
}

/// Failure to resolve a role-scoped panel route.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RouteError {
    InvalidShape,
    UnknownRole,
    UnknownPanel,
    PanelUnavailable,
}

impl fmt::Display for RouteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidShape => "route must have the form /<role>/<panel>",
            Self::UnknownRole => "route contains an unknown role",
            Self::UnknownPanel => "route contains an unknown panel",
            Self::PanelUnavailable => "panel is unavailable to this role",
        })
    }
}

impl std::error::Error for RouteError {}

fn role_slug(role: Role) -> &'static str {
    match role {
        Role::Viewer => "viewer",
        Role::Graphics => "graphics",
        Role::Audio => "audio",
        Role::Replay => "replay",
        Role::Operator => "operator",
        Role::Admin => "admin",
    }
}

fn role_from_slug(slug: &str) -> Option<Role> {
    match slug {
        "viewer" => Some(Role::Viewer),
        "graphics" => Some(Role::Graphics),
        "audio" => Some(Role::Audio),
        "replay" => Some(Role::Replay),
        "operator" => Some(Role::Operator),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

/// A validated WebSocket endpoint string owned by the control shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionUrl(String);

impl ConnectionUrl {
    /// Validates a `ws://` or `wss://` endpoint without performing any I/O.
    ///
    /// # Errors
    ///
    /// Rejects unsupported schemes, malformed authorities, fragments, and whitespace.
    pub fn parse(value: impl Into<String>) -> Result<Self, ConnectionUrlError> {
        let value = value.into();
        let remainder = value
            .strip_prefix("ws://")
            .or_else(|| value.strip_prefix("wss://"))
            .ok_or(ConnectionUrlError::UnsupportedScheme)?;

        if remainder.is_empty() {
            return Err(ConnectionUrlError::MissingAuthority);
        }
        if value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(ConnectionUrlError::Whitespace);
        }
        if remainder.contains('#') {
            return Err(ConnectionUrlError::Fragment);
        }
        if remainder.contains('\\') {
            return Err(ConnectionUrlError::InvalidAuthority);
        }

        let authority_end = remainder.find(['/', '?']).unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        validate_authority(authority)?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConnectionUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate_authority(authority: &str) -> Result<(), ConnectionUrlError> {
    if authority.is_empty() {
        return Err(ConnectionUrlError::MissingAuthority);
    }
    if authority.contains('@') {
        return Err(ConnectionUrlError::Credentials);
    }

    if let Some(ipv6) = authority.strip_prefix('[') {
        let closing = ipv6.find(']').ok_or(ConnectionUrlError::InvalidAuthority)?;
        if closing == 0 {
            return Err(ConnectionUrlError::InvalidAuthority);
        }
        if ipv6[..closing].parse::<Ipv6Addr>().is_err() {
            return Err(ConnectionUrlError::InvalidAuthority);
        }
        let suffix = &ipv6[closing + 1..];
        if !suffix.is_empty() {
            validate_port(
                suffix
                    .strip_prefix(':')
                    .ok_or(ConnectionUrlError::InvalidAuthority)?,
            )?;
        }
        return Ok(());
    }

    let (host, port) = authority
        .split_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if host.is_empty()
        || host.starts_with(['.', '-'])
        || host.ends_with(['.', '-'])
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return Err(ConnectionUrlError::InvalidAuthority);
    }
    if let Some(port) = port {
        validate_port(port)?;
    }
    Ok(())
}

fn validate_port(port: &str) -> Result<(), ConnectionUrlError> {
    if port.is_empty() || port.parse::<u16>().is_err() {
        Err(ConnectionUrlError::InvalidPort)
    } else {
        Ok(())
    }
}

/// Why a connection endpoint was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionUrlError {
    UnsupportedScheme,
    MissingAuthority,
    InvalidAuthority,
    InvalidPort,
    Credentials,
    Fragment,
    Whitespace,
}

impl fmt::Display for ConnectionUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedScheme => "connection URL must start with ws:// or wss://",
            Self::MissingAuthority => "connection URL must include a host",
            Self::InvalidAuthority => "connection URL has an invalid host",
            Self::InvalidPort => "connection URL has an invalid port",
            Self::Credentials => "connection URL must not include credentials",
            Self::Fragment => "connection URL must not include a fragment",
            Self::Whitespace => "connection URL must not include whitespace",
        })
    }
}

impl std::error::Error for ConnectionUrlError {}

/// Complete transport-independent settings selected by the web connection form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebClientConfig {
    pub connection_url: ConnectionUrl,
    pub client: ClientConfig,
}

impl WebClientConfig {
    /// Validates the endpoint and constructs the protocol client settings.
    ///
    /// # Errors
    ///
    /// Returns a URL error when `connection_url` is not an accepted WebSocket URL.
    pub fn new(
        connection_url: impl Into<String>,
        desired_role: Role,
        client_id: impl Into<String>,
        project_id: ProjectId,
    ) -> Result<Self, ConnectionUrlError> {
        Ok(Self {
            connection_url: ConnectionUrl::parse(connection_url)?,
            client: client_config(desired_role, client_id, project_id),
        })
    }
}

/// Constructs the `fm-client` settings used by the web control surface.
#[must_use]
pub fn client_config(
    desired_role: Role,
    client_id: impl Into<String>,
    project_id: ProjectId,
) -> ClientConfig {
    ClientConfig::new(
        SUPPORTED_PROTOCOL_VERSIONS.to_vec(),
        concat!("freemix-web ", env!("CARGO_PKG_VERSION")),
        ClientType::Web,
        desired_role,
        client_id,
        project_id,
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU128;

    use super::*;

    fn project_id() -> ProjectId {
        ProjectId::new(NonZeroU128::new(7).expect("test ID is nonzero"))
    }

    #[test]
    fn routes_are_scoped_to_semantic_roles() {
        let route = Route::parse("/operator/transition").unwrap();
        assert_eq!(route.role(), Role::Operator);
        assert_eq!(route.panel(), Panel::Transition);
        assert_eq!(route.path(), "/operator/transition");

        assert_eq!(
            Route::new(Role::Viewer, Panel::Transition),
            Err(RouteError::PanelUnavailable)
        );
        assert!(Route::new(Role::Graphics, Panel::Program).is_ok());
        assert!(Route::new(Role::Admin, Panel::Audio).is_ok());
        assert_eq!(
            Route::parse("/operator/audio"),
            Err(RouteError::PanelUnavailable)
        );
    }

    #[test]
    fn connection_urls_accept_only_well_formed_websocket_strings() {
        for url in [
            "ws://localhost:9000/control",
            "wss://control.example.test/events?token=opaque",
            "ws://[::1]:8080",
        ] {
            assert_eq!(ConnectionUrl::parse(url).unwrap().as_str(), url);
        }

        for url in [
            "http://example.test",
            "WSS://example.test",
            "ws://",
            "ws://user@example.test",
            "ws://example.test:99999",
            "ws://example.test/control#fragment",
            "ws://example.test/a path",
        ] {
            assert!(ConnectionUrl::parse(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn critical_controls_have_explicit_accessibility_labels() {
        assert_eq!(Panel::Program.accessibility_label(), "Program output");
        assert_eq!(Panel::Preview.accessibility_label(), "Preview output");
        assert_eq!(
            Panel::Transition.accessibility_label(),
            "Transition controls"
        );
        assert_eq!(
            TransitionControl::Cut.accessibility_label(),
            "Cut Preview to Program"
        );
        assert_eq!(
            TransitionControl::Auto.accessibility_label(),
            "Transition Preview to Program"
        );
        assert_eq!(
            TransitionControl::Wipe.accessibility_label(),
            "Wipe Preview to Program"
        );
        assert_eq!(
            TransitionControl::Duration.accessibility_label(),
            "Transition duration"
        );
    }

    #[test]
    fn web_config_builds_an_fm_client_configuration() {
        let config = WebClientConfig::new(
            "wss://control.example.test/ws",
            Role::Operator,
            "web-console-1",
            project_id(),
        )
        .unwrap();

        assert_eq!(
            config.connection_url.as_str(),
            "wss://control.example.test/ws"
        );
        assert_eq!(
            config.client.supported_versions,
            SUPPORTED_PROTOCOL_VERSIONS
        );
        assert_eq!(config.client.client_type, ClientType::Web);
        assert_eq!(config.client.desired_role, Role::Operator);
        assert_eq!(config.client.client_id, "web-console-1");
        assert_eq!(config.client.project_id, project_id());
        assert_eq!(config.client.build, "freemix-web 0.1.0");
        assert_eq!(
            config.client.supported_versions,
            [FADE_TO_BLACK_PROTOCOL_VERSION]
        );
    }
}
