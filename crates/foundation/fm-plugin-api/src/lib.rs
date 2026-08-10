//! Versioned, dependency-free contracts shared by plugins and plugin hosts.
//!
//! The records in this crate use fixed-width integers and bounded inline data so
//! the same model can be represented by generated WIT bindings or a C ABI. The
//! crate defines validation rules only; it does not implement a plugin host.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// The current manifest schema understood by this crate.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
/// Maximum UTF-8 bytes in a diagnostic message.
pub const MAX_MESSAGE_BYTES: usize = 1_024;
/// Maximum bytes in a string or byte value.
pub const MAX_VALUE_BYTES: usize = 1_024;
/// Maximum values in one command record.
pub const MAX_COMMAND_ARGUMENTS: usize = 8;
/// Maximum capability entries in a manifest or host grant set.
pub const MAX_CAPABILITY_GRANTS: usize = 32;
/// Maximum UTF-8 bytes in a capability resource scope.
pub const MAX_CAPABILITY_RESOURCE_BYTES: usize = 256;
/// Maximum UTF-8 bytes in a plugin name.
pub const MAX_PLUGIN_NAME_BYTES: usize = 128;

/// A stable numeric status code transported across an ABI boundary.
///
/// This is intentionally a newtype rather than a Rust enum so the FFI record
/// has a fixed integer representation.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct StatusCode(pub u32);

impl StatusCode {
    /// The operation succeeded.
    pub const OK: Self = Self(0);
    /// An argument was malformed.
    pub const INVALID_ARGUMENT: Self = Self(1);
    /// No mutually supported ABI version exists.
    pub const ABI_INCOMPATIBLE: Self = Self(2);
    /// A manifest is structurally invalid.
    pub const MANIFEST_INVALID: Self = Self(3);
    /// A bounded record exceeded its declared limit.
    pub const LIMIT_EXCEEDED: Self = Self(4);
    /// A required capability was not granted.
    pub const PERMISSION_DENIED: Self = Self(5);
    /// A handle identifier or caller is invalid.
    pub const INVALID_HANDLE: Self = Self(6);
    /// A borrowed handle was used in an ownership-only operation.
    pub const HANDLE_BORROWED: Self = Self(7);
    /// A handle has already been released.
    pub const HANDLE_RELEASED: Self = Self(8);
    /// The requested feature is not supported.
    pub const NOT_SUPPORTED: Self = Self(9);
    /// An implementation failed without a more specific status.
    pub const INTERNAL: Self = Self(10);

    /// Returns the stable wire value.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self.0
    }

    /// Whether this status denotes success.
    #[must_use]
    pub const fn is_ok(self) -> bool {
        self.0 == Self::OK.0
    }
}

/// An inline, length-delimited byte sequence.
#[repr(C)]
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct BoundedBytes<const N: usize> {
    len: u32,
    bytes: [u8; N],
}

impl<const N: usize> BoundedBytes<N> {
    /// An empty byte sequence.
    pub const EMPTY: Self = Self {
        len: 0,
        bytes: [0; N],
    };

    /// Copies a slice into a bounded record.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::LIMIT_EXCEEDED`] if `bytes` is too large.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, StatusCode> {
        if bytes.len() > N || bytes.len() > u32::MAX as usize {
            return Err(StatusCode::LIMIT_EXCEEDED);
        }
        let mut result = Self::EMPTY;
        result.bytes[..bytes.len()].copy_from_slice(bytes);
        result.len = u32::try_from(bytes.len()).map_err(|_| StatusCode::LIMIT_EXCEEDED)?;
        Ok(result)
    }

    /// Returns the initialized bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// Returns the byte length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether the record is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for BoundedBytes<N> {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl<const N: usize> fmt::Debug for BoundedBytes<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("BoundedBytes")
            .field(&self.as_slice())
            .finish()
    }
}

/// An inline, length-delimited UTF-8 string.
#[repr(transparent)]
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct BoundedText<const N: usize>(BoundedBytes<N>);

impl<const N: usize> BoundedText<N> {
    /// An empty string.
    pub const EMPTY: Self = Self(BoundedBytes::EMPTY);

    /// Copies a string into a bounded record.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::LIMIT_EXCEEDED`] if its UTF-8 encoding is too
    /// large.
    pub fn from_text(value: &str) -> Result<Self, StatusCode> {
        BoundedBytes::from_slice(value.as_bytes()).map(Self)
    }

    /// Returns the contained string.
    ///
    /// # Panics
    ///
    /// This cannot panic for a value built through the safe API because the
    /// backing bytes are private and construction requires valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Construction is only possible from `str`, and the bytes are private.
        core::str::from_utf8(self.0.as_slice()).expect("BoundedText invariant")
    }

    /// Returns the UTF-8 byte length.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the string is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<const N: usize> fmt::Debug for BoundedText<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

/// A bounded diagnostic message.
pub type Message = BoundedText<MAX_MESSAGE_BYTES>;
/// A bounded plugin display name.
pub type PluginName = BoundedText<MAX_PLUGIN_NAME_BYTES>;
/// A bounded capability resource scope.
pub type CapabilityResource = BoundedText<MAX_CAPABILITY_RESOURCE_BYTES>;

/// A status and bounded human-readable diagnostic.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ErrorRecord {
    /// Stable machine-readable status.
    pub code: StatusCode,
    /// Bounded diagnostic intended for logs or user interfaces.
    pub message: Message,
}

impl ErrorRecord {
    /// Creates an error record.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::LIMIT_EXCEEDED`] if `message` is too large.
    pub fn new(code: StatusCode, message: &str) -> Result<Self, StatusCode> {
        Ok(Self {
            code,
            message: Message::from_text(message)?,
        })
    }
}

/// A semantic ABI version.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AbiVersion {
    /// Breaking-change version.
    pub major: u32,
    /// Feature version component.
    pub minor: u32,
    /// Fix version component.
    pub patch: u32,
}

impl AbiVersion {
    /// Creates a semantic ABI version.
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

/// A stable 128-bit plugin identifier represented without platform UUID types.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct PluginId {
    /// Most significant 64 bits.
    pub high: u64,
    /// Least significant 64 bits.
    pub low: u64,
}

impl PluginId {
    /// The reserved invalid identifier.
    pub const INVALID: Self = Self { high: 0, low: 0 };

    /// Creates an identifier from stable integer components.
    #[must_use]
    pub const fn new(high: u64, low: u64) -> Self {
        Self { high, low }
    }

    /// Whether this is the reserved invalid identifier.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.high != 0 || self.low != 0
    }
}

macro_rules! stable_u64_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
        pub struct $name(pub u64);

        impl $name {
            /// The reserved invalid identifier.
            pub const INVALID: Self = Self(0);

            /// Creates an identifier from its stable wire value.
            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the stable wire value.
            #[must_use]
            pub const fn as_u64(self) -> u64 {
                self.0
            }

            /// Whether this is not the reserved invalid identifier.
            #[must_use]
            pub const fn is_valid(self) -> bool {
                self.0 != 0
            }
        }
    };
}

stable_u64_id!(CapabilityId, "A stable capability identifier.");
stable_u64_id!(HandleId, "A stable host-resource handle identifier.");
stable_u64_id!(CommandId, "A stable command identifier.");

impl CapabilityId {
    /// Scoped filesystem access.
    pub const FILESYSTEM: Self = Self(1);
    /// Scoped network access.
    pub const NETWORK: Self = Self(2);
    /// Clock access.
    pub const CLOCK: Self = Self(3);
    /// Command registration or invocation.
    pub const COMMANDS: Self = Self(4);

    /// Whether this contract defines the capability.
    #[must_use]
    pub const fn is_known(self) -> bool {
        matches!(self.0, 1..=4)
    }
}

/// Stable bit flags used by capability grant records.
pub mod permission {
    /// Read files under a filesystem scope.
    pub const FILESYSTEM_READ: u32 = 1 << 0;
    /// Write existing files under a filesystem scope.
    pub const FILESYSTEM_WRITE: u32 = 1 << 1;
    /// Create or remove entries under a filesystem scope.
    pub const FILESYSTEM_CREATE: u32 = 1 << 2;
    /// Connect to a network endpoint scope.
    pub const NETWORK_CONNECT: u32 = 1 << 0;
    /// Listen on a network endpoint scope.
    pub const NETWORK_LISTEN: u32 = 1 << 1;
    /// Read a monotonic clock.
    pub const CLOCK_MONOTONIC: u32 = 1 << 0;
    /// Read wall-clock time.
    pub const CLOCK_WALL: u32 = 1 << 1;
    /// Invoke commands in the named scope.
    pub const COMMANDS_INVOKE: u32 = 1 << 0;
    /// Register commands in the named scope.
    pub const COMMANDS_REGISTER: u32 = 1 << 1;
}

/// A scoped capability and its permission bits.
///
/// Filesystem resources are host-defined path scopes, network resources are
/// host-defined endpoint scopes, command resources are host-defined command
/// scopes, and clock resources must be empty.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapabilityGrant {
    /// Kind of capability.
    pub capability: CapabilityId,
    /// Capability-specific bits from [`permission`].
    pub permissions: u32,
    /// Exact resource scope. Clock grants use an empty scope.
    pub resource: CapabilityResource,
}

impl CapabilityGrant {
    /// Creates a scoped capability grant.
    ///
    /// # Errors
    ///
    /// Returns a validation or bounds status for malformed values.
    pub fn new(
        capability: CapabilityId,
        permissions: u32,
        resource: &str,
    ) -> Result<Self, StatusCode> {
        let grant = Self {
            capability,
            permissions,
            resource: CapabilityResource::from_text(resource)?,
        };
        grant.validate()?;
        Ok(grant)
    }

    /// Validates the capability kind, permissions, and resource shape.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::INVALID_ARGUMENT`] for unknown bits, empty scoped
    /// resources, or a non-empty clock resource.
    pub const fn validate(&self) -> Result<(), StatusCode> {
        let allowed = match self.capability.0 {
            1 => {
                permission::FILESYSTEM_READ
                    | permission::FILESYSTEM_WRITE
                    | permission::FILESYSTEM_CREATE
            }
            2 => permission::NETWORK_CONNECT | permission::NETWORK_LISTEN,
            3 => permission::CLOCK_MONOTONIC | permission::CLOCK_WALL,
            4 => permission::COMMANDS_INVOKE | permission::COMMANDS_REGISTER,
            _ => return Err(StatusCode::INVALID_ARGUMENT),
        };
        if self.permissions == 0 || self.permissions & !allowed != 0 {
            return Err(StatusCode::INVALID_ARGUMENT);
        }
        if self.capability.0 == CapabilityId::CLOCK.0 {
            if !self.resource.is_empty() {
                return Err(StatusCode::INVALID_ARGUMENT);
            }
        } else if self.resource.is_empty() {
            return Err(StatusCode::INVALID_ARGUMENT);
        }
        Ok(())
    }

    /// Whether this grant includes all permissions and the exact resource of
    /// `requested`.
    #[must_use]
    pub fn allows(&self, requested: &Self) -> bool {
        self.validate().is_ok()
            && requested.validate().is_ok()
            && self.capability == requested.capability
            && self.resource == requested.resource
            && self.permissions & requested.permissions == requested.permissions
    }
}

/// A bounded set of grants. An empty set is the default, so access is denied
/// unless explicitly granted.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilitySet {
    count: u32,
    entries: [CapabilityGrant; MAX_CAPABILITY_GRANTS],
}

impl CapabilitySet {
    /// An empty, default-deny capability set.
    pub const EMPTY: Self = Self {
        count: 0,
        entries: [CapabilityGrant {
            capability: CapabilityId::INVALID,
            permissions: 0,
            resource: CapabilityResource::EMPTY,
        }; MAX_CAPABILITY_GRANTS],
    };

    /// Adds one grant.
    ///
    /// # Errors
    ///
    /// Returns a validation status or [`StatusCode::LIMIT_EXCEEDED`] when full.
    pub fn push(&mut self, grant: CapabilityGrant) -> Result<(), StatusCode> {
        grant.validate()?;
        if self.len() == MAX_CAPABILITY_GRANTS {
            return Err(StatusCode::LIMIT_EXCEEDED);
        }
        if self
            .as_slice()
            .iter()
            .any(|entry| entry.capability == grant.capability && entry.resource == grant.resource)
        {
            return Err(StatusCode::INVALID_ARGUMENT);
        }
        let index = self.len();
        self.entries[index] = grant;
        self.count += 1;
        Ok(())
    }

    /// Returns the active grants.
    #[must_use]
    pub fn as_slice(&self) -> &[CapabilityGrant] {
        &self.entries[..self.len()]
    }

    /// Returns the number of active grants.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count as usize
    }

    /// Whether there are no grants.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Whether any entry explicitly allows the request.
    #[must_use]
    pub fn allows(&self, requested: &CapabilityGrant) -> bool {
        self.as_slice().iter().any(|grant| grant.allows(requested))
    }

    /// Validates all active records and rejects duplicate scopes.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::MANIFEST_INVALID`] for invalid or duplicate data.
    pub fn validate(&self) -> Result<(), StatusCode> {
        if self.count as usize > MAX_CAPABILITY_GRANTS {
            return Err(StatusCode::MANIFEST_INVALID);
        }
        for (index, grant) in self.as_slice().iter().enumerate() {
            grant.validate().map_err(|_| StatusCode::MANIFEST_INVALID)?;
            if self.as_slice()[..index].iter().any(|entry| {
                entry.capability == grant.capability && entry.resource == grant.resource
            }) {
                return Err(StatusCode::MANIFEST_INVALID);
            }
        }
        Ok(())
    }

    fn allows_all(&self, requested: &Self) -> bool {
        requested
            .as_slice()
            .iter()
            .all(|request| self.allows(request))
    }
}

impl Default for CapabilitySet {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// A plugin manifest containing identity, its exact ABI, and requested access.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginManifest {
    /// Version of this manifest record.
    pub schema_version: u32,
    /// Stable plugin identity. The all-zero value is invalid.
    pub plugin_id: PluginId,
    /// Semantic version of the plugin implementation.
    pub plugin_version: AbiVersion,
    /// Exact current ABI required by the plugin.
    pub abi_version: AbiVersion,
    /// Human-readable plugin name.
    pub name: PluginName,
    /// Capabilities requested by the plugin. Requests are not grants.
    pub requested_capabilities: CapabilitySet,
}

impl PluginManifest {
    /// Creates a manifest with no requested capabilities.
    ///
    /// # Errors
    ///
    /// Returns a bounds status if `name` is too large. Use [`Self::validate`]
    /// before accepting the completed manifest.
    pub fn new(
        plugin_id: PluginId,
        plugin_version: AbiVersion,
        abi_version: AbiVersion,
        name: &str,
    ) -> Result<Self, StatusCode> {
        Ok(Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            plugin_id,
            plugin_version,
            abi_version,
            name: PluginName::from_text(name)?,
            requested_capabilities: CapabilitySet::EMPTY,
        })
    }

    /// Validates all intrinsic manifest fields.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::MANIFEST_INVALID`] for an unsupported schema,
    /// invalid identity, name, or capability request.
    pub fn validate(&self) -> Result<(), StatusCode> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION
            || !self.plugin_id.is_valid()
            || self.name.is_empty()
            || self.requested_capabilities.validate().is_err()
        {
            return Err(StatusCode::MANIFEST_INVALID);
        }
        Ok(())
    }

    /// Validates the manifest against the exact current host ABI and explicit
    /// grants.
    ///
    /// Capability policy is default-deny: every request must be covered by an
    /// exact resource grant with a superset of permission bits.
    ///
    /// # Errors
    ///
    /// Returns a manifest, exact-ABI, or permission status.
    pub fn validate_current(
        &self,
        host_abi: AbiVersion,
        host_grants: &CapabilitySet,
    ) -> Result<(), StatusCode> {
        self.validate()?;
        host_grants.validate()?;
        if self.abi_version != host_abi {
            return Err(StatusCode::ABI_INCOMPATIBLE);
        }
        if !host_grants.allows_all(&self.requested_capabilities) {
            return Err(StatusCode::PERMISSION_DENIED);
        }
        Ok(())
    }
}

/// Ownership mode of a handle record.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HandleOwnership(pub u32);

impl HandleOwnership {
    /// The holder owns the live handle and is responsible for releasing it.
    pub const OWNED: Self = Self(1);
    /// The holder may use but must not release the handle.
    pub const BORROWED: Self = Self(2);
}

/// Lifecycle state of a handle record.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HandleState(pub u32);

impl HandleState {
    /// The handle may be used according to its ownership mode.
    pub const LIVE: Self = Self(1);
    /// The owning handle was released and may no longer be used.
    pub const RELEASED: Self = Self(2);
}

/// A host-resource handle and the metadata required to enforce its lifecycle.
///
/// The original owner remains in `owner` when a temporary borrowed record is
/// issued to another plugin. Borrowed records cannot be released or reborrowed.
#[repr(C)]
#[derive(Debug, Eq, PartialEq)]
pub struct HandleRecord {
    /// Host-unique nonzero handle identifier.
    pub id: HandleId,
    /// Stable host-defined resource type identifier.
    pub resource_type: u64,
    /// Plugin that owns and must release this handle.
    pub owner: PluginId,
    /// Plugin currently allowed to use this record.
    pub holder: PluginId,
    /// Owned or borrowed mode.
    pub ownership: HandleOwnership,
    /// Live or released state.
    pub state: HandleState,
}

impl HandleRecord {
    /// Creates a live owned handle.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::INVALID_HANDLE`] for zero IDs or resource types.
    pub fn owned(id: HandleId, resource_type: u64, owner: PluginId) -> Result<Self, StatusCode> {
        if !id.is_valid() || resource_type == 0 || !owner.is_valid() {
            return Err(StatusCode::INVALID_HANDLE);
        }
        Ok(Self {
            id,
            resource_type,
            owner,
            holder: owner,
            ownership: HandleOwnership::OWNED,
            state: HandleState::LIVE,
        })
    }

    /// Issues a temporary borrowed record to `borrower`.
    ///
    /// The caller must hold the live owned record. This method does not alter
    /// the owned record; its lifetime remains the host's responsibility.
    ///
    /// # Errors
    ///
    /// Returns a handle lifecycle status when the caller cannot borrow it.
    pub fn borrow_to(&self, caller: PluginId, borrower: PluginId) -> Result<Self, StatusCode> {
        self.validate_use(caller)?;
        if self.ownership != HandleOwnership::OWNED {
            return Err(StatusCode::HANDLE_BORROWED);
        }
        if !borrower.is_valid() {
            return Err(StatusCode::INVALID_HANDLE);
        }
        Ok(Self {
            id: self.id,
            resource_type: self.resource_type,
            owner: self.owner,
            holder: borrower,
            ownership: HandleOwnership::BORROWED,
            state: HandleState::LIVE,
        })
    }

    /// Checks that `caller` may use this record.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::HANDLE_RELEASED`] after release, or
    /// [`StatusCode::INVALID_HANDLE`] for a wrong caller or malformed record.
    pub fn validate_use(&self, caller: PluginId) -> Result<(), StatusCode> {
        if self.state == HandleState::RELEASED {
            return Err(StatusCode::HANDLE_RELEASED);
        }
        if self.state != HandleState::LIVE
            || (self.ownership != HandleOwnership::OWNED
                && self.ownership != HandleOwnership::BORROWED)
            || !self.id.is_valid()
            || self.resource_type == 0
            || !self.owner.is_valid()
            || caller != self.holder
        {
            return Err(StatusCode::INVALID_HANDLE);
        }
        Ok(())
    }

    /// Releases an owned handle exactly once.
    ///
    /// # Errors
    ///
    /// Borrowers receive [`StatusCode::HANDLE_BORROWED`], repeated releases
    /// receive [`StatusCode::HANDLE_RELEASED`], and non-owners receive
    /// [`StatusCode::INVALID_HANDLE`].
    pub fn release(&mut self, caller: PluginId) -> Result<(), StatusCode> {
        if self.state == HandleState::RELEASED {
            return Err(StatusCode::HANDLE_RELEASED);
        }
        if self.ownership == HandleOwnership::BORROWED {
            return Err(StatusCode::HANDLE_BORROWED);
        }
        self.validate_use(caller)?;
        if caller != self.owner {
            return Err(StatusCode::INVALID_HANDLE);
        }
        self.state = HandleState::RELEASED;
        Ok(())
    }
}

/// Stable value discriminants.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValueKind(pub u32);

impl ValueKind {
    /// No value.
    pub const NULL: Self = Self(0);
    /// Boolean encoded as scalar zero or one.
    pub const BOOL: Self = Self(1);
    /// Signed 64-bit integer encoded in `scalar`.
    pub const I64: Self = Self(2);
    /// Unsigned 64-bit integer encoded in `scalar`.
    pub const U64: Self = Self(3);
    /// IEEE-754 binary64 bits encoded in `scalar`.
    pub const F64: Self = Self(4);
    /// UTF-8 bytes encoded in `data`.
    pub const STRING: Self = Self(5);
    /// Opaque bytes encoded in `data`.
    pub const BYTES: Self = Self(6);
    /// A nonzero [`HandleId`] encoded in `scalar`.
    pub const HANDLE: Self = Self(7);
}

/// A fixed-layout value suitable for command arguments and results.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValueRecord {
    /// Value representation.
    pub kind: ValueKind,
    /// Reserved for a future ABI version; must be zero.
    pub flags: u32,
    /// Scalar payload for booleans, numbers, and handles.
    pub scalar: u64,
    /// String or byte payload.
    pub data: BoundedBytes<MAX_VALUE_BYTES>,
}

impl ValueRecord {
    /// A null record.
    pub const NULL: Self = Self {
        kind: ValueKind::NULL,
        flags: 0,
        scalar: 0,
        data: BoundedBytes::EMPTY,
    };

    /// Creates a boolean value.
    #[must_use]
    pub const fn boolean(value: bool) -> Self {
        Self {
            kind: ValueKind::BOOL,
            flags: 0,
            scalar: value as u64,
            data: BoundedBytes::EMPTY,
        }
    }

    /// Creates a signed integer value.
    #[must_use]
    pub const fn i64(value: i64) -> Self {
        Self {
            kind: ValueKind::I64,
            flags: 0,
            scalar: value.cast_unsigned(),
            data: BoundedBytes::EMPTY,
        }
    }

    /// Creates an unsigned integer value.
    #[must_use]
    pub const fn u64(value: u64) -> Self {
        Self {
            kind: ValueKind::U64,
            flags: 0,
            scalar: value,
            data: BoundedBytes::EMPTY,
        }
    }

    /// Creates a floating-point value.
    #[must_use]
    pub const fn f64(value: f64) -> Self {
        Self {
            kind: ValueKind::F64,
            flags: 0,
            scalar: value.to_bits(),
            data: BoundedBytes::EMPTY,
        }
    }

    /// Creates a UTF-8 string value.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::LIMIT_EXCEEDED`] when `value` is too large.
    pub fn string(value: &str) -> Result<Self, StatusCode> {
        Ok(Self {
            kind: ValueKind::STRING,
            flags: 0,
            scalar: 0,
            data: BoundedBytes::from_slice(value.as_bytes())?,
        })
    }

    /// Creates an opaque byte value.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::LIMIT_EXCEEDED`] when `value` is too large.
    pub fn bytes(value: &[u8]) -> Result<Self, StatusCode> {
        Ok(Self {
            kind: ValueKind::BYTES,
            flags: 0,
            scalar: 0,
            data: BoundedBytes::from_slice(value)?,
        })
    }

    /// Creates a handle reference value.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::INVALID_HANDLE`] for the reserved zero handle.
    pub fn handle(value: HandleId) -> Result<Self, StatusCode> {
        if !value.is_valid() {
            return Err(StatusCode::INVALID_HANDLE);
        }
        Ok(Self {
            kind: ValueKind::HANDLE,
            flags: 0,
            scalar: value.0,
            data: BoundedBytes::EMPTY,
        })
    }

    /// Validates discriminant-specific invariants.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::INVALID_ARGUMENT`] for malformed values.
    pub fn validate(&self) -> Result<(), StatusCode> {
        if self.flags != 0 {
            return Err(StatusCode::INVALID_ARGUMENT);
        }
        match self.kind {
            ValueKind::NULL => {
                if self.scalar != 0 || !self.data.is_empty() {
                    return Err(StatusCode::INVALID_ARGUMENT);
                }
            }
            ValueKind::BOOL => {
                if self.scalar > 1 || !self.data.is_empty() {
                    return Err(StatusCode::INVALID_ARGUMENT);
                }
            }
            ValueKind::I64 | ValueKind::U64 | ValueKind::F64 => {
                if !self.data.is_empty() {
                    return Err(StatusCode::INVALID_ARGUMENT);
                }
            }
            ValueKind::STRING => {
                if self.scalar != 0 || core::str::from_utf8(self.data.as_slice()).is_err() {
                    return Err(StatusCode::INVALID_ARGUMENT);
                }
            }
            ValueKind::BYTES => {
                if self.scalar != 0 {
                    return Err(StatusCode::INVALID_ARGUMENT);
                }
            }
            ValueKind::HANDLE => {
                if self.scalar == 0 || !self.data.is_empty() {
                    return Err(StatusCode::INVALID_HANDLE);
                }
            }
            _ => return Err(StatusCode::INVALID_ARGUMENT),
        }
        Ok(())
    }
}

impl Default for ValueRecord {
    fn default() -> Self {
        Self::NULL
    }
}

/// A bounded command invocation record.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandRecord {
    /// Stable command identifier.
    pub command: CommandId,
    /// Caller-selected token copied to the corresponding response.
    pub correlation_id: u64,
    argument_count: u32,
    /// Reserved for a future ABI version; must be zero.
    pub flags: u32,
    arguments: [ValueRecord; MAX_COMMAND_ARGUMENTS],
}

impl CommandRecord {
    /// Creates an empty invocation.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::INVALID_ARGUMENT`] for the reserved command ID.
    pub const fn new(command: CommandId, correlation_id: u64) -> Result<Self, StatusCode> {
        if !command.is_valid() {
            return Err(StatusCode::INVALID_ARGUMENT);
        }
        Ok(Self {
            command,
            correlation_id,
            argument_count: 0,
            flags: 0,
            arguments: [ValueRecord::NULL; MAX_COMMAND_ARGUMENTS],
        })
    }

    /// Appends one validated argument.
    ///
    /// # Errors
    ///
    /// Returns a value validation status or [`StatusCode::LIMIT_EXCEEDED`] when
    /// the command is full.
    pub fn push_argument(&mut self, argument: ValueRecord) -> Result<(), StatusCode> {
        argument.validate()?;
        if self.argument_count as usize == MAX_COMMAND_ARGUMENTS {
            return Err(StatusCode::LIMIT_EXCEEDED);
        }
        self.arguments[self.argument_count as usize] = argument;
        self.argument_count += 1;
        Ok(())
    }

    /// Returns active arguments.
    #[must_use]
    pub fn arguments(&self) -> &[ValueRecord] {
        &self.arguments[..self.argument_count as usize]
    }

    /// Validates a command record, including records decoded from another ABI.
    ///
    /// # Errors
    ///
    /// Returns [`StatusCode::INVALID_ARGUMENT`] for malformed records.
    pub fn validate(&self) -> Result<(), StatusCode> {
        if !self.command.is_valid()
            || self.flags != 0
            || self.argument_count as usize > MAX_COMMAND_ARGUMENTS
        {
            return Err(StatusCode::INVALID_ARGUMENT);
        }
        self.arguments().iter().try_for_each(ValueRecord::validate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABI_1_2_0: AbiVersion = AbiVersion::new(1, 2, 0);
    const ABI_1_3_0: AbiVersion = AbiVersion::new(1, 3, 0);
    const OWNER: PluginId = PluginId::new(1, 10);
    const BORROWER: PluginId = PluginId::new(2, 20);

    #[test]
    fn only_owner_can_release_and_borrowed_handles_cannot_release() {
        let mut owned = HandleRecord::owned(HandleId::new(9), 42, OWNER).unwrap();
        let mut borrowed = owned.borrow_to(OWNER, BORROWER).unwrap();
        assert_eq!(borrowed.validate_use(BORROWER), Ok(()));
        assert_eq!(borrowed.release(BORROWER), Err(StatusCode::HANDLE_BORROWED));
        assert_eq!(owned.release(BORROWER), Err(StatusCode::INVALID_HANDLE));
        assert_eq!(owned.release(OWNER), Ok(()));
        assert_eq!(owned.validate_use(OWNER), Err(StatusCode::HANDLE_RELEASED));
        assert_eq!(owned.release(OWNER), Err(StatusCode::HANDLE_RELEASED));
    }

    #[test]
    fn bounded_records_reject_oversized_input() {
        let too_large = [b'x'; MAX_MESSAGE_BYTES + 1];
        let text = core::str::from_utf8(&too_large).unwrap();
        assert_eq!(Message::from_text(text), Err(StatusCode::LIMIT_EXCEEDED));

        let mut command = CommandRecord::new(CommandId::new(7), 99).unwrap();
        for _ in 0..MAX_COMMAND_ARGUMENTS {
            command.push_argument(ValueRecord::NULL).unwrap();
        }
        assert_eq!(
            command.push_argument(ValueRecord::NULL),
            Err(StatusCode::LIMIT_EXCEEDED)
        );
    }

    #[test]
    fn capabilities_are_default_deny_and_scoped() {
        let request = CapabilityGrant::new(
            CapabilityId::FILESYSTEM,
            permission::FILESYSTEM_READ,
            "/media",
        )
        .unwrap();
        let grants = CapabilitySet::default();
        assert!(!grants.allows(&request));

        let mut grants = CapabilitySet::default();
        grants
            .push(
                CapabilityGrant::new(
                    CapabilityId::FILESYSTEM,
                    permission::FILESYSTEM_READ | permission::FILESYSTEM_WRITE,
                    "/media",
                )
                .unwrap(),
            )
            .unwrap();
        assert!(grants.allows(&request));
        assert!(
            !grants.allows(
                &CapabilityGrant::new(
                    CapabilityId::FILESYSTEM,
                    permission::FILESYSTEM_READ,
                    "/private",
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn manifest_requires_exact_current_abi_and_explicit_grants() {
        let abi = ABI_1_3_0;
        let mut manifest = PluginManifest::new(OWNER, ABI_1_2_0, abi, "mixer").unwrap();
        manifest
            .requested_capabilities
            .push(
                CapabilityGrant::new(
                    CapabilityId::NETWORK,
                    permission::NETWORK_CONNECT,
                    "api.example.test:443",
                )
                .unwrap(),
            )
            .unwrap();

        assert_eq!(
            manifest.validate_current(abi, &CapabilitySet::default()),
            Err(StatusCode::PERMISSION_DENIED)
        );
        let mut grants = CapabilitySet::default();
        grants
            .push(
                CapabilityGrant::new(
                    CapabilityId::NETWORK,
                    permission::NETWORK_CONNECT,
                    "api.example.test:443",
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(manifest.validate_current(abi, &grants), Ok(()));

        manifest.schema_version += 1;
        assert_eq!(manifest.validate(), Err(StatusCode::MANIFEST_INVALID));
    }

    #[test]
    fn numeric_codes_and_ids_are_stable() {
        assert_eq!(StatusCode::OK.as_u32(), 0);
        assert_eq!(StatusCode::INVALID_ARGUMENT.as_u32(), 1);
        assert_eq!(StatusCode::ABI_INCOMPATIBLE.as_u32(), 2);
        assert_eq!(StatusCode::MANIFEST_INVALID.as_u32(), 3);
        assert_eq!(StatusCode::LIMIT_EXCEEDED.as_u32(), 4);
        assert_eq!(StatusCode::PERMISSION_DENIED.as_u32(), 5);
        assert_eq!(StatusCode::INVALID_HANDLE.as_u32(), 6);
        assert_eq!(StatusCode::HANDLE_BORROWED.as_u32(), 7);
        assert_eq!(StatusCode::HANDLE_RELEASED.as_u32(), 8);
        assert_eq!(StatusCode::NOT_SUPPORTED.as_u32(), 9);
        assert_eq!(StatusCode::INTERNAL.as_u32(), 10);
        assert_eq!(CapabilityId::FILESYSTEM.as_u64(), 1);
        assert_eq!(CapabilityId::NETWORK.as_u64(), 2);
        assert_eq!(CapabilityId::CLOCK.as_u64(), 3);
        assert_eq!(CapabilityId::COMMANDS.as_u64(), 4);
        assert_eq!(HandleOwnership::OWNED.0, 1);
        assert_eq!(HandleOwnership::BORROWED.0, 2);
        assert_eq!(HandleState::LIVE.0, 1);
        assert_eq!(HandleState::RELEASED.0, 2);
        assert_eq!(ValueKind::NULL.0, 0);
        assert_eq!(ValueKind::HANDLE.0, 7);
    }
}
