//! Builder API for spawning processes with explicit stream consumption configuration.

mod builder;
mod name;
mod stream_config;

pub use builder::{
    ConfiguredProcessBuilder, NamedProcess, Process, ProcessBuilderWithStderr,
    ProcessBuilderWithStdout,
};
pub use name::{AutoName, AutoNameSettings, ProcessName};
pub use stream_config::{ProcessStreamBuilder, ProcessStreamConfig};
