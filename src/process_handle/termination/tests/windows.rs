use super::*;
use crate::test_support::{CTRL_BREAK_HELPER_EXIT_CODE, compile_ctrl_break_helper};

#[tokio::test(flavor = "multi_thread")]
async fn windows_interrupt_signal_delivers_targeted_ctrl_break() {
    let temp_dir = tempfile::tempdir().unwrap();
    let helper = compile_ctrl_break_helper(temp_dir.path());
    let cmd = tokio::process::Command::new(helper);
    let mut process = crate::Process::new(cmd)
        .name("ctrl-break-helper")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
        .terminate_on_drop(Duration::from_secs(1), Duration::from_secs(1));

    let ready = process
        .stdout()
        .wait_for_line(
            Duration::from_secs(5),
            |line| line == "ready",
            crate::LineParsingOptions::default(),
        )
        .await
        .unwrap();
    assert_that!(ready).is_equal_to(crate::WaitForLineResult::Matched);

    process.send_interrupt_signal().unwrap();

    let exit_status = process
        .wait_for_completion(Duration::from_secs(5))
        .await
        .unwrap()
        .expect_completed("process should complete");
    assert_that!(exit_status.code())
        .is_some()
        .is_equal_to(CTRL_BREAK_HELPER_EXIT_CODE);
}

#[tokio::test(flavor = "multi_thread")]
async fn windows_terminate_exits_during_first_graceful_phase() {
    let temp_dir = tempfile::tempdir().unwrap();
    let helper = compile_ctrl_break_helper(temp_dir.path());
    let cmd = tokio::process::Command::new(helper);
    let mut process = crate::Process::new(cmd)
        .name("ctrl-break-helper")
        .stdout_and_stderr(|stream| {
            stream
                .broadcast()
                .best_effort_delivery()
                .replay_last_bytes(1.megabytes())
                .read_chunk_size(DEFAULT_READ_CHUNK_SIZE)
                .max_buffered_chunks(DEFAULT_MAX_BUFFERED_CHUNKS)
        })
        .spawn()
        .unwrap()
        .terminate_on_drop(Duration::from_secs(1), Duration::from_secs(1));

    let ready = process
        .stdout()
        .wait_for_line(
            Duration::from_secs(5),
            |line| line == "ready",
            crate::LineParsingOptions::default(),
        )
        .await
        .unwrap();
    assert_that!(ready).is_equal_to(crate::WaitForLineResult::Matched);

    let started_at = std::time::Instant::now();
    let exit_status = process
        .terminate(Duration::from_secs(2), Duration::from_secs(5))
        .await
        .unwrap();

    assert_that!(started_at.elapsed() < Duration::from_secs(2)).is_true();
    assert_that!(exit_status.code())
        .is_some()
        .is_equal_to(CTRL_BREAK_HELPER_EXIT_CODE);
}
