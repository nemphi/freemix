use core::{fmt, num::NonZeroU128};

macro_rules! domain_id {
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

domain_id!(ProjectId);
domain_id!(InputId);
domain_id!(SceneId);
domain_id!(BusId);
domain_id!(OutputId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_preserve_nonzero_value() {
        let value = NonZeroU128::new(42).unwrap();
        let id = InputId::new(value);
        assert_eq!(id.get(), value);
        assert_eq!(id.to_string(), "42");
    }
}
