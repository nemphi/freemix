#![allow(clippy::float_cmp)]

use fm_audio::{
    MAX_CHANNELS, MAX_SAMPLE_DELAY_SAMPLES, MAX_SAMPLES_PER_BLOCK, SampleDelay, SampleDelayError,
    SampleDelaySide,
};

fn process(delay: &mut SampleDelay, input: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let samples = input[0].len();
    let mut output = vec![vec![0.0; samples]; input.len()];
    delay.process_into(input, &mut output).unwrap();
    output
}

#[test]
fn zero_delay_copies_exact_samples() {
    let mut delay = SampleDelay::new(2, 0).unwrap();
    let input = vec![vec![1.0, -2.0, 3.5], vec![-4.0, 5.0, 6.0]];

    assert_eq!(delay.channels(), 2);
    assert_eq!(delay.delay_samples(), 0);
    assert_eq!(process(&mut delay, &input), input);
}

#[test]
fn impulse_has_exact_leading_silence_across_blocks() {
    let mut delay = SampleDelay::new(1, 3).unwrap();

    assert_eq!(process(&mut delay, &[vec![1.0, 0.0]]), vec![vec![0.0, 0.0]]);
    assert_eq!(
        process(&mut delay, &[vec![0.0, 0.0, 0.0]]),
        vec![vec![0.0, 1.0, 0.0]]
    );
}

#[test]
fn channels_retain_independent_history() {
    let mut delay = SampleDelay::new(3, 2).unwrap();
    let input = vec![
        vec![1.0, 2.0, 3.0, 4.0],
        vec![10.0, 20.0, 30.0, 40.0],
        vec![-1.0, -2.0, -3.0, -4.0],
    ];

    assert_eq!(
        process(&mut delay, &input),
        vec![
            vec![0.0, 0.0, 1.0, 2.0],
            vec![0.0, 0.0, 10.0, 20.0],
            vec![0.0, 0.0, -1.0, -2.0],
        ]
    );
}

#[test]
fn arbitrary_block_partitions_produce_the_same_stream() {
    let input = vec![
        (0_u16..37).map(|sample| f32::from(sample) - 9.0).collect(),
        (0_u16..37)
            .map(|sample| f32::from(sample) * -0.25)
            .collect(),
    ];
    let mut contiguous = SampleDelay::new(2, 7).unwrap();
    let expected = process(&mut contiguous, &input);

    let mut partitioned = SampleDelay::new(2, 7).unwrap();
    let mut actual = vec![Vec::new(), Vec::new()];
    let mut offset = 0;
    for samples in [1, 6, 0, 9, 3, 18] {
        let block: Vec<Vec<f32>> = input
            .iter()
            .map(|plane| plane[offset..offset + samples].to_vec())
            .collect();
        let output = process(&mut partitioned, &block);
        for (stream, block) in actual.iter_mut().zip(output) {
            stream.extend(block);
        }
        offset += samples;
    }

    assert_eq!(offset, 37);
    assert_eq!(actual, expected);
}

#[test]
fn reset_restores_leading_silence() {
    let mut delay = SampleDelay::new(1, 2).unwrap();
    assert_eq!(
        process(&mut delay, &[vec![1.0, 2.0, 3.0]]),
        vec![vec![0.0, 0.0, 1.0]]
    );

    delay.reset();

    assert_eq!(
        process(&mut delay, &[vec![4.0, 5.0, 6.0]]),
        vec![vec![0.0, 0.0, 4.0]]
    );
}

#[test]
fn invalid_operations_leave_state_and_output_unchanged() {
    let mut delay = SampleDelay::new(2, 3).unwrap();
    let mut reference = delay.clone();
    let priming = vec![vec![1.0, 2.0], vec![10.0, 20.0]];
    assert_eq!(
        process(&mut delay, &priming),
        process(&mut reference, &priming)
    );

    let mut wrong_input_count = vec![vec![7.0; 2]; 2];
    assert_eq!(
        delay.process_into(&[vec![3.0, 4.0]], &mut wrong_input_count),
        Err(SampleDelayError::PlaneCountMismatch {
            side: SampleDelaySide::Input,
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(wrong_input_count, vec![vec![7.0; 2]; 2]);

    let mut wrong_output_count = vec![vec![8.0; 2]];
    assert_eq!(
        delay.process_into(&priming, &mut wrong_output_count),
        Err(SampleDelayError::PlaneCountMismatch {
            side: SampleDelaySide::Output,
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(wrong_output_count, vec![vec![8.0; 2]]);

    let mut output = vec![vec![9.0; 2]; 2];
    assert_eq!(
        delay.process_into(&[vec![3.0, 4.0], vec![5.0]], &mut output),
        Err(SampleDelayError::PlaneLengthMismatch {
            side: SampleDelaySide::Input,
            plane: 1,
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(output, vec![vec![9.0; 2]; 2]);

    let mut short_output = vec![vec![6.0; 2], vec![6.0]];
    assert_eq!(
        delay.process_into(&priming, &mut short_output),
        Err(SampleDelayError::PlaneLengthMismatch {
            side: SampleDelaySide::Output,
            plane: 1,
            expected: 2,
            actual: 1,
        })
    );
    assert_eq!(short_output, vec![vec![6.0; 2], vec![6.0]]);

    let mut non_finite_output = vec![vec![5.0; 2]; 2];
    assert_eq!(
        delay.process_into(
            &[vec![3.0, f32::NAN], vec![f32::INFINITY, 4.0]],
            &mut non_finite_output,
        ),
        Err(SampleDelayError::NonFiniteInput {
            channel: 0,
            sample: 1,
        })
    );
    assert_eq!(non_finite_output, vec![vec![5.0; 2]; 2]);

    let oversized = vec![vec![0.0; MAX_SAMPLES_PER_BLOCK + 1]; 2];
    let mut oversized_output = vec![vec![4.0; MAX_SAMPLES_PER_BLOCK + 1]; 2];
    assert_eq!(
        delay.process_into(&oversized, &mut oversized_output),
        Err(SampleDelayError::SampleCountOutOfRange {
            actual: MAX_SAMPLES_PER_BLOCK + 1,
            maximum: MAX_SAMPLES_PER_BLOCK,
        })
    );
    assert!(
        oversized_output
            .iter()
            .flatten()
            .all(|sample| *sample == 4.0)
    );

    let continuation = vec![vec![3.0, 4.0, 5.0], vec![30.0, 40.0, 50.0]];
    assert_eq!(
        process(&mut delay, &continuation),
        process(&mut reference, &continuation)
    );
}

#[test]
fn construction_enforces_maximums_and_checks_extreme_arithmetic() {
    assert!(SampleDelay::new(MAX_CHANNELS, MAX_SAMPLE_DELAY_SAMPLES).is_ok());
    assert_eq!(
        SampleDelay::new(0, 0),
        Err(SampleDelayError::ChannelCountOutOfRange {
            actual: 0,
            maximum: MAX_CHANNELS,
        })
    );
    assert_eq!(
        SampleDelay::new(MAX_CHANNELS + 1, 0),
        Err(SampleDelayError::ChannelCountOutOfRange {
            actual: MAX_CHANNELS + 1,
            maximum: MAX_CHANNELS,
        })
    );
    assert_eq!(
        SampleDelay::new(1, MAX_SAMPLE_DELAY_SAMPLES + 1),
        Err(SampleDelayError::DelayOutOfRange {
            actual: MAX_SAMPLE_DELAY_SAMPLES + 1,
            maximum: MAX_SAMPLE_DELAY_SAMPLES,
        })
    );
    assert_eq!(
        SampleDelay::new(MAX_CHANNELS, usize::MAX),
        Err(SampleDelayError::AllocationOverflow)
    );
}
