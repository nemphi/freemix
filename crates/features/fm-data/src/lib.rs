//! Typed data sources, transforms, mapping, and cache.
//!
//! This crate deliberately contains contracts and deterministic in-memory
//! adapters only. Network clients, database drivers, and system clocks belong
//! in integration crates.

mod adapters;
mod cache;
mod mapping;
mod retry;
mod schema;
mod secret;
mod source;
mod transform;
mod value;

pub use adapters::{AdapterError, CsvRowsAdapter, DataAdapter, JsonObjectAdapter};
pub use cache::{BoundedCache, CacheError, Clock, ManualClock};
pub use mapping::{FieldBinding, FieldMappingReport, Mapper, MappingReport, MappingStatus};
pub use retry::{RetryError, RetryOutcome, RetryPolicy, RetryRecord, RetryState};
pub use schema::{
    DataPath, PathError, PathSegment, Schema, SchemaExtractError, SchemaType, TypeError,
};
pub use secret::SecretRefId;
pub use source::{
    DataSource, FakePollingSource, FakePushSource, PollingSource, PushSource, SourceError,
    SourceEvent, SourceState,
};
pub use transform::{Transform, TransformError, ValueSelector};
pub use value::{DataValue, Decimal, DecimalError, ValueType};

#[cfg(test)]
mod tests;
