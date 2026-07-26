use std::{io::Cursor, num::NonZeroU128};

use fm_frame::ClockDomainId;
use fm_io_macos::protocol::FrameReader;

// Queue drop-oldest accounting is exercised by the crate's internal queue test
// path; this integration test keeps a portable multi-record parser regression.
#[test]
fn portable_fake_framed_child_stream_is_continuously_drained() {
    let mut stream = b"FMCAMF3\0".to_vec();
    for sequence in 0_u64..10 {
        let payload = [0, 0, 0, 255];
        stream.extend_from_slice(&62_u32.to_le_bytes());
        stream.extend_from_slice(&sequence.to_le_bytes());
        stream.extend_from_slice(&(sequence / 3).to_le_bytes());
        stream.extend_from_slice(&i64::try_from(sequence).unwrap().to_le_bytes());
        stream.extend_from_slice(&1_000_i32.to_le_bytes());
        stream.extend_from_slice(&1_i64.to_le_bytes());
        stream.extend_from_slice(&1_000_i32.to_le_bytes());
        stream.extend_from_slice(&1_u32.to_le_bytes());
        stream.extend_from_slice(&1_u32.to_le_bytes());
        stream.extend_from_slice(&4_u32.to_le_bytes());
        stream.extend_from_slice(&4_u32.to_le_bytes());
        stream.extend_from_slice(&[1, 1]);
        stream.extend_from_slice(&payload);
    }
    let clock = ClockDomainId::new(NonZeroU128::new(1).unwrap());
    let mut reader = FrameReader::new(Cursor::new(stream), clock).unwrap();
    let mut count = 0;
    while reader.read_frame().unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 10);
}
