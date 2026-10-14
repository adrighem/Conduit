use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use super::{
    OperationContext, RequestId, RuntimeAdmissionKey, RuntimeAdmissionKind, RuntimeIdentity,
    RuntimeRequest, SessionId, BACKGROUND_TASK_CONCURRENCY, IMAGE_TASK_CONCURRENCY,
    INTERACTIVE_TASK_CONCURRENCY, NAVIGATION_TASK_CONCURRENCY, RUNTIME_BACKGROUND_QUEUE_CAPACITY,
    RUNTIME_CONTROL_QUEUE_CAPACITY, RUNTIME_IMAGE_QUEUE_CAPACITY,
    RUNTIME_INTERACTIVE_QUEUE_CAPACITY, RUNTIME_NAVIGATION_QUEUE_CAPACITY,
    RUNTIME_UPLOAD_QUEUE_CAPACITY, UPLOAD_TASK_CONCURRENCY,
};

pub(super) fn runtime_identity_order(identity: RuntimeIdentity) -> (SessionId, RequestId) {
    (identity.session, identity.request)
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) enum RuntimeInFlightKey {
    Coalescible {
        session: SessionId,
        key: RuntimeAdmissionKey,
    },
    Ordered {
        session: SessionId,
        context: OperationContext,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuntimeTaskLane {
    Navigation,
    Interactive,
    Background,
    Image,
    Upload,
}

impl RuntimeTaskLane {
    pub(super) const ALL: [Self; 5] = [
        Self::Navigation,
        Self::Interactive,
        Self::Upload,
        Self::Background,
        Self::Image,
    ];

    pub(super) const fn index(self) -> usize {
        match self {
            Self::Navigation => 0,
            Self::Interactive => 1,
            Self::Upload => 2,
            Self::Background => 3,
            Self::Image => 4,
        }
    }

    pub(super) const fn running_capacity(self) -> usize {
        match self {
            Self::Navigation => NAVIGATION_TASK_CONCURRENCY,
            Self::Interactive => INTERACTIVE_TASK_CONCURRENCY,
            Self::Background => BACKGROUND_TASK_CONCURRENCY,
            Self::Image => IMAGE_TASK_CONCURRENCY,
            Self::Upload => UPLOAD_TASK_CONCURRENCY,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct RuntimeTaskLimits {
    navigation: Arc<Semaphore>,
    interactive: Arc<Semaphore>,
    background: Arc<Semaphore>,
    image: Arc<Semaphore>,
    upload: Arc<Semaphore>,
    capacities: [u32; 5],
}

impl RuntimeTaskLimits {
    pub(super) fn new(
        navigation: usize,
        interactive: usize,
        background: usize,
        image: usize,
        upload: usize,
    ) -> Self {
        let capacities = [navigation, interactive, upload, background, image]
            .map(|capacity| u32::try_from(capacity).expect("runtime task capacity exceeds u32"));
        Self {
            navigation: Arc::new(Semaphore::new(navigation)),
            interactive: Arc::new(Semaphore::new(interactive)),
            background: Arc::new(Semaphore::new(background)),
            image: Arc::new(Semaphore::new(image)),
            upload: Arc::new(Semaphore::new(upload)),
            capacities,
        }
    }

    pub(super) fn semaphore(&self, lane: RuntimeTaskLane) -> Arc<Semaphore> {
        match lane {
            RuntimeTaskLane::Navigation => Arc::clone(&self.navigation),
            RuntimeTaskLane::Interactive => Arc::clone(&self.interactive),
            RuntimeTaskLane::Background => Arc::clone(&self.background),
            RuntimeTaskLane::Image => Arc::clone(&self.image),
            RuntimeTaskLane::Upload => Arc::clone(&self.upload),
        }
    }

    pub(super) async fn acquire(&self, lane: RuntimeTaskLane) -> OwnedSemaphorePermit {
        self.semaphore(lane)
            .acquire_owned()
            .await
            .expect("runtime task semaphore unexpectedly closed")
    }

    pub(super) async fn wait_idle(&self) {
        let mut permits = Vec::with_capacity(RuntimeTaskLane::ALL.len());
        for lane in RuntimeTaskLane::ALL {
            permits.push(
                self.semaphore(lane)
                    .acquire_many_owned(self.capacities[lane.index()])
                    .await
                    .expect("runtime task semaphore unexpectedly closed"),
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RuntimeCommandAdmissionConfig {
    pub(super) control_capacity: usize,
    pub(super) lane_capacities: [usize; 5],
}

impl RuntimeCommandAdmissionConfig {
    pub(super) fn production() -> Self {
        Self {
            control_capacity: RUNTIME_CONTROL_QUEUE_CAPACITY,
            lane_capacities: [
                RUNTIME_NAVIGATION_QUEUE_CAPACITY,
                RUNTIME_INTERACTIVE_QUEUE_CAPACITY,
                RUNTIME_UPLOAD_QUEUE_CAPACITY,
                RUNTIME_BACKGROUND_QUEUE_CAPACITY,
                RUNTIME_IMAGE_QUEUE_CAPACITY,
            ],
        }
    }

    #[cfg(test)]
    pub(super) fn uniform(control_capacity: usize, lane_capacity: usize) -> Self {
        Self {
            control_capacity,
            lane_capacities: [lane_capacity; 5],
        }
    }

    pub(super) fn lane_capacity(self, lane: RuntimeTaskLane) -> usize {
        self.lane_capacities[lane.index()]
    }

    pub(super) fn validate(self) {
        assert!(
            self.control_capacity > 0,
            "control capacity must be non-zero"
        );
        assert!(
            self.lane_capacities.iter().all(|capacity| *capacity > 0),
            "lane capacities must be non-zero"
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuntimeCommandAdmissionOutcome {
    Enqueued,
    Coalesced,
    Superseded,
    DroppedStale,
    StaleSession,
    DroppedAtCapacity,
    Closed,
}

pub(super) struct RuntimeCommandAdmissionError {
    pub(super) outcome: RuntimeCommandAdmissionOutcome,
    pub(super) request: RuntimeRequest,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct RuntimeCommandAdmissionSnapshot {
    pub(super) admitted: u64,
    pub(super) dequeued: u64,
    pub(super) coalesced: u64,
    pub(super) superseded: u64,
    pub(super) cancelled: u64,
    pub(super) rejected: u64,
    pub(super) depth: usize,
    pub(super) peak_depth: usize,
    pub(super) in_flight: usize,
    pub(super) peak_in_flight: usize,
}

#[derive(Default)]
struct RuntimeCommandAdmissionCounters {
    admitted: u64,
    dequeued: u64,
    coalesced: u64,
    superseded: u64,
    cancelled: u64,
    rejected: u64,
    peak_depth: usize,
    peak_in_flight: usize,
}

struct RuntimeCommandAdmissionState {
    senders_closed: bool,
    receiver_closed: bool,
    newest_session: SessionId,
    pending_session_starts: VecDeque<SessionId>,
    control: VecDeque<RuntimeRequest>,
    lanes: [VecDeque<RuntimeRequest>; 5],
    in_flight: HashSet<RuntimeInFlightKey>,
    counters: RuntimeCommandAdmissionCounters,
}

impl RuntimeCommandAdmissionState {
    fn new() -> Self {
        Self {
            senders_closed: false,
            receiver_closed: false,
            newest_session: SessionId::default(),
            pending_session_starts: VecDeque::new(),
            control: VecDeque::new(),
            lanes: std::array::from_fn(|_| VecDeque::new()),
            in_flight: HashSet::new(),
            counters: RuntimeCommandAdmissionCounters::default(),
        }
    }

    fn depth(&self) -> usize {
        self.control.len() + self.lanes.iter().map(VecDeque::len).sum::<usize>()
    }

    fn record_depth(&mut self) {
        self.counters.peak_depth = self.counters.peak_depth.max(self.depth());
    }

    fn has_ordered_work_before(&self, session: SessionId) -> bool {
        self.lanes.iter().flatten().any(|request| {
            request.identity.session < session
                && matches!(
                    request.descriptor.admission.kind,
                    RuntimeAdmissionKind::DurableAction
                )
        }) || self.in_flight.iter().any(|key| {
            matches!(
                key,
                RuntimeInFlightKey::Ordered {
                    session: ordered_session,
                    ..
                } if *ordered_session < session
            )
        })
    }

    fn has_pending_session_start_at_or_before(&self, session: SessionId) -> bool {
        self.pending_session_starts
            .front()
            .is_some_and(|pending| *pending <= session)
    }

    fn snapshot(&self) -> RuntimeCommandAdmissionSnapshot {
        RuntimeCommandAdmissionSnapshot {
            admitted: self.counters.admitted,
            dequeued: self.counters.dequeued,
            coalesced: self.counters.coalesced,
            superseded: self.counters.superseded,
            cancelled: self.counters.cancelled,
            rejected: self.counters.rejected,
            depth: self.depth(),
            peak_depth: self.counters.peak_depth,
            in_flight: self.in_flight.len(),
            peak_in_flight: self.counters.peak_in_flight,
        }
    }
}

pub(super) struct RuntimeCommandAdmission {
    config: RuntimeCommandAdmissionConfig,
    state: Mutex<RuntimeCommandAdmissionState>,
    control_ready: Notify,
    lane_ready: [Notify; 5],
}

impl RuntimeCommandAdmission {
    pub(super) fn channel(
        config: RuntimeCommandAdmissionConfig,
    ) -> (RuntimeCommandSender, RuntimeCommandReceiver) {
        config.validate();
        let admission = Arc::new(Self {
            config,
            state: Mutex::new(RuntimeCommandAdmissionState::new()),
            control_ready: Notify::new(),
            lane_ready: std::array::from_fn(|_| Notify::new()),
        });
        let sender_lifetime = Arc::new(RuntimeCommandSenderLifetime {
            admission: Arc::clone(&admission),
        });
        let receiver_lifetime = Arc::new(RuntimeCommandReceiverLifetime {
            admission: Arc::clone(&admission),
        });
        (
            RuntimeCommandSender {
                admission: Arc::clone(&admission),
                lifetime: sender_lifetime,
            },
            RuntimeCommandReceiver {
                admission,
                lifetime: receiver_lifetime,
            },
        )
    }

    pub(super) fn try_send(
        self: &Arc<Self>,
        mut request: RuntimeRequest,
    ) -> std::result::Result<RuntimeCommandAdmissionOutcome, Box<RuntimeCommandAdmissionError>>
    {
        let lane = request.descriptor.lane;
        let policy = request.descriptor.admission;
        let request_session = request.identity.session;
        let mut state = self
            .state
            .lock()
            .expect("runtime command admission lock poisoned");

        if state.receiver_closed {
            state.counters.rejected = state.counters.rejected.saturating_add(1);
            return Err(Box::new(RuntimeCommandAdmissionError {
                outcome: RuntimeCommandAdmissionOutcome::Closed,
                request,
            }));
        }

        if request.identity.session < state.newest_session {
            state.counters.rejected = state.counters.rejected.saturating_add(1);
            return Err(Box::new(RuntimeCommandAdmissionError {
                outcome: RuntimeCommandAdmissionOutcome::StaleSession,
                request,
            }));
        }

        let command_starts_session = request.command.starts_session();
        let starts_session = command_starts_session
            && !state
                .pending_session_starts
                .contains(&request.identity.session);
        let use_control_queue =
            policy.kind == RuntimeAdmissionKind::Control || command_starts_session;
        request.starts_session = starts_session;

        if !use_control_queue {
            if let Some(replacement_key) = policy.replacement_key {
                let queue = &mut state.lanes[lane.index()];
                if let Some(position) = queue.iter().position(|queued| {
                    queued.identity.session == request.identity.session
                        && queued.descriptor.admission.replacement_key == Some(replacement_key)
                }) {
                    let queued = queue
                        .get_mut(position)
                        .expect("replacement queue position exists");
                    if runtime_identity_order(request.identity)
                        <= runtime_identity_order(queued.identity)
                    {
                        state.counters.rejected = state.counters.rejected.saturating_add(1);
                        return Err(Box::new(RuntimeCommandAdmissionError {
                            outcome: RuntimeCommandAdmissionOutcome::DroppedStale,
                            request,
                        }));
                    }
                    *queued = request;
                    state.newest_session = state.newest_session.max(request_session);
                    state.counters.admitted = state.counters.admitted.saturating_add(1);
                    return Ok(match policy.kind {
                        RuntimeAdmissionKind::Coalescible => {
                            state.counters.coalesced = state.counters.coalesced.saturating_add(1);
                            RuntimeCommandAdmissionOutcome::Coalesced
                        }
                        RuntimeAdmissionKind::Supersedable => {
                            state.counters.superseded = state.counters.superseded.saturating_add(1);
                            RuntimeCommandAdmissionOutcome::Superseded
                        }
                        RuntimeAdmissionKind::Control | RuntimeAdmissionKind::DurableAction => {
                            unreachable!("only replaceable commands have replacement keys")
                        }
                    });
                }
            }
        }

        let full = if use_control_queue {
            state.control.len() >= self.config.control_capacity
        } else {
            state.lanes[lane.index()].len() >= self.config.lane_capacity(lane)
        };
        if full {
            state.counters.rejected = state.counters.rejected.saturating_add(1);
            return Err(Box::new(RuntimeCommandAdmissionError {
                outcome: RuntimeCommandAdmissionOutcome::DroppedAtCapacity,
                request,
            }));
        }

        if use_control_queue {
            state.control.push_back(request);
        } else {
            state.lanes[lane.index()].push_back(request);
        }
        if starts_session {
            state.pending_session_starts.push_back(request_session);
        }
        state.newest_session = state.newest_session.max(request_session);
        state.counters.admitted = state.counters.admitted.saturating_add(1);
        state.record_depth();
        drop(state);

        if use_control_queue {
            self.control_ready.notify_one();
        } else {
            self.lane_ready[lane.index()].notify_one();
        }
        Ok(RuntimeCommandAdmissionOutcome::Enqueued)
    }

    pub(super) fn pop_control(self: &Arc<Self>) -> (Option<RuntimeControlRequest>, bool) {
        let mut state = self
            .state
            .lock()
            .expect("runtime command admission lock poisoned");
        let request = if state
            .control
            .front()
            .is_some_and(|request| !state.has_ordered_work_before(request.identity.session))
        {
            state.control.pop_front()
        } else {
            None
        };
        if request.is_some() {
            state.counters.dequeued = state.counters.dequeued.saturating_add(1);
        }
        let finished = state.control.is_empty() && state.senders_closed;
        (
            request.map(|request| {
                let session_start =
                    request
                        .starts_session
                        .then(|| RuntimeSessionStartReservation {
                            admission: Arc::clone(self),
                            session: request.identity.session,
                        });
                RuntimeControlRequest {
                    request,
                    session_start,
                }
            }),
            finished,
        )
    }

    pub(super) fn pop_lane(
        self: &Arc<Self>,
        lane: RuntimeTaskLane,
    ) -> (Option<RuntimeLaneRequest>, bool) {
        let mut state = self
            .state
            .lock()
            .expect("runtime command admission lock poisoned");
        let lane_index = lane.index();
        let supersedable = state.lanes[lane_index].iter().position(|request| {
            request.descriptor.admission.kind == RuntimeAdmissionKind::Supersedable
                && !state.has_pending_session_start_at_or_before(request.identity.session)
                && !state.has_ordered_work_before(request.identity.session)
        });
        let position = supersedable.or_else(|| {
            state.lanes[lane_index].iter().position(|request| {
                !state.has_pending_session_start_at_or_before(request.identity.session)
                    && !state.has_ordered_work_before(request.identity.session)
                    && request
                        .in_flight_key
                        .as_ref()
                        .is_none_or(|key| !state.in_flight.contains(key))
            })
        });
        let request = position.and_then(|position| state.lanes[lane_index].remove(position));
        let reservation = request
            .as_ref()
            .and_then(|request| request.in_flight_key.clone())
            .map(|key| {
                assert!(state.in_flight.insert(key.clone()));
                state.counters.peak_in_flight =
                    state.counters.peak_in_flight.max(state.in_flight.len());
                RuntimeCommandReservation {
                    admission: Arc::clone(self),
                    lane,
                    key,
                }
            });
        if request.is_some() {
            state.counters.dequeued = state.counters.dequeued.saturating_add(1);
        }
        let finished = state.lanes[lane_index].is_empty() && state.senders_closed;
        (
            request.map(|request| RuntimeLaneRequest {
                request,
                reservation,
            }),
            finished,
        )
    }

    fn release_in_flight(&self, _lane: RuntimeTaskLane, key: &RuntimeInFlightKey) {
        let removed = self
            .state
            .lock()
            .expect("runtime command admission lock poisoned")
            .in_flight
            .remove(key);
        if removed {
            self.control_ready.notify_waiters();
            for ready in &self.lane_ready {
                ready.notify_waiters();
            }
        }
    }

    fn complete_session_start(&self, session: SessionId) {
        let mut state = self
            .state
            .lock()
            .expect("runtime command admission lock poisoned");
        let pending = state.pending_session_starts.front().copied();
        debug_assert_eq!(pending, Some(session));
        let removed = if pending == Some(session) {
            state.pending_session_starts.pop_front();
            true
        } else if let Some(position) = state
            .pending_session_starts
            .iter()
            .position(|pending| *pending == session)
        {
            state.pending_session_starts.remove(position);
            true
        } else {
            false
        };
        drop(state);
        if removed {
            self.control_ready.notify_waiters();
            for ready in &self.lane_ready {
                ready.notify_waiters();
            }
        }
    }

    pub(super) fn snapshot(&self) -> RuntimeCommandAdmissionSnapshot {
        self.state
            .lock()
            .expect("runtime command admission lock poisoned")
            .snapshot()
    }

    fn close_senders(&self) {
        self.state
            .lock()
            .expect("runtime command admission lock poisoned")
            .senders_closed = true;
        self.control_ready.notify_waiters();
        for ready in &self.lane_ready {
            ready.notify_waiters();
        }
    }

    fn close_receiver(&self) {
        let mut state = self
            .state
            .lock()
            .expect("runtime command admission lock poisoned");
        if state.receiver_closed {
            return;
        }
        state.receiver_closed = true;
        let cancelled = state.depth() as u64;
        state.control.clear();
        for queue in &mut state.lanes {
            queue.clear();
        }
        state.counters.cancelled = state.counters.cancelled.saturating_add(cancelled);
        drop(state);
        self.control_ready.notify_waiters();
        for ready in &self.lane_ready {
            ready.notify_waiters();
        }
    }
}

pub(super) struct RuntimeSessionStartReservation {
    admission: Arc<RuntimeCommandAdmission>,
    session: SessionId,
}

impl Drop for RuntimeSessionStartReservation {
    fn drop(&mut self) {
        self.admission.complete_session_start(self.session);
    }
}

pub(super) struct RuntimeControlRequest {
    pub(super) request: RuntimeRequest,
    pub(super) session_start: Option<RuntimeSessionStartReservation>,
}

pub(super) struct RuntimeCommandReservation {
    admission: Arc<RuntimeCommandAdmission>,
    lane: RuntimeTaskLane,
    key: RuntimeInFlightKey,
}

impl Drop for RuntimeCommandReservation {
    fn drop(&mut self) {
        self.admission.release_in_flight(self.lane, &self.key);
    }
}

pub(super) struct RuntimeLaneRequest {
    pub(super) request: RuntimeRequest,
    pub(super) reservation: Option<RuntimeCommandReservation>,
}

pub(super) struct RuntimeCommandLease {
    pub(super) _reservation: RuntimeCommandReservation,
    pub(super) _permit: OwnedSemaphorePermit,
}

struct RuntimeCommandSenderLifetime {
    admission: Arc<RuntimeCommandAdmission>,
}

impl Drop for RuntimeCommandSenderLifetime {
    fn drop(&mut self) {
        self.admission.close_senders();
    }
}

#[derive(Clone)]
pub(super) struct RuntimeCommandSender {
    admission: Arc<RuntimeCommandAdmission>,
    lifetime: Arc<RuntimeCommandSenderLifetime>,
}

impl RuntimeCommandSender {
    pub(super) fn try_send(
        &self,
        request: RuntimeRequest,
    ) -> std::result::Result<RuntimeCommandAdmissionOutcome, Box<RuntimeCommandAdmissionError>>
    {
        self.admission.try_send(request)
    }

    #[cfg(test)]
    pub(super) fn send(&self, request: RuntimeRequest) -> RuntimeCommandAdmissionOutcome {
        match self.try_send(request) {
            Ok(outcome) => outcome,
            Err(error) => error.outcome,
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot(&self) -> RuntimeCommandAdmissionSnapshot {
        self.admission.snapshot()
    }
}

impl std::fmt::Debug for RuntimeCommandSender {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _keep_open = &self.lifetime;
        formatter.write_str("RuntimeCommandSender")
    }
}

struct RuntimeCommandReceiverLifetime {
    admission: Arc<RuntimeCommandAdmission>,
}

impl Drop for RuntimeCommandReceiverLifetime {
    fn drop(&mut self) {
        self.admission.close_receiver();
    }
}

#[derive(Clone)]
pub(super) struct RuntimeCommandReceiver {
    admission: Arc<RuntimeCommandAdmission>,
    lifetime: Arc<RuntimeCommandReceiverLifetime>,
}

impl RuntimeCommandReceiver {
    pub(super) async fn recv_control(&self) -> Option<RuntimeControlRequest> {
        loop {
            let notified = self.admission.control_ready.notified();
            let (request, finished) = self.admission.pop_control();
            if request.is_some() || finished {
                return request;
            }
            notified.await;
        }
    }

    pub(super) async fn recv_lane(&self, lane: RuntimeTaskLane) -> Option<RuntimeLaneRequest> {
        loop {
            let notified = self.admission.lane_ready[lane.index()].notified();
            let (request, finished) = self.admission.pop_lane(lane);
            if request.is_some() || finished {
                return request;
            }
            notified.await;
        }
    }

    pub(super) fn snapshot(&self) -> RuntimeCommandAdmissionSnapshot {
        let _keep_open = &self.lifetime;
        self.admission.snapshot()
    }
}
