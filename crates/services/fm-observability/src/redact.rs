use std::net::{Ipv4Addr, Ipv6Addr};

/// Stateless, deterministic redaction for operational text.
#[derive(Clone, Copy, Debug, Default)]
pub struct Redactor;

impl Redactor {
    pub const SECRET_MARKER: &'static str = "[REDACTED_SECRET]";
    pub const PATH_MARKER: &'static str = "[REDACTED_PATH]";
    pub const IP_MARKER: &'static str = "[REDACTED_IP]";

    /// Redacts common inline credentials, absolute paths, and IP addresses.
    #[must_use]
    pub fn redact(self, value: &str) -> String {
        let value = redact_secrets(value);
        let value = redact_paths(&value);
        redact_ips(&value)
    }

    /// Redacts the entire value when its structured field name is sensitive.
    #[must_use]
    pub fn redact_field(self, name: &str, value: &str) -> String {
        if self.is_secret_name(name) {
            Self::SECRET_MARKER.to_owned()
        } else {
            self.redact(value)
        }
    }

    #[must_use]
    pub fn is_secret_name(self, name: &str) -> bool {
        let normalized: String = name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|character| character.to_ascii_lowercase())
            .collect();
        [
            "authorization",
            "password",
            "passwd",
            "token",
            "apikey",
            "secret",
            "streamkey",
            "credential",
            "cookie",
            "privatekey",
        ]
        .iter()
        .any(|secret| normalized.contains(secret))
    }
}

fn redact_secrets(value: &str) -> String {
    const NAMES: [&str; 10] = [
        "authorization",
        "password",
        "passwd",
        "api_key",
        "api-key",
        "stream_key",
        "credential",
        "cookie",
        "token",
        "secret",
    ];

    let lower = value.to_ascii_lowercase();
    let bytes = value.as_bytes();
    let mut ranges = Vec::new();
    for name in NAMES {
        for (start, _) in lower.match_indices(name) {
            let name_end = start + name.len();
            if !is_name_boundary(bytes, start, name_end) {
                continue;
            }
            let mut separator = name_end;
            while bytes
                .get(separator)
                .is_some_and(|byte| byte.is_ascii_whitespace() || *byte == b'"' || *byte == b'\'')
            {
                separator += 1;
            }
            if !matches!(bytes.get(separator), Some(b':' | b'=')) {
                continue;
            }
            let mut secret_start = separator + 1;
            while bytes.get(secret_start).is_some_and(u8::is_ascii_whitespace) {
                secret_start += 1;
            }
            let quote = bytes
                .get(secret_start)
                .copied()
                .filter(|byte| matches!(byte, b'"' | b'\''));
            if quote.is_some() {
                secret_start += 1;
            }
            let mut secret_end = secret_start;
            if name == "authorization" && lower[secret_start..].starts_with("bearer ") {
                secret_end += "bearer ".len();
            }
            while let Some(byte) = bytes.get(secret_end) {
                let end = if let Some(quote) = quote {
                    *byte == quote
                } else if name == "cookie" {
                    matches!(byte, b',' | b';' | b'\n' | b'\r')
                } else {
                    byte.is_ascii_whitespace() || matches!(byte, b'&' | b',' | b';' | b'"' | b'\'')
                };
                if end {
                    break;
                }
                secret_end += 1;
            }
            if secret_start < secret_end {
                ranges.push((secret_start, secret_end, Redactor::SECRET_MARKER));
            }
        }
    }

    for (start, _) in lower.match_indices("bearer ") {
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let secret_start = start + "bearer ".len();
        let secret_end = bytes[secret_start..]
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b'"'))
            .map_or(bytes.len(), |offset| secret_start + offset);
        if secret_start < secret_end {
            ranges.push((secret_start, secret_end, Redactor::SECRET_MARKER));
        }
    }

    // URL user-info (`scheme://user:password@host`) is always credential data.
    for (scheme, _) in value.match_indices("://") {
        let authority_start = scheme + 3;
        let authority_end = bytes[authority_start..]
            .iter()
            .position(|byte| matches!(byte, b'/' | b'?' | b'#') || byte.is_ascii_whitespace())
            .map_or(bytes.len(), |offset| authority_start + offset);
        if let Some(at) = value[authority_start..authority_end].find('@') {
            ranges.push((
                authority_start,
                authority_start + at,
                Redactor::SECRET_MARKER,
            ));
        }
    }

    replace_ranges(value, ranges)
}

fn is_name_boundary(bytes: &[u8], start: usize, end: usize) -> bool {
    let valid_start =
        start == 0 || !bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_';
    let valid_end = end == bytes.len() || !bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_';
    valid_start && valid_end
}

fn redact_paths(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let windows = index + 2 < bytes.len()
            && bytes[index].is_ascii_alphabetic()
            && bytes[index + 1] == b':'
            && matches!(bytes[index + 2], b'/' | b'\\');
        let unc = bytes[index..].starts_with(b"\\\\");
        let posix = bytes[index] == b'/'
            && bytes
                .get(index.wrapping_sub(1))
                .is_none_or(|byte| *byte != b':')
            && bytes.get(index + 1).is_some_and(|byte| *byte != b'/');
        let boundary = index == 0
            || bytes[index - 1].is_ascii_whitespace()
            || matches!(bytes[index - 1], b'"' | b'\'' | b'=' | b'(' | b'[');

        if boundary && (windows || unc || posix) {
            let quote = index
                .checked_sub(1)
                .and_then(|previous| bytes.get(previous))
                .copied()
                .filter(|byte| matches!(byte, b'"' | b'\''));
            let mut end = index;
            while let Some(byte) = bytes.get(end) {
                let reached_end = quote.map_or_else(
                    || byte.is_ascii_whitespace() || matches!(byte, b',' | b';' | b')' | b']'),
                    |quote| *byte == quote,
                );
                if reached_end {
                    break;
                }
                end += 1;
            }
            ranges.push((index, end, Redactor::PATH_MARKER));
            index = end;
        } else {
            index += 1;
        }
    }
    replace_ranges(value, ranges)
}

fn redact_ips(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some(close_offset) = value[index + 1..].find(']')
        {
            let end = index + 1 + close_offset;
            let candidate = &value[index + 1..end];
            if parse_ipv6(candidate) {
                ranges.push((index, end + 1, Redactor::IP_MARKER));
                index = end + 1;
                continue;
            }
        }

        if bytes[index].is_ascii_digit()
            && (index == 0 || !bytes[index - 1].is_ascii_alphanumeric())
        {
            let mut end = index;
            while bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
            {
                end += 1;
            }
            let candidate = &value[index..end];
            if candidate.contains('.') && candidate.parse::<Ipv4Addr>().is_ok() {
                ranges.push((index, end, Redactor::IP_MARKER));
                index = end;
                continue;
            }
        }

        let ipv6_start = bytes[index] == b':' || bytes[index].is_ascii_hexdigit();
        let boundary =
            index == 0 || !bytes[index - 1].is_ascii_alphanumeric() && bytes[index - 1] != b':';
        if ipv6_start && boundary {
            let mut end = index;
            while bytes.get(end).is_some_and(|byte| {
                byte.is_ascii_hexdigit()
                    || *byte == b':'
                    || *byte == b'%'
                    || (value[index..end].contains('%') && byte.is_ascii_alphanumeric())
            }) {
                end += 1;
            }
            let candidate = &value[index..end];
            if candidate.matches(':').count() >= 2 && parse_ipv6(candidate) {
                ranges.push((index, end, Redactor::IP_MARKER));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    replace_ranges(value, ranges)
}

fn parse_ipv6(candidate: &str) -> bool {
    candidate
        .split_once('%')
        .map_or(candidate, |(address, _)| address)
        .parse::<Ipv6Addr>()
        .is_ok()
}

fn replace_ranges(value: &str, mut ranges: Vec<(usize, usize, &str)>) -> String {
    ranges.sort_unstable_by_key(|range| (range.0, range.1));
    let mut output = String::with_capacity(value.len());
    let mut copied = 0;
    for (start, end, marker) in ranges {
        if start < copied || start >= end || end > value.len() {
            continue;
        }
        output.push_str(&value[copied..start]);
        output.push_str(marker);
        copied = end;
    }
    output.push_str(&value[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_secret_path_and_ip_traps() {
        let input = concat!(
            "password=hunter2 Authorization: Bearer abc.def ",
            "url=https://alice:p4ss@example.test/live?token=query-secret ",
            "posix=\"/Users/alice/My Show/key.mov\" win='C:\\Users\\alice\\show.mov' ",
            "peers=192.168.1.9:9000,[2001:db8::1]:443,::1"
        );
        let redacted = Redactor.redact(input);

        for leaked in [
            "hunter2",
            "abc.def",
            "alice:p4ss",
            "query-secret",
            "/Users/alice",
            "C:\\Users",
            "192.168.1.9",
            "2001:db8::1",
            "::1",
        ] {
            assert!(!redacted.contains(leaked), "leaked {leaked}: {redacted}");
        }
        assert!(redacted.contains(Redactor::SECRET_MARKER));
        assert!(redacted.contains(Redactor::PATH_MARKER));
        assert!(redacted.contains(Redactor::IP_MARKER));
    }

    #[test]
    fn structured_secret_names_mask_non_textual_looking_values() {
        assert_eq!(
            Redactor.redact_field("stream_key", "12345"),
            Redactor::SECRET_MARKER
        );
        assert_eq!(Redactor.redact("frame=12345"), "frame=12345");
    }
}
