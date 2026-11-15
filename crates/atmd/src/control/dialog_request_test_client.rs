//! Hold one external read/key on an explicit barrier; delegate all other effects.
use atm_tmux::{PaneDirection, PaneInfo, TmuxClient, TmuxError};
use std::{future::Future, pin::Pin, sync::Arc};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy)]
pub(super) enum At {
    Server,
    Capture,
    Navigation,
    OtherFrame,
}
pub(super) struct Paused {
    pub inner: Box<dyn TmuxClient>,
    pub at: At,
    pub entered: Arc<Notify>,
    pub release: Arc<Semaphore>,
}
impl Paused {
    async fn wait(&self) {
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
    }
}
type Call<'a, T> = Pin<Box<dyn Future<Output = Result<T, TmuxError>> + Send + 'a>>;
macro_rules! forward {
    ($method:ident <$($lt:lifetime),*> ($($arg:ident : $ty:ty),*) -> $result:ty) => {
        fn $method<'s, $($lt,)* 'async_trait>(&'s self, $($arg: $ty),*) -> Call<'async_trait, $result>
        where 's: 'async_trait, $($lt: 'async_trait,)* Self: 'async_trait {
            Box::pin(self.inner.$method($($arg),*))
        }
    };
}
impl TmuxClient for Paused {
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
            let result = self.inner.raw_command(command, args).await;
            if (matches!(self.at, At::Server) && args.contains(&"#{socket_path}"))
                || (matches!(self.at, At::OtherFrame)
                    && args.contains(&"capture-pane")
                    && args.contains(&"%2"))
            {
                self.wait().await;
            }
            result
        })
    }
    fn capture_pane<'s, 'p, 'async_trait>(
        &'s self,
        pane: &'p str,
    ) -> Call<'async_trait, Vec<String>>
    where
        's: 'async_trait,
        'p: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let result = self.inner.capture_pane(pane).await;
            if matches!(self.at, At::Capture) {
                self.wait().await;
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
            let result = self.inner.send_keys(pane, keys).await;
            if matches!(self.at, At::Navigation) && keys != "Enter" && keys != "C-c" {
                self.wait().await;
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
    forward!(paste_text<'a, 'b>(pane: &'a str, text: &'b str) -> ());
    forward!(new_session<'a>(name: &'a str) -> String);
}
