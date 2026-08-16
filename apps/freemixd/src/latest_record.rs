use std::io::Write;

use super::PendingWrite;

#[derive(Default)]
pub(super) struct LatestRecord {
    current: Option<PendingWrite>,
    next: Option<PendingWrite>,
}

impl LatestRecord {
    pub(super) fn replace(&mut self, bytes: Vec<u8>) {
        let record = PendingWrite::new(bytes);
        if self.started() {
            self.next = Some(record);
        } else {
            self.current = Some(record);
            self.next = None;
        }
    }

    pub(super) fn started(&self) -> bool {
        self.current.as_ref().is_some_and(PendingWrite::started)
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.current.is_none() && self.next.is_none()
    }

    pub(super) fn discard_unstarted(&mut self) {
        self.next = None;
        if !self.started() {
            self.current = None;
        }
    }

    pub(super) fn write_once(&mut self, writer: &mut impl Write) -> std::io::Result<()> {
        if self.current.is_none() {
            self.current = self.next.take();
        }
        let Some(current) = &mut self.current else {
            return Ok(());
        };
        if current.write_once(writer)? {
            self.current = None;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct PartialWriter {
        bytes: Vec<u8>,
        limit: usize,
    }

    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let written = bytes.len().min(self.limit);
            self.bytes.extend_from_slice(&bytes[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn latest_record_replaces_only_unstarted_or_next_data() {
        let mut slot = LatestRecord::default();
        slot.replace(b"old\n".to_vec());
        slot.replace(b"current\n".to_vec());
        let mut writer = PartialWriter {
            bytes: Vec::new(),
            limit: 3,
        };

        slot.write_once(&mut writer).unwrap();
        slot.replace(b"stale\n".to_vec());
        slot.replace(b"latest\n".to_vec());
        slot.write_once(&mut writer).unwrap();
        slot.write_once(&mut writer).unwrap();
        assert!(!slot.started());
        writer.write_all(b"control\n").unwrap();
        while !slot.is_empty() {
            slot.write_once(&mut writer).unwrap();
        }

        assert_eq!(writer.bytes, b"current\ncontrol\nlatest\n");
    }
}
