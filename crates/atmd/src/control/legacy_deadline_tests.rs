//! The admission deadline ends at the first effect, not between paste and Enter.
use super::*;
use atm_tmux::{PaneInfo, TmuxError};

#[derive(Clone, Copy)]
enum DelayAt {
    Paste,
    Command,
    State,
}
struct DelayedClient {
    mock: MockTmuxClient,
    at: DelayAt,
}

type Call<'a, T> = std::pin::Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
// Use the trait's boxed-future ABI, as the consumer scripted client does.
macro_rules! forward {
    ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
        fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
        where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
            Box::pin(self.mock.$method($($arg),*))
        }
    };
}
impl TmuxClient for DelayedClient {
    fn raw_command<'s, 'c, 'a, 'v, 'async_trait>(
        &'s self,
        command: &'c str,
        args: &'a [&'v str],
    ) -> Call<'async_trait, String>
    where
        's: 'async_trait,
        'c: 'async_trait,
        'a: 'async_trait,
        'v: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let result = self.mock.raw_command(command, args).await;
            if matches!(self.at, DelayAt::State)
                && args.contains(&"#{pane_in_mode} #{bracket_paste_flag}")
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            result
        })
    }
    fn paste_text<'s, 'p, 't, 'async_trait>(
        &'s self,
        pane: &'p str,
        text: &'t str,
    ) -> Call<'async_trait, ()>
    where
        's: 'async_trait,
        'p: 'async_trait,
        't: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let result = self.mock.paste_text(pane, text).await;
            if matches!(self.at, DelayAt::Paste) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            result
        })
    }
    fn send_keys<'s, 'p, 'k, 'async_trait>(
        &'s self,
        pane: &'p str,
        keys: &'k str,
    ) -> Call<'async_trait, ()>
    where
        's: 'async_trait,
        'p: 'async_trait,
        'k: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let result = self.mock.send_keys(pane, keys).await;
            if matches!(self.at, DelayAt::Command) && keys != "Enter" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            result
        })
    }
    forward!(split_window<'a, 'b, 'c, 'd>(target: &'a str, size: &'b str, direction: PaneDirection, command: Option<&'c str>, cwd: Option<&'d str>, detached: bool) -> String);
    forward!(get_pane_cwd<'a>(pane: &'a str) -> Option<String>);
    forward!(new_window<'a, 'b>(session: &'a str, command: Option<&'b str>) -> String);
    forward!(kill_pane<'a>(pane: &'a str) -> ());
    forward!(resize_pane<'a>(pane: &'a str, width: Option<u16>, height: Option<u16>) -> ());
    forward!(list_panes<>() -> Vec<PaneInfo>);
    forward!(display_popup<'a, 'b, 'c>(width: &'a str, height: &'b str, command: &'c str) -> ());
    forward!(select_pane<'a>(pane: &'a str) -> ());
    forward!(capture_pane<'a>(pane: &'a str) -> Vec<String>);
    forward!(new_session<'a>(name: &'a str) -> String);
}

async fn near_deadline(at: DelayAt) {
    let mock = MockTmuxClient::new();
    let cancel = CancellationToken::new();
    let (control, actor) = spawn_control(
        Box::new(DelayedClient {
            mock: mock.clone(),
            at,
        }),
        crate::registry::spawn_registry(),
        OrgHandle::disabled("test"),
        cancel.clone(),
    );
    let (job, release) = hold(&control, ReservationKind::Delivery).await;
    let c = control.clone();
    let send = queued(async move {
        if matches!(at, DelayAt::Command) {
            c.type_spawn_command("%1".into(), "half".into())
                .await
                .map(|()| "%1".to_string())
        } else {
            c.send("%1".into(), "half".into()).await
        }
    })
    .await;
    barrier(&control).await;
    assert!(input_calls(&mock).is_empty());
    tokio::time::advance(Duration::from_millis(29_999)).await;
    untagged(&mock, "%1");
    mock.set_next_raw_output("0 1");
    release.send(()).unwrap();
    job.await.unwrap().unwrap();
    let result = send.await.unwrap();
    // The caller's original response budget is unchanged. The actor must finish
    // the admitted input even after that caller timed out and dropped its wait.
    assert!(matches!(result, Err(ServiceError::Timeout)));
    control.flush_audits().await;
    if matches!(at, DelayAt::State) {
        assert!(input_calls(&mock).is_empty());
    } else {
        let first = if matches!(at, DelayAt::Command) {
            MockCall::SendKeys {
                pane: "%1".into(),
                keys: "half".into(),
            }
        } else {
            MockCall::PasteText {
                pane: "%1".into(),
                text: "half".into(),
            }
        };
        assert_eq!(
            input_calls(&mock),
            vec![
                first,
                MockCall::SendKeys {
                    pane: "%1".into(),
                    keys: "Enter".into()
                }
            ]
        );
    }
    cancel.cancel();
    actor.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn reservation_parked_paste_near_deadline_still_submits() {
    near_deadline(DelayAt::Paste).await;
}
#[tokio::test(start_paused = true)]
async fn reservation_parked_spawn_near_deadline_still_submits() {
    near_deadline(DelayAt::Command).await;
}
#[tokio::test(start_paused = true)]
async fn reservation_parked_state_read_past_deadline_never_pastes() {
    near_deadline(DelayAt::State).await;
}
