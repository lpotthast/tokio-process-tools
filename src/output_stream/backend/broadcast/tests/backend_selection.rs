use super::super::{Backend, BroadcastOutputStream};
use super::common::{
    best_effort_no_replay_options, best_effort_no_replay_options_with, best_effort_options,
    reliable_no_replay_options, reliable_options,
};
use crate::ReplayRetention;
use assertr::prelude::*;

#[tokio::test]
async fn default_broadcast_uses_tokio_broadcast_fast_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_no_replay_options_with(
            crate::DEFAULT_READ_CHUNK_SIZE,
            crate::DEFAULT_MAX_BUFFERED_CHUNKS,
        ),
    );

    assert_that!(matches!(&stream.backend, Backend::Fast(_))).is_true();
}

#[tokio::test]
async fn typed_best_effort_no_replay_uses_tokio_broadcast_fast_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_no_replay_options(),
    );

    assert_that!(matches!(&stream.backend, Backend::Fast(_))).is_true();
}

#[tokio::test]
async fn reliable_no_replay_uses_fanout_replay_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        reliable_no_replay_options(),
    );

    assert_that!(matches!(&stream.backend, Backend::FanoutReplay(_))).is_true();
}

#[tokio::test]
async fn reliable_replay_uses_fanout_replay_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        reliable_options(ReplayRetention::LastChunks(1)),
    );

    assert_that!(matches!(&stream.backend, Backend::FanoutReplay(_))).is_true();
}

#[tokio::test]
async fn replay_enabled_best_effort_uses_fanout_replay_backend() {
    let stream = BroadcastOutputStream::from_stream(
        tokio::io::empty(),
        "custom",
        best_effort_options(ReplayRetention::LastChunks(1)),
    );

    assert_that!(matches!(&stream.backend, Backend::FanoutReplay(_))).is_true();
    stream.seal_replay();
    assert_that!(stream.is_replay_sealed()).is_true();
}
