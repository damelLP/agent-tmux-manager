//! Register shared support checks in stand_in_harness only, never per consumer.
use crate::stand_in::{
    events::tests as events, process::tests as process, rows::tests as rows,
    test_cleanup as cleanup,
};

macro_rules! register {
    ($module:ident: $($name:ident),+ $(,)?) => {
        $(#[test]
        fn $name() {
            $module::$name();
        })+
    };
}

register!(events:
    stale_start_identity_cannot_match_reused_pid,
    events_replay_without_lost_wake_and_close_with_open_peer,
);
register!(rows: subscription_consumes_snapshot_before_returning_then_updates_and_removal);
register!(process:
    command_exit_returns_complete_large_output_without_pipe_deadlock,
    owned_child_drop_reaps_on_unwind,
);
register!(cleanup:
    stopped_and_replaced_private_servers_clean_up_without_stale_pid,
    vanished_or_reused_process_is_never_signalled,
    detached_daemon_log_provides_lost_handshake_diagnostics,
    foreign_socket_is_refused_without_affecting_either_server,
    drop_retains_run_directory_when_cleanup_refuses_an_identity,
    daemon_error_still_cleans_tmux_and_returns_first_error,
);
