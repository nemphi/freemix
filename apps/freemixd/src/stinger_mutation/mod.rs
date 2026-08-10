mod resources;

pub(crate) use resources::{NativeStingerMutation, NativeStingerRetirements};

#[cfg(test)]
pub(crate) use resources::{native_stinger_requires_ffmpeg, retirement_limit_for_test};
