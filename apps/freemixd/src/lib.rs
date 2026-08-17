use std::{error::Error, fmt, net::SocketAddr, num::NonZeroU128, str::FromStr};

use fm_types::ProjectId;

#[cfg(feature = "native-media")]
pub mod native_media;

const READY_PREFIX: &str = "FREEMIXD_READY";
const STATUS_READY_PREFIX: &str = "FREEMIXD_STATUS_READY";
const READY_VERSION: &str = "v=1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadinessRecord {
    pub address: SocketAddr,
    pub project_id: ProjectId,
}

impl fmt::Display for ReadinessRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{READY_PREFIX}\t{READY_VERSION}\taddress={}\tproject_id={}",
            self.address, self.project_id
        )
    }
}

impl FromStr for ReadinessRecord {
    type Err = ReadinessParseError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut fields = line.split('\t');
        if fields.next() != Some(READY_PREFIX) || fields.next() != Some(READY_VERSION) {
            return Err(ReadinessParseError);
        }
        let address = fields
            .next()
            .and_then(|field| field.strip_prefix("address="))
            .and_then(|address| address.parse().ok())
            .ok_or(ReadinessParseError)?;
        let project_id = fields
            .next()
            .and_then(|field| field.strip_prefix("project_id="))
            .and_then(|project_id| project_id.parse::<NonZeroU128>().ok())
            .map(ProjectId::new)
            .ok_or(ReadinessParseError)?;
        if fields.next().is_some() {
            return Err(ReadinessParseError);
        }
        Ok(Self {
            address,
            project_id,
        })
    }
}

/// The operator status listener address, announced once the daemon is ready.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusReadinessRecord {
    pub address: SocketAddr,
}

impl fmt::Display for StatusReadinessRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{STATUS_READY_PREFIX}\t{READY_VERSION}\taddress={}",
            self.address
        )
    }
}

impl FromStr for StatusReadinessRecord {
    type Err = ReadinessParseError;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let line = line.trim_end_matches(['\r', '\n']);
        let mut fields = line.split('\t');
        if fields.next() != Some(STATUS_READY_PREFIX) || fields.next() != Some(READY_VERSION) {
            return Err(ReadinessParseError);
        }
        let address = fields
            .next()
            .and_then(|field| field.strip_prefix("address="))
            .and_then(|address| address.parse().ok())
            .ok_or(ReadinessParseError)?;
        if fields.next().is_some() {
            return Err(ReadinessParseError);
        }
        Ok(Self { address })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadinessParseError;

impl fmt::Display for ReadinessParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid freemixd readiness record")
    }
}

impl Error for ReadinessParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_record_round_trips_full_project_id() {
        let record = ReadinessRecord {
            address: "127.0.0.1:32123".parse().unwrap(),
            project_id: ProjectId::new(NonZeroU128::new(18_446_744_073_709_551_657).unwrap()),
        };
        let line = record.to_string();

        assert_eq!(
            line,
            "FREEMIXD_READY\tv=1\taddress=127.0.0.1:32123\tproject_id=18446744073709551657"
        );
        assert_eq!(format!("{line}\n").parse(), Ok(record));
    }

    #[test]
    fn readiness_record_rejects_other_shapes_and_versions() {
        for line in [
            "LISTENING 127.0.0.1:32123",
            "FREEMIXD_READY\tv=2\taddress=127.0.0.1:32123\tproject_id=42",
            "FREEMIXD_READY\tv=1\tproject_id=42\taddress=127.0.0.1:32123",
            "FREEMIXD_READY\tv=1\taddress=127.0.0.1:32123\tproject_id=0",
            "FREEMIXD_READY\tv=1\taddress=127.0.0.1:32123\tproject_id=42\textra=x",
        ] {
            assert_eq!(line.parse::<ReadinessRecord>(), Err(ReadinessParseError));
        }
    }
}
