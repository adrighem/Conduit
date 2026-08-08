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
    /// Close a session and release all portal-side resources.
    ///
    /// Implementations must tolerate retries after errors and partial cleanup.
    /// Repeated calls must be safe and idempotent. A session already known to
    /// be absent must be treated as successfully closed.
    fn close<'a>(&'a self, session: &'a Self::Session)
        -> PortalFuture<'a, Result<(), PortalError>>;
}

#[must_use = "portal sessions require explicit close"]
pub struct ScreenCastLease<B: ScreenCastBackend> {
    // Rust drops fields in declaration order. Keep the PipeWire descriptor
    // ahead of the portal session so ordinary drops release it first.
    remote: Option<(OwnedFd, u32)>,
    session: Option<B::Session>,
    backend: Arc<B>,
}

impl<B: ScreenCastBackend> ScreenCastLease<B> {
    fn pending(backend: Arc<B>, session: B::Session) -> Self {
        Self {
            remote: None,
            session: Some(session),
            backend,
        }
    }

    /// Transfer the PipeWire descriptor and node exactly once.
    ///
    /// The caller becomes the descriptor owner. It must release that ownership
    /// before calling [`Self::close`], because the lease cannot track a
    /// transferred descriptor.
    pub fn take_remote(&mut self) -> Result<(OwnedFd, u32), PortalError> {
        self.remote.take().ok_or(PortalError::OperationFailed)
    }

    pub async fn close(&mut self) -> Result<(), PortalError> {
        // PipeWire ownership must end before portal-side close can block.
        drop(self.remote.take());
        let Some(session) = self.session.as_ref() else {
            return Ok(());
        };
        self.backend.close(session).await?;
        drop(self.session.take());
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        self.session.is_none()
    }
}

impl<B: ScreenCastBackend> fmt::Debug for ScreenCastLease<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScreenCastLease")
            .finish_non_exhaustive()
    }
}

#[must_use = "request failures may retain a portal session requiring explicit cleanup"]
pub struct ScreenCastRequestError<B: ScreenCastBackend> {
    primary: PortalError,
    cleanup: Option<PortalError>,
    pending_lease: Option<ScreenCastLease<B>>,
}

impl<B: ScreenCastBackend> ScreenCastRequestError<B> {
    fn new(
        primary: PortalError,
        cleanup: Option<PortalError>,
        pending_lease: Option<ScreenCastLease<B>>,
    ) -> Self {
        Self {
            primary,
            cleanup,
            pending_lease,
        }
    }

    fn primary(primary: PortalError) -> Self {
        Self::new(primary, None, None)
    }

    pub fn primary_error(&self) -> PortalError {
        self.primary
    }

    pub fn operation_error(&self) -> PortalError {
        self.primary
    }

    pub fn cleanup_error(&self) -> Option<PortalError> {
        self.cleanup
    }

    pub fn pending_lease(&self) -> Option<&ScreenCastLease<B>> {
        self.pending_lease.as_ref()
    }

    pub fn has_pending_lease(&self) -> bool {
        self.pending_lease.is_some()
    }

    pub fn has_pending_cleanup(&self) -> bool {
        self.pending_lease.is_some()
    }

    pub fn into_parts(self) -> (PortalError, Option<PortalError>, Option<ScreenCastLease<B>>) {
        (self.primary, self.cleanup, self.pending_lease)
    }
}

impl<B: ScreenCastBackend> fmt::Debug for ScreenCastRequestError<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScreenCastRequestError")
            .field("primary", &self.primary)
            .field("cleanup", &self.cleanup)
            .field("pending_lease", &self.pending_lease.is_some())
            .finish()
    }
}

impl<B: ScreenCastBackend> fmt::Display for ScreenCastRequestError<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.primary.fmt(formatter)
    }
}

impl<B: ScreenCastBackend> std::error::Error for ScreenCastRequestError<B> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
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
}

pub async fn request_screen_cast<B: ScreenCastBackend>(
    backend: Arc<B>,
    parent: Option<&B::Parent>,
    cancellation: watch::Receiver<bool>,
) -> Result<ScreenCastLease<B>, ScreenCastRequestError<B>> {
    // Cancellation is cooperative. Once called, owners must drive this future
    // to completion so an acquired portal session can be explicitly closed.
    if cancellation_requested(&cancellation) {
        return Err(ScreenCastRequestError::primary(PortalError::Cancelled));
    }

    // Never race or abort session creation: its future may have created a
    // portal session before returning the handle needed to close it.
    let session = backend
        .create_session()
        .await
        .map_err(ScreenCastRequestError::primary)?;
    let mut lease = ScreenCastLease::pending(backend, session);

    if cancellation_requested(&cancellation) {
        return Err(fail_request(PortalError::Cancelled, lease).await);
    }

    let selection = {
        let session = lease.session.as_ref().expect("pending lease has a session");
        lease.backend.select_sources(session)
    };
    if let Err(error) = cancellable(selection, &cancellation).await {
        return Err(fail_request(error, lease).await);
    }

    let start = {
        let session = lease.session.as_ref().expect("pending lease has a session");
        lease.backend.start(session, parent)
    };
    let streams = match cancellable(start, &cancellation).await {
        Ok(streams) => streams,
        Err(error) => return Err(fail_request(error, lease).await),
    };
    let [stream] = streams.as_slice() else {
        return Err(fail_request(PortalError::InvalidResponse, lease).await);
    };
    let node_id = stream.pipe_wire_node_id();
    if node_id == 0 {
        return Err(fail_request(PortalError::InvalidResponse, lease).await);
    }

    let open = {
        let session = lease.session.as_ref().expect("pending lease has a session");
        lease.backend.open_remote(session)
    };
    let remote_fd = match cancellable(open, &cancellation).await {
        Ok(remote_fd) => remote_fd,
        Err(error) => return Err(fail_request(error, lease).await),
    };
    lease.remote = Some((remote_fd, node_id));

    // Catch cancellation made ready in the same poll as open_remote.
    if cancellation_requested(&cancellation) {
        return Err(fail_request(PortalError::Cancelled, lease).await);
    }

    Ok(lease)
}

async fn fail_request<B: ScreenCastBackend>(
    primary: PortalError,
    mut lease: ScreenCastLease<B>,
) -> ScreenCastRequestError<B> {
    match lease.close().await {
        Ok(()) => ScreenCastRequestError::new(primary, None, None),
        Err(cleanup) => ScreenCastRequestError::new(primary, Some(cleanup), Some(lease)),
    }
}

async fn cancellable<T>(
    future: PortalFuture<'_, Result<T, PortalError>>,
    cancellation: &watch::Receiver<bool>,
) -> Result<T, PortalError> {
    if cancellation_requested(cancellation) {
        return Err(PortalError::Cancelled);
    }
    let mut cancellation_waiter = cancellation.clone();
    let cancellation_future = wait_for_cancellation(&mut cancellation_waiter);
    futures_util::pin_mut!(cancellation_future);
    match select(future, cancellation_future).await {
        Either::Left((result, _)) => match result {
            Ok(_) if cancellation_requested(cancellation) => Err(PortalError::Cancelled),
            result => result,
        },
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
    *cancellation.borrow() || cancellation.has_changed().is_err()
}

#[cfg(feature = "screen-share")]
mod ashpd_backend {
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
            Box::pin(async move {
                match session.close().await {
                    Ok(()) => Ok(()),
                    Err(error) if session_is_already_closed(&error) => Ok(()),
                    Err(error) => Err(map_ashpd_error(error)),
                }
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

    pub(super) fn session_is_already_closed(error: &ashpd::Error) -> bool {
        match error {
            ashpd::Error::Portal(ashpd::PortalError::NotFound(_)) => true,
            ashpd::Error::Portal(ashpd::PortalError::ZBus(error)) | ashpd::Error::Zbus(error) => {
                zbus_error_is_unknown_object(error)
            }
            _ => false,
        }
    }

    fn zbus_error_is_unknown_object(error: &ashpd::zbus::Error) -> bool {
        match error {
            ashpd::zbus::Error::FDO(error) => {
                matches!(error.as_ref(), ashpd::zbus::fdo::Error::UnknownObject(_))
            }
            ashpd::zbus::Error::MethodError(name, _, _) => {
                name.as_str() == "org.freedesktop.DBus.Error.UnknownObject"
            }
            _ => false,
        }
    }
}

#[cfg(feature = "screen-share")]
#[allow(unused_imports)]
pub use ashpd_backend::AshpdScreenCastBackend;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;

    const SESSION_MARKER: u64 = 982_451_653;
    const BACKEND_MARKER: &str = "backend-sensitive-marker";

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Create,
        Select,
        Start,
        Open,
        Close,
    }

    #[derive(Debug)]
    struct FakeSession(u64);

    struct FakeBackend {
        calls: Arc<Mutex<Vec<Call>>>,
        block_create: bool,
        create_release: Arc<tokio::sync::Semaphore>,
        block_start: bool,
        streams: Vec<PortalStream>,
        select_error: Option<PortalError>,
        start_error: Option<PortalError>,
        open_error: Option<PortalError>,
        cancel_on_start: Option<watch::Sender<bool>>,
        cancel_on_open: Option<watch::Sender<bool>>,
        close_results: Arc<Mutex<VecDeque<Result<(), PortalError>>>>,
        remote_peer: Arc<Mutex<Option<UnixStream>>>,
        fd_closed_at_close: Arc<Mutex<Vec<bool>>>,
    }

    impl fmt::Debug for FakeBackend {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter
                .debug_tuple("FakeBackend")
                .field(&BACKEND_MARKER)
                .finish()
        }
    }

    impl FakeBackend {
        fn ready() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                block_create: false,
                create_release: Arc::new(tokio::sync::Semaphore::new(0)),
                block_start: false,
                streams: vec![PortalStream::new(42)],
                select_error: None,
                start_error: None,
                open_error: None,
                cancel_on_start: None,
                cancel_on_open: None,
                close_results: Arc::new(Mutex::new(VecDeque::new())),
                remote_peer: Arc::new(Mutex::new(None)),
                fd_closed_at_close: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: Call) {
            self.calls.lock().unwrap().push(call);
        }

        fn release_create(&self) {
            self.create_release.add_permits(1);
        }

        fn close_count(&self) -> usize {
            self.calls()
                .into_iter()
                .filter(|call| *call == Call::Close)
                .count()
        }

        fn remote_is_closed(&self) -> bool {
            let mut peer = self.remote_peer.lock().unwrap();
            peer.as_mut().is_none_or(peer_is_closed)
        }

        fn fd_closed_at_close(&self) -> Vec<bool> {
            self.fd_closed_at_close.lock().unwrap().clone()
        }
    }

    impl ScreenCastBackend for FakeBackend {
        type Session = FakeSession;
        type Parent = ();

        fn create_session(&self) -> PortalFuture<'_, Result<Self::Session, PortalError>> {
            self.record(Call::Create);
            let block = self.block_create;
            let release = Arc::clone(&self.create_release);
            Box::pin(async move {
                if block {
                    let permit = release
                        .acquire()
                        .await
                        .map_err(|_| PortalError::OperationFailed)?;
                    permit.forget();
                }
                Ok(FakeSession(SESSION_MARKER))
            })
        }

        fn select_sources<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            self.record(Call::Select);
            let error = self.select_error;
            Box::pin(async move { error.map_or(Ok(()), Err) })
        }

        fn start<'a>(
            &'a self,
            _session: &'a Self::Session,
            _parent: Option<&'a Self::Parent>,
        ) -> PortalFuture<'a, Result<Vec<PortalStream>, PortalError>> {
            self.record(Call::Start);
            let block = self.block_start;
            let streams = self.streams.clone();
            let error = self.start_error;
            let cancel = self.cancel_on_start.clone();
            Box::pin(async move {
                if let Some(cancel) = cancel {
                    let _ = cancel.send(true);
                }
                if block {
                    return future::pending().await;
                }
                error.map_or(Ok(streams), Err)
            })
        }

        fn open_remote<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<OwnedFd, PortalError>> {
            self.record(Call::Open);
            let error = self.open_error;
            let cancel = self.cancel_on_open.clone();
            let remote_peer = Arc::clone(&self.remote_peer);
            Box::pin(async move {
                if let Some(error) = error {
                    return Err(error);
                }
                let (remote, _peer) = UnixStream::pair().unwrap();
                *remote_peer.lock().unwrap() = Some(_peer);
                if let Some(cancel) = cancel {
                    let _ = cancel.send(true);
                }
                Ok(remote.into())
            })
        }

        fn close<'a>(
            &'a self,
            _session: &'a Self::Session,
        ) -> PortalFuture<'a, Result<(), PortalError>> {
            self.record(Call::Close);
            let fd_closed = self.remote_is_closed();
            self.fd_closed_at_close.lock().unwrap().push(fd_closed);
            let result = self
                .close_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(()));
            Box::pin(async move { result })
        }
    }

    fn peer_is_closed(peer: &mut UnixStream) -> bool {
        peer.set_nonblocking(true).unwrap();
        let mut byte = [0_u8; 1];
        match peer.read(&mut byte) {
            Ok(0) => true,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
            Err(_) | Ok(_) => true,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn wait_for_call(backend: &FakeBackend, expected: Call) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !backend.calls().contains(&expected) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn portal_is_idle_until_an_explicit_request_and_then_orders_every_step() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend::ready());
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            let mut lease = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap();
            let (remote, node_id) = lease.take_remote().unwrap();
            assert_eq!(node_id, 42);
            assert_eq!(
                lease.take_remote().unwrap_err(),
                PortalError::OperationFailed
            );
            assert_eq!(
                backend.calls(),
                vec![Call::Create, Call::Select, Call::Start, Call::Open]
            );

            drop(remote);
            lease.close().await.unwrap();
            assert_eq!(backend.calls().last(), Some(&Call::Close));
        });
    }

    #[test]
    fn pre_cancelled_request_performs_no_backend_calls() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend::ready());
            let (cancel, receiver) = tokio::sync::watch::channel(false);
            cancel.send(true).unwrap();

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();

            assert_eq!(error.primary_error(), PortalError::Cancelled);
            assert_eq!(error.cleanup_error(), None);
            assert!(!error.has_pending_lease());
            assert!(backend.calls().is_empty());
        });
    }

    #[test]
    fn closed_cancellation_channel_performs_no_backend_calls() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend::ready());
            let (cancel, receiver) = tokio::sync::watch::channel(false);
            drop(cancel);

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();

            assert_eq!(error.primary_error(), PortalError::Cancelled);
            assert!(backend.calls().is_empty());
        });
    }

    #[test]
    fn cancellation_during_create_waits_for_the_handle_then_closes_once() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                block_create: true,
                ..FakeBackend::ready()
            });
            let (cancel, receiver) = tokio::sync::watch::channel(false);
            let request_backend = Arc::clone(&backend);
            let request =
                tokio::spawn(
                    async move { request_screen_cast(request_backend, None, receiver).await },
                );

            wait_for_call(&backend, Call::Create).await;
            cancel.send(true).unwrap();
            tokio::task::yield_now().await;
            assert!(!request.is_finished());
            assert_eq!(backend.calls(), vec![Call::Create]);

            backend.release_create();
            let error = request.await.unwrap().unwrap_err();
            assert_eq!(error.primary_error(), PortalError::Cancelled);
            assert!(!error.has_pending_lease());
            assert_eq!(backend.calls(), vec![Call::Create, Call::Close]);
            assert_eq!(backend.close_count(), 1);
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
            wait_for_call(&backend, Call::Start).await;
            cancel.send(true).unwrap();
            let error = request.await.unwrap().unwrap_err();
            assert_eq!(error.primary_error(), PortalError::Cancelled);
            assert!(!error.has_pending_lease());
            assert_eq!(
                backend.calls(),
                vec![Call::Create, Call::Select, Call::Start, Call::Close]
            );
        });
    }

    #[test]
    fn cancellation_ready_with_chooser_success_is_checked_before_open() {
        runtime().block_on(async {
            let (cancel, receiver) = tokio::sync::watch::channel(false);
            let backend = Arc::new(FakeBackend {
                cancel_on_start: Some(cancel.clone()),
                ..FakeBackend::ready()
            });

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();

            assert_eq!(error.primary_error(), PortalError::Cancelled);
            assert_eq!(
                backend.calls(),
                vec![Call::Create, Call::Select, Call::Start, Call::Close]
            );
        });
    }

    #[test]
    fn cancellation_ready_with_open_success_drops_fd_then_closes() {
        runtime().block_on(async {
            let (cancel, receiver) = tokio::sync::watch::channel(false);
            let backend = Arc::new(FakeBackend {
                cancel_on_open: Some(cancel.clone()),
                ..FakeBackend::ready()
            });

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();

            assert_eq!(error.primary_error(), PortalError::Cancelled);
            assert!(!error.has_pending_lease());
            assert_eq!(
                backend.calls(),
                vec![
                    Call::Create,
                    Call::Select,
                    Call::Start,
                    Call::Open,
                    Call::Close,
                ]
            );
            assert_eq!(backend.fd_closed_at_close(), vec![true]);
        });
    }

    #[test]
    fn backend_failure_remains_primary_after_successful_cleanup() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                select_error: Some(PortalError::PermissionDenied),
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();

            assert_eq!(error.primary_error(), PortalError::PermissionDenied);
            assert_eq!(error.cleanup_error(), None);
            assert!(!error.has_pending_lease());
            assert_eq!(
                backend.calls(),
                vec![Call::Create, Call::Select, Call::Close]
            );
        });
    }

    #[test]
    fn close_failure_preserves_primary_and_pending_lease_for_retry() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                streams: Vec::new(),
                close_results: Arc::new(Mutex::new(VecDeque::from([
                    Err(PortalError::Unavailable),
                    Ok(()),
                ]))),
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();
            assert_eq!(error.primary_error(), PortalError::InvalidResponse);
            assert_eq!(error.cleanup_error(), Some(PortalError::Unavailable));
            assert!(error.has_pending_lease());

            let (primary, cleanup, pending) = error.into_parts();
            assert_eq!(primary, PortalError::InvalidResponse);
            assert_eq!(cleanup, Some(PortalError::Unavailable));
            let mut lease = pending.unwrap();
            assert!(!lease.is_closed());

            lease.close().await.unwrap();
            assert!(lease.is_closed());
            lease.close().await.unwrap();
            assert_eq!(backend.close_count(), 2);
        });
    }

    #[test]
    fn explicit_close_drops_fd_before_backend_close_and_retries_session() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                close_results: Arc::new(Mutex::new(VecDeque::from([
                    Err(PortalError::Unavailable),
                    Ok(()),
                ]))),
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);
            let mut lease = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap();

            assert!(!backend.remote_is_closed());
            assert_eq!(lease.close().await, Err(PortalError::Unavailable));

            assert!(!lease.is_closed());
            assert_eq!(backend.fd_closed_at_close(), vec![true]);
            assert!(backend.remote_is_closed());

            lease.close().await.unwrap();
            lease.close().await.unwrap();

            assert_eq!(backend.fd_closed_at_close(), vec![true, true]);
            assert_eq!(backend.close_count(), 2);
            assert!(lease.is_closed());
        });
    }

    #[test]
    fn lease_and_request_error_debug_output_is_redacted() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                streams: vec![PortalStream::new(424_242)],
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);
            let mut lease = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap();
            let fd = lease.remote.as_ref().unwrap().0.as_raw_fd();
            let debug = format!("{lease:?}");
            assert!(!debug.contains(BACKEND_MARKER));
            assert!(!debug.contains(&SESSION_MARKER.to_string()));
            assert!(!debug.contains("424242"));
            assert!(!debug.contains(&fd.to_string()));
            lease.close().await.unwrap();

            let backend = Arc::new(FakeBackend {
                select_error: Some(PortalError::PermissionDenied),
                close_results: Arc::new(Mutex::new(VecDeque::from([
                    Err(PortalError::OperationFailed),
                    Ok(()),
                ]))),
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);
            let error = request_screen_cast(backend, None, receiver)
                .await
                .unwrap_err();
            let debug = format!("{error:?}");
            assert!(!debug.contains(BACKEND_MARKER));
            assert!(!debug.contains(&SESSION_MARKER.to_string()));
            assert_eq!(error.to_string(), PortalError::PermissionDenied.to_string());
            assert_eq!(
                std::error::Error::source(&error).map(ToString::to_string),
                Some(PortalError::PermissionDenied.to_string())
            );
            let (_, _, pending) = error.into_parts();
            pending.unwrap().close().await.unwrap();
        });
    }

    #[cfg(feature = "screen-share")]
    #[test]
    fn ashpd_missing_session_errors_make_close_idempotent() {
        let portal_not_found =
            ashpd::Error::Portal(ashpd::PortalError::NotFound("missing session".into()));
        assert!(ashpd_backend::session_is_already_closed(&portal_not_found));

        let unknown_object =
            ashpd::Error::Portal(ashpd::PortalError::ZBus(ashpd::zbus::Error::FDO(Box::new(
                ashpd::zbus::fdo::Error::UnknownObject("missing session".into()),
            ))));
        assert!(ashpd_backend::session_is_already_closed(&unknown_object));

        let unrelated = ashpd::Error::Portal(ashpd::PortalError::Failed("close failed".into()));
        assert!(!ashpd_backend::session_is_already_closed(&unrelated));
    }

    #[test]
    fn an_invalid_stream_response_always_closes_the_session() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                streams: Vec::new(),
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();
            assert_eq!(error.primary_error(), PortalError::InvalidResponse);
            assert!(!error.has_pending_lease());
            assert_eq!(backend.calls().last(), Some(&Call::Close));
        });
    }

    #[test]
    fn a_zero_pipewire_node_is_rejected_before_opening_the_remote() {
        runtime().block_on(async {
            let backend = Arc::new(FakeBackend {
                streams: vec![PortalStream::new(0)],
                ..FakeBackend::ready()
            });
            let (_cancel, receiver) = tokio::sync::watch::channel(false);

            let error = request_screen_cast(Arc::clone(&backend), None, receiver)
                .await
                .unwrap_err();
            assert_eq!(error.primary_error(), PortalError::InvalidResponse);
            assert!(!error.has_pending_lease());
            assert!(!backend.calls().contains(&Call::Open));
            assert_eq!(backend.calls().last(), Some(&Call::Close));
        });
    }
}
