use std::ops::{BitOr, BitOrAssign};

use crate::DeviceProfile;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TextureFormat {
    R8Unorm,
    Rg8Unorm,
    Rgba8Unorm,
    Bgra8Unorm,
    Rgba16Float,
    Rgba32Float,
    Depth32Float,
}

impl TextureFormat {
    #[must_use]
    pub const fn bytes_per_texel(self) -> u64 {
        match self {
            Self::R8Unorm => 1,
            Self::Rg8Unorm => 2,
            Self::Rgba8Unorm | Self::Bgra8Unorm | Self::Depth32Float => 4,
            Self::Rgba16Float => 8,
            Self::Rgba32Float => 16,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TextureDimension {
    One,
    Two,
    Three,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TextureUsage(u8);

impl TextureUsage {
    pub const NONE: Self = Self(0);
    pub const COPY_SOURCE: Self = Self(1 << 0);
    pub const COPY_DESTINATION: Self = Self(1 << 1);
    pub const SAMPLED: Self = Self(1 << 2);
    pub const STORAGE: Self = Self(1 << 3);
    pub const RENDER_ATTACHMENT: Self = Self(1 << 4);

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for TextureUsage {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for TextureUsage {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TextureDescriptor {
    pub label: Option<String>,
    pub dimension: TextureDimension,
    pub width: u32,
    pub height: u32,
    pub depth_or_layers: u32,
    pub mip_levels: u32,
    pub sample_count: u32,
    pub format: TextureFormat,
    pub usage: TextureUsage,
}

impl TextureDescriptor {
    #[must_use]
    pub fn two_dimensional(
        width: u32,
        height: u32,
        format: TextureFormat,
        usage: TextureUsage,
    ) -> Self {
        Self {
            label: None,
            dimension: TextureDimension::Two,
            width,
            height,
            depth_or_layers: 1,
            mip_levels: 1,
            sample_count: 1,
            format,
            usage,
        }
    }

    /// Validates this descriptor against an enabled device profile.
    ///
    /// # Errors
    ///
    /// Returns a precise descriptor contract violation.
    pub fn validate(&self, profile: &DeviceProfile) -> Result<(), DescriptorError> {
        <Self as ResourceDescriptor>::validate(self, profile)
    }

    /// Returns the conservative pool budget charge.
    ///
    /// # Errors
    ///
    /// Returns [`DescriptorError::SizeOverflow`] if the charge overflows.
    pub fn byte_size(&self) -> Result<u64, DescriptorError> {
        <Self as ResourceDescriptor>::byte_size(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BufferUsage(u8);

impl BufferUsage {
    pub const NONE: Self = Self(0);
    pub const COPY_SOURCE: Self = Self(1 << 0);
    pub const COPY_DESTINATION: Self = Self(1 << 1);
    pub const UNIFORM: Self = Self(1 << 2);
    pub const STORAGE: Self = Self(1 << 3);
    pub const VERTEX: Self = Self(1 << 4);
    pub const INDEX: Self = Self(1 << 5);
    pub const MAP_READ: Self = Self(1 << 6);
    pub const MAP_WRITE: Self = Self(1 << 7);

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for BufferUsage {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for BufferUsage {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct BufferDescriptor {
    pub label: Option<String>,
    pub size: u64,
    pub usage: BufferUsage,
}

impl BufferDescriptor {
    #[must_use]
    pub const fn new(size: u64, usage: BufferUsage) -> Self {
        Self {
            label: None,
            size,
            usage,
        }
    }

    /// Validates this descriptor against an enabled device profile.
    ///
    /// # Errors
    ///
    /// Returns a precise descriptor contract violation.
    pub fn validate(&self, profile: &DeviceProfile) -> Result<(), DescriptorError> {
        <Self as ResourceDescriptor>::validate(self, profile)
    }

    /// Returns the pool budget charge for this buffer.
    ///
    /// # Errors
    ///
    /// This portable buffer descriptor cannot overflow, but the result remains
    /// fallible to match the common resource descriptor contract.
    pub fn byte_size(&self) -> Result<u64, DescriptorError> {
        <Self as ResourceDescriptor>::byte_size(self)
    }
}

/// Descriptor behavior required by the bounded pool.
pub trait ResourceDescriptor: Clone + Eq {
    /// Validates the descriptor against an enabled device profile.
    ///
    /// # Errors
    ///
    /// Returns a precise descriptor contract violation.
    fn validate(&self, profile: &DeviceProfile) -> Result<(), DescriptorError>;

    /// Returns the conservative physical byte charge used by pool budgets.
    ///
    /// # Errors
    ///
    /// Returns [`DescriptorError::SizeOverflow`] if the charge cannot be
    /// represented.
    fn byte_size(&self) -> Result<u64, DescriptorError>;
}

impl ResourceDescriptor for TextureDescriptor {
    fn validate(&self, profile: &DeviceProfile) -> Result<(), DescriptorError> {
        if self.width == 0 || self.height == 0 || self.depth_or_layers == 0 {
            return Err(DescriptorError::ZeroExtent);
        }
        if self.mip_levels == 0 {
            return Err(DescriptorError::ZeroMipLevels);
        }
        if self.sample_count == 0 || !self.sample_count.is_power_of_two() {
            return Err(DescriptorError::InvalidSampleCount(self.sample_count));
        }
        if self.usage.is_empty() {
            return Err(DescriptorError::EmptyUsage);
        }
        if !profile.supports_texture_format(self.format) {
            return Err(DescriptorError::UnsupportedTextureFormat(self.format));
        }
        let limits = profile.limits();
        let in_limits = match self.dimension {
            TextureDimension::One => {
                self.height == 1
                    && self.depth_or_layers <= limits.max_texture_array_layers
                    && self.width <= limits.max_texture_dimension_1d
            }
            TextureDimension::Two => {
                self.width <= limits.max_texture_dimension_2d
                    && self.height <= limits.max_texture_dimension_2d
                    && self.depth_or_layers <= limits.max_texture_array_layers
            }
            TextureDimension::Three => {
                self.width <= limits.max_texture_dimension_3d
                    && self.height <= limits.max_texture_dimension_3d
                    && self.depth_or_layers <= limits.max_texture_dimension_3d
            }
        };
        if !in_limits {
            return Err(DescriptorError::LimitExceeded);
        }
        self.byte_size().map(|_| ())
    }

    fn byte_size(&self) -> Result<u64, DescriptorError> {
        let texels = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|value| value.checked_mul(u64::from(self.depth_or_layers)))
            .ok_or(DescriptorError::SizeOverflow)?;
        let base = texels
            .checked_mul(self.format.bytes_per_texel())
            .and_then(|value| value.checked_mul(u64::from(self.sample_count)))
            .ok_or(DescriptorError::SizeOverflow)?;
        // This is a conservative full-chain charge without pretending to know
        // backend-specific alignment or tiling.
        base.checked_mul(u64::from(self.mip_levels))
            .ok_or(DescriptorError::SizeOverflow)
    }
}

impl ResourceDescriptor for BufferDescriptor {
    fn validate(&self, profile: &DeviceProfile) -> Result<(), DescriptorError> {
        if self.size == 0 {
            return Err(DescriptorError::ZeroSize);
        }
        if self.usage.is_empty() {
            return Err(DescriptorError::EmptyUsage);
        }
        if self.size > profile.limits().max_buffer_size {
            return Err(DescriptorError::LimitExceeded);
        }
        Ok(())
    }

    fn byte_size(&self) -> Result<u64, DescriptorError> {
        Ok(self.size)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptorError {
    ZeroExtent,
    ZeroSize,
    ZeroMipLevels,
    InvalidSampleCount(u32),
    EmptyUsage,
    UnsupportedTextureFormat(TextureFormat),
    LimitExceeded,
    SizeOverflow,
}

impl std::fmt::Display for DescriptorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroExtent => formatter.write_str("texture extent must be non-zero"),
            Self::ZeroSize => formatter.write_str("buffer size must be non-zero"),
            Self::ZeroMipLevels => formatter.write_str("texture mip level count must be non-zero"),
            Self::InvalidSampleCount(count) => write!(formatter, "invalid sample count {count}"),
            Self::EmptyUsage => formatter.write_str("resource usage must be non-empty"),
            Self::UnsupportedTextureFormat(format) => {
                write!(formatter, "texture format {format:?} is not supported")
            }
            Self::LimitExceeded => formatter.write_str("resource descriptor exceeds device limits"),
            Self::SizeOverflow => formatter.write_str("resource byte size overflows u64"),
        }
    }
}

impl std::error::Error for DescriptorError {}
