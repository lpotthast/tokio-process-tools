use super::*;
use crate::{GracefulTimeouts, both};

#[test]
fn fluent_with_distinct_unix_phases_populates_platform_relevant_fields() {
    let timeouts = GracefulTimeouts::builder()
        .unix((Duration::from_secs(2), Duration::from_secs(5)))
        .windows(Duration::from_secs(7))
        .build();

    #[cfg(unix)]
    {
        assert_that!(timeouts.interrupt_timeout).is_equal_to(Duration::from_secs(2));
        assert_that!(timeouts.terminate_timeout).is_equal_to(Duration::from_secs(5));
    }
    #[cfg(windows)]
    {
        assert_that!(timeouts.graceful_timeout).is_equal_to(Duration::from_secs(7));
    }
}

#[test]
fn fluent_with_both_helper_yields_equal_unix_phases() {
    let timeouts = GracefulTimeouts::builder()
        .unix(both(Duration::from_secs(3)))
        .windows(Duration::from_secs(7))
        .build();

    #[cfg(unix)]
    {
        assert_that!(timeouts.interrupt_timeout).is_equal_to(Duration::from_secs(3));
        assert_that!(timeouts.terminate_timeout).is_equal_to(Duration::from_secs(3));
    }
    #[cfg(windows)]
    {
        assert_that!(timeouts.graceful_timeout).is_equal_to(Duration::from_secs(7));
    }
}

#[test]
fn fluent_value_equals_literal_struct_with_same_inputs() {
    let fluent = GracefulTimeouts::builder()
        .unix((Duration::from_secs(2), Duration::from_secs(5)))
        .windows(Duration::from_secs(7))
        .build();

    #[cfg(unix)]
    let literal = GracefulTimeouts {
        interrupt_timeout: Duration::from_secs(2),
        terminate_timeout: Duration::from_secs(5),
    };
    #[cfg(windows)]
    let literal = GracefulTimeouts {
        graceful_timeout: Duration::from_secs(7),
    };

    assert_that!(fluent).is_equal_to(literal);
}

#[test]
fn both_helper_returns_pair_of_equal_durations() {
    let pair = both(Duration::from_millis(250));
    assert_that!(pair.0).is_equal_to(Duration::from_millis(250));
    assert_that!(pair.1).is_equal_to(Duration::from_millis(250));
}
