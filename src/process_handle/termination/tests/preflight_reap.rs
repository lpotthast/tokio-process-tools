use super::*;

#[tokio::test]
async fn preflight_status_error_does_not_stop_termination_escalation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut process = spawn_long_running_process();
    let interrupt_attempted = Arc::new(AtomicBool::new(false));
    let interrupt_attempted_in_sender = Arc::clone(&interrupt_attempted);
    let terminate_attempted = Arc::new(AtomicBool::new(false));
    let terminate_attempted_in_sender = Arc::clone(&terminate_attempted);

    let outcome = process
        .terminate_inner_with_preflight_reaper(
            Duration::from_millis(10),
            Duration::from_millis(10),
            |_| Err(io::Error::other("injected preflight status failure")),
            move |_| {
                interrupt_attempted_in_sender.store(true, Ordering::SeqCst);
                Err(io::Error::other("injected interrupt signal failure"))
            },
            move |_| {
                terminate_attempted_in_sender.store(true, Ordering::SeqCst);
                Err(io::Error::other("injected terminate signal failure"))
            },
        )
        .await
        .unwrap();

    assert_that!(interrupt_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(terminate_attempted.load(Ordering::SeqCst)).is_true();
    assert_that!(outcome.exit_status.success()).is_false();
    assert_that!(process.cleanup_on_drop).is_false();
    assert_that!(&process.panic_on_drop).is_none();
}

#[tokio::test]
async fn send_interrupt_signal_reaps_already_exited_child_before_signalling() {
    let mut process = spawn_immediately_exiting_process();

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_that!(process.id()).is_some();

    process.send_interrupt_signal().unwrap();

    assert_that!(process.id()).is_none();
    assert_that!(process.cleanup_on_drop).is_false();
    assert_that!(&process.panic_on_drop).is_none();
}

#[tokio::test]
async fn send_terminate_signal_reaps_already_exited_child_before_signalling() {
    let mut process = spawn_immediately_exiting_process();

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_that!(process.id()).is_some();

    process.send_terminate_signal().unwrap();

    assert_that!(process.id()).is_none();
    assert_that!(process.cleanup_on_drop).is_false();
    assert_that!(&process.panic_on_drop).is_none();
}

#[tokio::test]
async fn send_signal_reaps_exit_observed_after_signal_failure() {
    let mut process = spawn_long_running_process();
    let mut signal_attempts = 0;
    let mut reap_attempts = 0;

    let result = process.send_signal_with_reaper(
        GracefulTerminationPhase::Interrupt,
        signal::INTERRUPT_SIGNAL_NAME,
        |_| {
            signal_attempts += 1;
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "injected signal failure",
            ))
        },
        |process| {
            reap_attempts += 1;
            match reap_attempts {
                1 => Ok(None),
                2 => {
                    process.must_not_be_terminated();
                    Ok(Some(successful_exit_status()))
                }
                _ => panic!("unexpected reap attempt"),
            }
        },
    );

    assert_that!(result).is_ok();
    assert_that!(signal_attempts).is_equal_to(1);
    assert_that!(reap_attempts).is_equal_to(2);
    assert_that!(process.cleanup_on_drop).is_false();
    assert_that!(&process.panic_on_drop).is_none();

    process.kill().await.unwrap();
}
