// The production backend is capability-gated with native Slack joining. The
// lifecycle is exercised by the synthetic harness until that path is enabled.
#![allow(dead_code)]

use std::fmt;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::future::{select, Either};
use tokio::sync::watch;

pub type PortalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalStream {
    pipe_wire_node_id: u32,
}

impl PortalStream {
    pub fn new(pipe_wire_node_id: u32) -> Self {
        Self { pipe_wire_node_id }
    }

    pub fn pipe_wire_node_id(self) -> u32 {
        self.pipe_wire_node_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PortalError {
    #[error("screen sharing was cancelled")]
    Cancelled,
    #[error("screen sharing permission was not granted")]
    PermissionDenied,
    #[error("the screen sharing portal returned an invalid response")]
    InvalidResponse,
    #[error("the screen sharing portal is unavailable")]
    Unavailable,
    #[error("the screen sharing portal operation failed")]
    OperationFailed,
}

pub trait ScreenCastBackend: Send + Sync + 'static {
    type Session: Send + Sync + 'static;
    type Parent: Send + Sync + 'static;

    fn create_session(&self) -> PortalFuture<'_, Result<Self::Session, PortalError>>;
    fn select_sources<'a>(
        &'a self,
        session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<(), PortalError>>;
    fn start<'a>(
        &'a self,
        session: &'a Self::Session,
        parent: Option<&'a Self::Parent>,
    ) -> PortalFuture<'a, Result<Vec<PortalStream>, PortalError>>;
    fn open_remote<'a>(
        &'a self,
        session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<OwnedFd, PortalError>>;
    fn close<'a>(&'a self, session: &'a Self::Session)
        -> PortalFuture<'a, Result<(), PortalError>>;
    fn wait_closed<'a>(
        &'a self,
        session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<(), PortalError>>;
}

pub struct ScreenCastLease<B: ScreenCastBackend> {
    backend: Arc<B>,
    session: Option<B::Session>,
    remote_fd: Option<OwnedFd>,
    node_id: u32,
}

impl<B: ScreenCastBackend> ScreenCastLease<B> {
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn duplicate_remote_fd(&self) -> Result<OwnedFd, PortalError> {
        self.remote_fd
            .as_ref()
            .ok_or(PortalError::OperationFailed)?
            .try_clone()
            .map_err(|_| PortalError::OperationFailed)
    }

    pub async fn close(mut self) -> Result<(), PortalError> {
        let session = self.session.take();
        let remote_fd = self.remote_fd.take();
        let result = match session.as_ref() {
            Some(session) => self.backend.close(session).await,
            None => Ok(()),
        };
        drop(remote_fd);
        result
    }

    pub async fn wait_closed(&self) -> Result<(), PortalError> {
        let session = self.session.as_ref().ok_or(PortalError::OperationFailed)?;
        self.backend.wait_closed(session).await
    }
}

impl<B: ScreenCastBackend> fmt::Debug for ScreenCastLease<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScreenCastLease")
            .field("session", &self.session.as_ref().map(|_| "<redacted>"))
            .field("remote_fd", &self.remote_fd.as_ref().map(|_| "<redacted>"))
            .field("node_id", &self.node_id)
            .finish()
    }
}

impl<B: ScreenCastBackend> Drop for ScreenCastLease<B> {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        let remote_fd = self.remote_fd.take();
        let backend = Arc::clone(&self.backend);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = backend.close(&session).await;
                drop(remote_fd);
            });
        }
    }
}

#[cfg(any(test, feature = "huddle-harness"))]
#[derive(Debug, Default)]
pub struct SyntheticScreenCastBackend;

#[cfg(any(test, feature = "huddle-harness"))]
impl ScreenCastBackend for SyntheticScreenCastBackend {
    type Session = ();
    type Parent = ();

    fn create_session(&self) -> PortalFuture<'_, Result<Self::Session, PortalError>> {
        Box::pin(async { Ok(()) })
    }

    fn select_sources<'a>(
        &'a self,
        _session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<(), PortalError>> {
        Box::pin(async { Ok(()) })
    }

    fn start<'a>(
        &'a self,
        _session: &'a Self::Session,
        _parent: Option<&'a Self::Parent>,
    ) -> PortalFuture<'a, Result<Vec<PortalStream>, PortalError>> {
        Box::pin(async { Ok(vec![PortalStream::new(42)]) })
    }

    fn open_remote<'a>(
        &'a self,
        _session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<OwnedFd, PortalError>> {
        Box::pin(async {
            let (remote, _peer) =
                std::os::unix::net::UnixStream::pair().map_err(|_| PortalError::OperationFailed)?;
            Ok(remote.into())
        })
    }

    fn close<'a>(
        &'a self,
        _session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<(), PortalError>> {
        Box::pin(async { Ok(()) })
    }

    fn wait_closed<'a>(
        &'a self,
        _session: &'a Self::Session,
    ) -> PortalFuture<'a, Result<(), PortalError>> {
        Box::pin(std::future::pending())
    }
}

pub async fn request_screen_cast<B: ScreenCastBackend>(
    backend: Arc<B>,
    parent: Option<&B::Parent>,
    mut cancellation: watch::Receiver<bool>,
) -> Result<ScreenCastLease<B>, PortalError> {
    let session = backend.create_session().await?;
    if cancellation_requested(&cancellation) {
        let _ = backend.close(&session).await;
        return Err(PortalError::Cancelled);
    }

    if let Err(error) = cancellable(backend.select_sources(&session), &mut cancellation).await {
        let _ = backend.close(&session).await;
        return Err(error);
    }
    let streams = match cancellable(backend.start(&session, parent), &mut cancellation).await {
        Ok(streams) => streams,
        Err(error) => {
            let _ = backend.close(&session).await;
            return Err(error);
        }
    };
    let [stream] = streams.as_slice() else {
        let _ = backend.close(&session).await;
        return Err(PortalError::InvalidResponse);
    };
    let node_id = stream.pipe_wire_node_id();
    if node_id == 0 {
        let _ = backend.close(&session).await;
        return Err(PortalError::InvalidResponse);
    }
    let remote_fd = match cancellable(backend.open_remote(&session), &mut cancellation).await {
        Ok(remote_fd) => remote_fd,
        Err(error) => {
            let _ = backend.close(&session).await;
            return Err(error);
        }
    };

    Ok(ScreenCastLease {
        backend,
        session: Some(session),
        remote_fd: Some(remote_fd),
        node_id,
    })
}

async fn cancellable<T>(
    future: PortalFuture<'_, Result<T, PortalError>>,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<T, PortalError> {
    if cancellation_requested(cancellation) {
        return Err(PortalError::Cancelled);
    }
    let cancellation_future = wait_for_cancellation(cancellation);
    futures_util::pin_mut!(cancellation_future);
    match select(future, cancellation_future).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(PortalError::Cancelled),
    }
}

async fn wait_for_cancellation(cancellation: &mut watch::Receiver<bool>) {
    loop {
        if cancellation_requested(cancellation) {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

fn cancellation_requested(cancellation: &watch::Receiver<bool>) -> bool {
    *cancellation.borrow()
}

#[cfg(feature = "screen-share")]
mod ashpd_backend {
    use futures_util::StreamExt;

    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
    use ashpd::desktop::{PersistMode, ResponseError, Session};
    use ashpd::WindowIdentifier;

    use super::{OwnedFd, PortalError, PortalFuture, PortalStream, ScreenCastBackend};

    pub struct AshpdScreenCastBackend {
        proxy: Screencast<'static>,
    }

    impl AshpdScreenCastBackend {
        pub async fn new() -> Result<Self, PortalError> {
            let proxy = Screencast::new().await.map_err(map_ashpd_error)?;
            Ok(Self { proxy })
        }
    }

    impl ScreenCastBackend for AshpdScreenCastBackend {
        type Session = Session<'static, Screencast<'static>>;
        type Parent = WindowIdentifier;

        fn create_session(&self) -> PortalFuture<'_, Result<Self::Session, PortalError>> {
            Box::pin(async { self.proxy.create_session().await.map_err(map_ashpd_error) })
        }

        fn select_sources<'a>(
            &'a self,
            session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            Box::pin(async move {
                self.proxy
                    .select_sources(
                        session,
                        CursorMode::Embedded,
                        SourceType::Monitor | SourceType::Window,
                        false,
                        None,
                        PersistMode::DoNot,
                    )
                    .await
                    .map_err(map_ashpd_error)?
                    .response()
                    .map_err(map_ashpd_error)?;
                Ok(())
            })
        }

        fn start<'a>(
            &'a self,
            session: &'a Self::Session,
            parent: Option<&'a Self::Parent>,
        ) -> PortalFuture<'a, Result<Vec<PortalStream>, PortalError>> {
            Box::pin(async move {
                let response = self
                    .proxy
                    .start(session, parent)
                    .await
                    .map_err(map_ashpd_error)?
                    .response()
                    .map_err(map_ashpd_error)?;
                Ok(response
                    .streams()
                    .iter()
                    .map(|stream| PortalStream::new(stream.pipe_wire_node_id()))
                    .collect())
            })
        }

        fn open_remote<'a>(
            &'a self,
            session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<OwnedFd, PortalError>> {
            Box::pin(async move {
                self.proxy
                    .open_pipe_wire_remote(session)
                    .await
                    .map_err(map_ashpd_error)
            })
        }

        fn close<'a>(
            &'a self,
            session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            Box::pin(async move { session.close().await.map_err(map_ashpd_error) })
        }

        fn wait_closed<'a>(
            &'a self,
            session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            Box::pin(async move {
                let mut closed = session.receive_closed().await.map_err(map_ashpd_error)?;
                closed.next().await.ok_or(PortalError::OperationFailed)
            })
        }
    }

    fn map_ashpd_error(error: ashpd::Error) -> PortalError {
        match error {
            ashpd::Error::Response(ResponseError::Cancelled)
            | ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => PortalError::Cancelled,
            ashpd::Error::Portal(ashpd::PortalError::NotAllowed(_)) => {
                PortalError::PermissionDenied
            }
            ashpd::Error::PortalNotFound(_) => PortalError::Unavailable,
            _ => PortalError::OperationFailed,
        }
    }
}

#[cfg(feature = "screen-share")]
#[allow(unused_imports)]
pub use ashpd_backend::AshpdScreenCastBackend;

#[cfg(test)]
mod tests {
    use std::future;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Create,
        Select,
        Start,
        Open,
        Close,
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Arc<Mutex<Vec<Call>>>,
        block_start: bool,
        streams: Vec<PortalStream>,
    }

    impl FakeBackend {
        fn ready() -> Self {
            Self {
                streams: vec![PortalStream::new(42)],
                ..Default::default()
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }
    }

    impl ScreenCastBackend for FakeBackend {
        type Session = u64;
        type Parent = ();

        fn create_session(&self) -> PortalFuture<'_, Result<Self::Session, PortalError>> {
            self.record(Call::Create);
            Box::pin(async { Ok(1) })
        }

        fn select_sources<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            self.record(Call::Select);
            Box::pin(async { Ok(()) })
        }

        fn start<'a>(
            &'a self,
            _session: &'a Self::Session,
            _parent: Option<&'a Self::Parent>,
        ) -> PortalFuture<'a, Result<Vec<PortalStream>, PortalError>> {
            self.record(Call::Start);
            let streams = self.streams.clone();
            if self.block_start {
                Box::pin(future::pending())
            } else {
                Box::pin(async move { Ok(streams) })
            }
        }

        fn open_remote<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<OwnedFd, PortalError>> {
            self.record(Call::Open);
            Box::pin(async {
                let (remote, _peer) = UnixStream::pair().unwrap();
                Ok(remote.into())
            })
        }

        fn close<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            self.record(Call::Close);
            Box::pin(async { Ok(()) })
        }

        fn wait_closed<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            Box::pin(future::pending())
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn portal_is_idle_until_an_explicit_request_and_then_orders_every_step() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend::ready());
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            let lease = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap();
            assert_eq!(lease.node_id(), 42);
            assert!(lease.duplicate_remote_fd().is_ok());
            assert_eq!(
                backend.calls(),
                vec![Call::Create, Call::Select, Call::Start, Call::Open]
            );

            lease.close().await.unwrap();
            assert_eq!(backend.calls().last(), Some(&Call::Close));
        });
    }

    #[test]
    fn cancellation_while_the_chooser_is_pending_closes_without_opening_pipewire() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                block_start: true,
                ..FakeBackend::ready()
            });
            let (cancel, receiver) = tokio::sync::watch::channel(false);
            let request_backend = Arc::clone(&backend);
            let request =
                tokio::spawn(
                    async move { request_screen_cast(request_backend, None, receiver).await },
                );
            while !backend.calls().contains(&Call::Start) {
                tokio::task::yield_now().await;
            }
            cancel.send(true).unwrap();
            assert_eq!(request.await.unwrap().unwrap_err(), PortalError::Cancelled);
            assert_eq!(
                backend.calls(),
                vec![Call::Create, Call::Select, Call::Start, Call::Close]
            );
        });
    }

    #[test]
    fn an_invalid_stream_response_always_closes_the_session() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                streams: Vec::new(),
                ..Default::default()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            assert_eq!(
                request_screen_cast(Arc::clone(&backend), None, receiver)
                    .await
                    .unwrap_err(),
                PortalError::InvalidResponse
            );
            assert_eq!(backend.calls().last(), Some(&Call::Close));
        });
    }

    #[test]
    fn a_zero_pipewire_node_is_rejected_before_opening_the_remote() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                streams: vec![PortalStream::new(0)],
                ..Default::default()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            assert_eq!(
                request_screen_cast(Arc::clone(&backend), None, receiver)
                    .await
                    .unwrap_err(),
                PortalError::InvalidResponse
            );
            assert!(!backend.calls().contains(&Call::Open));
            assert_eq!(backend.calls().last(), Some(&Call::Close));
        });
    }
}
