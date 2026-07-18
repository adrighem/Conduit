// The production Slack/Chime adapter is capability-gated, so this reusable
// media implementation is exercised by the synthetic harness until that
// private contract can be verified.
#![allow(dead_code)]

use std::fmt;

use crate::huddles::state::{
    HuddleControls, HuddleDeviceKind, HuddleDeviceSelection, HuddleSessionStatistics,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MediaSourceMode {
    #[default]
    System,
    Synthetic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MediaSinkMode {
    #[default]
    System,
    Fake,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaSessionConfig {
    pub source_mode: MediaSourceMode,
    pub sink_mode: MediaSinkMode,
    pub controls: HuddleControls,
    pub devices: HuddleDeviceSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaGraphPlan {
    pub audio_enabled: bool,
    pub camera_capture_enabled: bool,
    pub screen_capture_enabled: bool,
    pub echo_cancellation_enabled: bool,
    pub uses_system_devices: bool,
}

impl MediaGraphPlan {
    pub fn for_session(config: &MediaSessionConfig) -> Self {
        let uses_system_devices = config.source_mode == MediaSourceMode::System;
        Self {
            audio_enabled: true,
            camera_capture_enabled: config.controls.camera_enabled,
            screen_capture_enabled: config.controls.screen_share_enabled,
            echo_cancellation_enabled: uses_system_devices,
            uses_system_devices,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaDescriptionKind {
    Offer,
    Answer,
}

struct SensitiveMediaValue(Box<[u8]>);

impl SensitiveMediaValue {
    fn new(value: &str) -> Result<Self, MediaError> {
        if value.trim().is_empty() {
            return Err(MediaError::InvalidSessionData);
        }
        Ok(Self(value.as_bytes().to_vec().into_boxed_slice()))
    }

    fn expose(&self) -> Result<&str, MediaError> {
        std::str::from_utf8(&self.0).map_err(|_| MediaError::InvalidSessionData)
    }
}

impl fmt::Debug for SensitiveMediaValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for SensitiveMediaValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub struct MediaDescription {
    kind: MediaDescriptionKind,
    sdp: SensitiveMediaValue,
}

impl MediaDescription {
    pub fn offer(sdp: &str) -> Result<Self, MediaError> {
        Self::new(MediaDescriptionKind::Offer, sdp)
    }

    pub fn answer(sdp: &str) -> Result<Self, MediaError> {
        Self::new(MediaDescriptionKind::Answer, sdp)
    }

    fn new(kind: MediaDescriptionKind, sdp: &str) -> Result<Self, MediaError> {
        if !sdp.trim_start().starts_with("v=0") {
            return Err(MediaError::InvalidSessionData);
        }
        Ok(Self {
            kind,
            sdp: SensitiveMediaValue::new(sdp)?,
        })
    }

    pub fn kind(&self) -> MediaDescriptionKind {
        self.kind
    }

    pub fn sdp(&self) -> Result<&str, MediaError> {
        self.sdp.expose()
    }
}

impl fmt::Debug for MediaDescription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaDescription")
            .field("kind", &self.kind)
            .field("sdp", &self.sdp)
            .finish()
    }
}

pub struct IceCandidate {
    sdp_m_line_index: u32,
    value: SensitiveMediaValue,
}

impl IceCandidate {
    pub fn new(sdp_m_line_index: u32, value: &str) -> Result<Self, MediaError> {
        let value = value.trim();
        if !value.starts_with("candidate:") {
            return Err(MediaError::InvalidSessionData);
        }
        Ok(Self {
            sdp_m_line_index,
            value: SensitiveMediaValue::new(value)?,
        })
    }

    pub fn sdp_m_line_index(&self) -> u32 {
        self.sdp_m_line_index
    }

    pub fn value(&self) -> Result<&str, MediaError> {
        self.value.expose()
    }
}

impl fmt::Debug for IceCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IceCandidate")
            .field("sdp_m_line_index", &self.sdp_m_line_index)
            .field("value", &self.value)
            .finish()
    }
}

#[derive(Debug)]
pub enum MediaEvent {
    LocalDescription(MediaDescription),
    LocalIceCandidate(IceCandidate),
    Statistics(HuddleSessionStatistics),
    Failed(MediaError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    #[error("the media session is already running")]
    AlreadyRunning,
    #[error("no media session is running")]
    NotRunning,
    #[error("the media session data was invalid")]
    InvalidSessionData,
    #[error("the required media components are unavailable")]
    ComponentsUnavailable,
    #[error("the selected media device is unavailable")]
    DeviceUnavailable,
    #[error("the media operation failed")]
    OperationFailed,
}

pub trait MediaEngine {
    fn start(&mut self, config: MediaSessionConfig) -> Result<(), MediaError>;
    fn create_offer(&mut self) -> Result<(), MediaError>;
    fn set_remote_description(&mut self, description: MediaDescription) -> Result<(), MediaError>;
    fn add_remote_ice_candidate(&mut self, candidate: IceCandidate) -> Result<(), MediaError>;
    fn apply_controls(&mut self, controls: HuddleControls) -> Result<(), MediaError>;
    fn select_device(&mut self, kind: HuddleDeviceKind, id: &str) -> Result<(), MediaError>;
    fn request_statistics(&mut self) -> Result<(), MediaError>;
    fn drain_events(&mut self) -> Vec<MediaEvent>;
    fn stop(&mut self) -> Result<(), MediaError>;
    fn is_running(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct SyntheticMediaEngine {
    config: Option<MediaSessionConfig>,
    remote_description_set: bool,
    remote_candidates: usize,
    events: Vec<MediaEvent>,
}

impl MediaEngine for SyntheticMediaEngine {
    fn start(&mut self, config: MediaSessionConfig) -> Result<(), MediaError> {
        if self.config.is_some() {
            return Err(MediaError::AlreadyRunning);
        }
        self.config = Some(config);
        self.remote_description_set = false;
        self.remote_candidates = 0;
        self.events.clear();
        Ok(())
    }

    fn create_offer(&mut self) -> Result<(), MediaError> {
        self.require_running()?;
        self.events
            .push(MediaEvent::LocalDescription(MediaDescription::offer(
                "v=0\r\no=conduit-synthetic 0 0 IN IP4 127.0.0.1\r\n",
            )?));
        Ok(())
    }

    fn set_remote_description(&mut self, _description: MediaDescription) -> Result<(), MediaError> {
        self.require_running()?;
        self.remote_description_set = true;
        Ok(())
    }

    fn add_remote_ice_candidate(&mut self, _candidate: IceCandidate) -> Result<(), MediaError> {
        self.require_running()?;
        self.remote_candidates = self.remote_candidates.saturating_add(1);
        Ok(())
    }

    fn apply_controls(&mut self, controls: HuddleControls) -> Result<(), MediaError> {
        self.require_running()?;
        if let Some(config) = self.config.as_mut() {
            config.controls = controls;
        }
        Ok(())
    }

    fn select_device(&mut self, kind: HuddleDeviceKind, id: &str) -> Result<(), MediaError> {
        self.require_running()?;
        if id.trim().is_empty() {
            return Err(MediaError::DeviceUnavailable);
        }
        if let Some(config) = self.config.as_mut() {
            config.devices.select(kind, id.to_string());
        }
        Ok(())
    }

    fn request_statistics(&mut self) -> Result<(), MediaError> {
        self.require_running()?;
        self.events.push(MediaEvent::Statistics(
            MediaStatisticsSample::default().into_session_statistics(),
        ));
        Ok(())
    }

    fn drain_events(&mut self) -> Vec<MediaEvent> {
        std::mem::take(&mut self.events)
    }

    fn stop(&mut self) -> Result<(), MediaError> {
        self.require_running()?;
        self.config = None;
        self.remote_description_set = false;
        self.remote_candidates = 0;
        self.events.clear();
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.config.is_some()
    }
}

impl SyntheticMediaEngine {
    fn require_running(&self) -> Result<(), MediaError> {
        self.is_running()
            .then_some(())
            .ok_or(MediaError::NotRunning)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MediaStatisticsSample {
    pub round_trip_seconds: Option<f64>,
    pub jitter_seconds: Option<f64>,
    pub packets_lost: Option<i64>,
    pub packets_received: Option<u64>,
    pub audio_bitrate_bps: Option<f64>,
    pub video_bitrate_bps: Option<f64>,
}

impl MediaStatisticsSample {
    pub fn into_session_statistics(self) -> HuddleSessionStatistics {
        HuddleSessionStatistics {
            round_trip_ms: seconds_to_milliseconds(self.round_trip_seconds),
            jitter_ms: seconds_to_milliseconds(self.jitter_seconds),
            packets_lost: self.packets_lost.unwrap_or_default().max(0) as u64,
            packets_received: self.packets_received.unwrap_or_default(),
            audio_bitrate_bps: finite_rate(self.audio_bitrate_bps),
            video_bitrate_bps: finite_rate(self.video_bitrate_bps),
        }
    }
}

fn seconds_to_milliseconds(value: Option<f64>) -> u32 {
    let value = value.unwrap_or_default();
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    (value * 1_000.0).floor().min(u32::MAX as f64) as u32
}

fn finite_rate(value: Option<f64>) -> u64 {
    let value = value.unwrap_or_default();
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    value.floor().min(u64::MAX as f64) as u64
}

#[cfg(feature = "native-media")]
mod native {
    use std::sync::mpsc::{self, Receiver, Sender};

    use gst::glib;
    use gst::prelude::*;
    use gstreamer as gst;
    use gstreamer_sdp as gst_sdp;
    use gstreamer_webrtc as gst_webrtc;

    use super::{
        IceCandidate, MediaDescription, MediaDescriptionKind, MediaEngine, MediaError, MediaEvent,
        MediaSessionConfig, MediaSinkMode, MediaSourceMode, MediaStatisticsSample,
    };
    use crate::huddles::devices::NativeDeviceCatalog;
    use crate::huddles::state::{HuddleControls, HuddleDevice, HuddleDeviceKind};

    pub struct GStreamerMediaEngine {
        catalog: NativeDeviceCatalog,
        session: Option<NativeSession>,
        event_sender: Sender<MediaEvent>,
        event_receiver: Receiver<MediaEvent>,
    }

    struct NativeSession {
        pipeline: gst::Pipeline,
        peer: gst::Element,
        microphone_source: gst::Element,
        microphone_valve: gst::Element,
        speaker_sink: gst::Element,
        camera_source: gst::Element,
        camera_valve: gst::Element,
        controls: HuddleControls,
    }

    impl GStreamerMediaEngine {
        pub fn new() -> Result<Self, MediaError> {
            gst::init().map_err(|_| MediaError::ComponentsUnavailable)?;
            ensure_factories()?;
            let catalog = NativeDeviceCatalog::scan()?;
            let (event_sender, event_receiver) = mpsc::channel();
            Ok(Self {
                catalog,
                session: None,
                event_sender,
                event_receiver,
            })
        }

        pub fn devices(&self) -> &[HuddleDevice] {
            self.catalog.descriptions()
        }

        pub fn camera_capture_active(&self) -> bool {
            self.session
                .as_ref()
                .is_some_and(|session| session.controls.camera_enabled)
        }

        fn build_session(&self, config: &MediaSessionConfig) -> Result<NativeSession, MediaError> {
            let pipeline = gst::Pipeline::with_name("conduit-huddle-media");
            let peer = make("webrtcbin", "huddle-webrtc")?;
            peer.set_property_from_str("bundle-policy", "max-bundle");
            pipeline
                .add(&peer)
                .map_err(|_| MediaError::OperationFailed)?;

            connect_ice_events(&peer, self.event_sender.clone())?;
            let (microphone_source, microphone_valve) =
                self.add_audio_sender(&pipeline, &peer, config)?;
            let (camera_source, camera_valve) = self.add_video_sender(&pipeline, &peer, config)?;
            let (audio_target, video_target, speaker_sink) =
                self.add_receivers(&pipeline, config)?;
            connect_incoming_streams(&pipeline, &peer, audio_target, video_target);

            camera_valve.set_property("drop", true);
            camera_source.set_locked_state(true);
            camera_source
                .set_state(gst::State::Null)
                .map_err(|_| MediaError::OperationFailed)?;
            microphone_valve.set_property("drop", config.controls.microphone_muted);

            pipeline
                .set_state(gst::State::Playing)
                .map_err(|_| MediaError::OperationFailed)?;
            if config.controls.camera_enabled {
                set_camera_capture(&camera_source, &camera_valve, true)?;
            }

            Ok(NativeSession {
                pipeline,
                peer,
                microphone_source,
                microphone_valve,
                speaker_sink,
                camera_source,
                camera_valve,
                controls: config.controls.clone(),
            })
        }

        fn add_audio_sender(
            &self,
            pipeline: &gst::Pipeline,
            peer: &gst::Element,
            config: &MediaSessionConfig,
        ) -> Result<(gst::Element, gst::Element), MediaError> {
            let source = match config.source_mode {
                MediaSourceMode::Synthetic => {
                    let source = make("audiotestsrc", "huddle-audio-source")?;
                    source.set_property("is-live", true);
                    source.set_property_from_str("wave", "silence");
                    source
                }
                MediaSourceMode::System => self.system_element(
                    HuddleDeviceKind::Microphone,
                    config.devices.microphone_id.as_deref(),
                    "autoaudiosrc",
                    "huddle-audio-source",
                )?,
            };
            let convert = make("audioconvert", "huddle-audio-convert")?;
            let resample = make("audioresample", "huddle-audio-resample")?;
            let valve = make("valve", "huddle-microphone-valve")?;
            let encoder = make("opusenc", "huddle-opus-encoder")?;
            let payloader = make("rtpopuspay", "huddle-opus-payloader")?;
            let capsfilter = make("capsfilter", "huddle-audio-rtp-caps")?;
            capsfilter.set_property(
                "caps",
                gst::Caps::builder("application/x-rtp")
                    .field("media", "audio")
                    .field("encoding-name", "OPUS")
                    .field("payload", 96i32)
                    .build(),
            );
            let queue = make("queue", "huddle-audio-send-queue")?;

            let mut elements = vec![source.clone(), convert, resample];
            if config.source_mode == MediaSourceMode::System {
                elements.push(make("webrtcdsp", "huddle-audio-dsp")?);
            }
            elements.extend([valve.clone(), encoder, payloader, capsfilter, queue]);
            add_and_link(pipeline, &elements)?;
            link_to_peer(peer, elements.last().expect("audio queue"))?;
            Ok((source, valve))
        }

        fn add_video_sender(
            &self,
            pipeline: &gst::Pipeline,
            peer: &gst::Element,
            config: &MediaSessionConfig,
        ) -> Result<(gst::Element, gst::Element), MediaError> {
            let source = match config.source_mode {
                MediaSourceMode::Synthetic => {
                    let source = make("videotestsrc", "huddle-camera-source")?;
                    source.set_property("is-live", true);
                    source.set_property_from_str("pattern", "smpte");
                    source
                }
                MediaSourceMode::System => self.system_element(
                    HuddleDeviceKind::Camera,
                    config.devices.camera_id.as_deref(),
                    "autovideosrc",
                    "huddle-camera-source",
                )?,
            };
            let queue = make("queue", "huddle-camera-source-queue")?;
            let convert = make("videoconvert", "huddle-camera-convert")?;
            let scale = make("videoscale", "huddle-camera-scale")?;
            let raw_caps = make("capsfilter", "huddle-camera-raw-caps")?;
            raw_caps.set_property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("width", 1280i32)
                    .field("height", 720i32)
                    .field("framerate", gst::Fraction::new(30, 1))
                    .build(),
            );
            let valve = make("valve", "huddle-camera-valve")?;
            let encoder = make("vp8enc", "huddle-vp8-encoder")?;
            encoder.set_property("deadline", 1i64);
            let payloader = make("rtpvp8pay", "huddle-vp8-payloader")?;
            let rtp_caps = make("capsfilter", "huddle-video-rtp-caps")?;
            rtp_caps.set_property(
                "caps",
                gst::Caps::builder("application/x-rtp")
                    .field("media", "video")
                    .field("encoding-name", "VP8")
                    .field("payload", 97i32)
                    .build(),
            );
            let send_queue = make("queue", "huddle-video-send-queue")?;
            let elements = vec![
                source.clone(),
                queue,
                convert,
                scale,
                raw_caps,
                valve.clone(),
                encoder,
                payloader,
                rtp_caps,
                send_queue,
            ];
            add_and_link(pipeline, &elements)?;
            link_to_peer(peer, elements.last().expect("video queue"))?;
            Ok((source, valve))
        }

        fn add_receivers(
            &self,
            pipeline: &gst::Pipeline,
            config: &MediaSessionConfig,
        ) -> Result<(gst::Element, gst::Element, gst::Element), MediaError> {
            let audio_queue = make("queue", "huddle-audio-receive-queue")?;
            let audio_convert = make("audioconvert", "huddle-audio-playback-convert")?;
            let audio_resample = make("audioresample", "huddle-audio-playback-resample")?;
            let audio_sink = match config.sink_mode {
                MediaSinkMode::Fake => {
                    let sink = make("fakesink", "huddle-audio-fake-sink")?;
                    sink.set_property("sync", false);
                    sink
                }
                MediaSinkMode::System => self.system_element(
                    HuddleDeviceKind::Speaker,
                    config.devices.speaker_id.as_deref(),
                    "autoaudiosink",
                    "huddle-audio-sink",
                )?,
            };
            let mut audio_elements = vec![audio_queue.clone(), audio_convert, audio_resample];
            if config.source_mode == MediaSourceMode::System {
                audio_elements.push(make("webrtcechoprobe", "huddle-echo-probe")?);
            }
            audio_elements.push(audio_sink.clone());
            add_and_link(pipeline, &audio_elements)?;

            let video_queue = make("queue", "huddle-video-receive-queue")?;
            let video_convert = make("videoconvert", "huddle-video-playback-convert")?;
            let video_sink = make("fakesink", "huddle-video-fake-sink")?;
            video_sink.set_property("sync", false);
            add_and_link(pipeline, &[video_queue.clone(), video_convert, video_sink])?;
            Ok((audio_queue, video_queue, audio_sink))
        }

        fn system_element(
            &self,
            kind: HuddleDeviceKind,
            selected_id: Option<&str>,
            automatic_factory: &str,
            name: &str,
        ) -> Result<gst::Element, MediaError> {
            match selected_id {
                Some(id) => self.catalog.create_element(kind, id, name),
                None => make(automatic_factory, name),
            }
        }

        fn session(&self) -> Result<&NativeSession, MediaError> {
            self.session.as_ref().ok_or(MediaError::NotRunning)
        }

        fn session_mut(&mut self) -> Result<&mut NativeSession, MediaError> {
            self.session.as_mut().ok_or(MediaError::NotRunning)
        }
    }

    impl MediaEngine for GStreamerMediaEngine {
        fn start(&mut self, config: MediaSessionConfig) -> Result<(), MediaError> {
            if self.session.is_some() {
                return Err(MediaError::AlreadyRunning);
            }
            self.session = Some(self.build_session(&config)?);
            Ok(())
        }

        fn create_offer(&mut self) -> Result<(), MediaError> {
            let peer = self.session()?.peer.clone();
            let sender = self.event_sender.clone();
            let callback_peer = peer.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let result = (|| {
                    let reply = reply
                        .map_err(|_| MediaError::OperationFailed)?
                        .ok_or(MediaError::OperationFailed)?;
                    let offer = reply
                        .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                        .map_err(|_| MediaError::OperationFailed)?;
                    callback_peer.emit_by_name::<()>(
                        "set-local-description",
                        &[&offer, &None::<gst::Promise>],
                    );
                    let sdp = offer
                        .sdp()
                        .as_text()
                        .map_err(|_| MediaError::OperationFailed)?;
                    MediaDescription::offer(&sdp)
                })();
                match result {
                    Ok(description) => {
                        let _ = sender.send(MediaEvent::LocalDescription(description));
                    }
                    Err(error) => {
                        let _ = sender.send(MediaEvent::Failed(error));
                    }
                }
            });
            peer.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
            Ok(())
        }

        fn set_remote_description(
            &mut self,
            description: MediaDescription,
        ) -> Result<(), MediaError> {
            let peer = self.session()?.peer.clone();
            let sdp = gst_sdp::SDPMessage::parse_buffer(description.sdp()?.as_bytes())
                .map_err(|_| MediaError::InvalidSessionData)?;
            let kind = match description.kind() {
                MediaDescriptionKind::Offer => gst_webrtc::WebRTCSDPType::Offer,
                MediaDescriptionKind::Answer => gst_webrtc::WebRTCSDPType::Answer,
            };
            let description = gst_webrtc::WebRTCSessionDescription::new(kind, sdp);
            peer.emit_by_name::<()>(
                "set-remote-description",
                &[&description, &None::<gst::Promise>],
            );
            Ok(())
        }

        fn add_remote_ice_candidate(&mut self, candidate: IceCandidate) -> Result<(), MediaError> {
            let peer = self.session()?.peer.clone();
            peer.emit_by_name::<()>(
                "add-ice-candidate",
                &[&candidate.sdp_m_line_index(), &candidate.value()?],
            );
            Ok(())
        }

        fn apply_controls(&mut self, controls: HuddleControls) -> Result<(), MediaError> {
            if controls.screen_share_enabled {
                return Err(MediaError::OperationFailed);
            }
            let session = self.session_mut()?;
            session
                .microphone_valve
                .set_property("drop", controls.microphone_muted);
            set_camera_capture(
                &session.camera_source,
                &session.camera_valve,
                controls.camera_enabled,
            )?;
            session.controls = controls;
            Ok(())
        }

        fn select_device(&mut self, kind: HuddleDeviceKind, id: &str) -> Result<(), MediaError> {
            let session = self.session()?;
            let element = match kind {
                HuddleDeviceKind::Microphone => &session.microphone_source,
                HuddleDeviceKind::Speaker => &session.speaker_sink,
                HuddleDeviceKind::Camera => &session.camera_source,
            };
            let camera_was_enabled =
                kind == HuddleDeviceKind::Camera && session.controls.camera_enabled;
            if kind == HuddleDeviceKind::Camera {
                set_camera_capture(&session.camera_source, &session.camera_valve, false)?;
            }
            let result = self.catalog.reconfigure_element(kind, id, element);
            if kind == HuddleDeviceKind::Camera && camera_was_enabled {
                set_camera_capture(&session.camera_source, &session.camera_valve, true)?;
            }
            result
        }

        fn request_statistics(&mut self) -> Result<(), MediaError> {
            let peer = self.session()?.peer.clone();
            let sender = self.event_sender.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                let result = reply
                    .ok()
                    .flatten()
                    .map(statistics_from_structure)
                    .ok_or(MediaError::OperationFailed);
                match result {
                    Ok(statistics) => {
                        let _ = sender.send(MediaEvent::Statistics(statistics));
                    }
                    Err(error) => {
                        let _ = sender.send(MediaEvent::Failed(error));
                    }
                }
            });
            peer.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
            Ok(())
        }

        fn drain_events(&mut self) -> Vec<MediaEvent> {
            self.event_receiver.try_iter().collect()
        }

        fn stop(&mut self) -> Result<(), MediaError> {
            let Some(session) = self.session.take() else {
                return Err(MediaError::NotRunning);
            };
            session.camera_valve.set_property("drop", true);
            session.microphone_valve.set_property("drop", true);
            session.camera_source.set_locked_state(true);
            session
                .pipeline
                .set_state(gst::State::Null)
                .map_err(|_| MediaError::OperationFailed)?;
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.session.is_some()
        }
    }

    impl Drop for GStreamerMediaEngine {
        fn drop(&mut self) {
            if let Some(session) = self.session.take() {
                session.camera_valve.set_property("drop", true);
                session.microphone_valve.set_property("drop", true);
                session.camera_source.set_locked_state(true);
                let _ = session.pipeline.set_state(gst::State::Null);
            }
        }
    }

    fn ensure_factories() -> Result<(), MediaError> {
        for factory in [
            "webrtcbin",
            "opusenc",
            "rtpopuspay",
            "vp8enc",
            "rtpvp8pay",
            "decodebin",
            "valve",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                return Err(MediaError::ComponentsUnavailable);
            }
        }
        Ok(())
    }

    fn make(factory: &str, name: &str) -> Result<gst::Element, MediaError> {
        gst::ElementFactory::make(factory)
            .name(name)
            .build()
            .map_err(|_| MediaError::ComponentsUnavailable)
    }

    fn add_and_link(pipeline: &gst::Pipeline, elements: &[gst::Element]) -> Result<(), MediaError> {
        pipeline
            .add_many(elements)
            .map_err(|_| MediaError::OperationFailed)?;
        gst::Element::link_many(elements).map_err(|_| MediaError::OperationFailed)
    }

    fn link_to_peer(peer: &gst::Element, element: &gst::Element) -> Result<(), MediaError> {
        let source_pad = element
            .static_pad("src")
            .ok_or(MediaError::OperationFailed)?;
        let sink_pad = peer
            .request_pad_simple("sink_%u")
            .ok_or(MediaError::OperationFailed)?;
        source_pad
            .link(&sink_pad)
            .map(|_| ())
            .map_err(|_| MediaError::OperationFailed)
    }

    fn set_camera_capture(
        source: &gst::Element,
        valve: &gst::Element,
        enabled: bool,
    ) -> Result<(), MediaError> {
        valve.set_property("drop", !enabled);
        if enabled {
            source.set_locked_state(false);
            source
                .sync_state_with_parent()
                .map_err(|_| MediaError::OperationFailed)
        } else {
            source.set_locked_state(true);
            source
                .set_state(gst::State::Null)
                .map(|_| ())
                .map_err(|_| MediaError::OperationFailed)
        }
    }

    fn connect_ice_events(
        peer: &gst::Element,
        sender: Sender<MediaEvent>,
    ) -> Result<(), MediaError> {
        peer.connect("on-ice-candidate", false, move |values| {
            let index = values.get(1).and_then(|value| value.get::<u32>().ok());
            let candidate = values.get(2).and_then(|value| value.get::<String>().ok());
            match index
                .zip(candidate)
                .ok_or(MediaError::InvalidSessionData)
                .and_then(|(index, candidate)| IceCandidate::new(index, &candidate))
            {
                Ok(candidate) => {
                    let _ = sender.send(MediaEvent::LocalIceCandidate(candidate));
                }
                Err(error) => {
                    let _ = sender.send(MediaEvent::Failed(error));
                }
            }
            None
        });
        Ok(())
    }

    fn connect_incoming_streams(
        pipeline: &gst::Pipeline,
        peer: &gst::Element,
        audio_target: gst::Element,
        video_target: gst::Element,
    ) {
        let pipeline = pipeline.downgrade();
        peer.connect_pad_added(move |_peer, incoming_pad| {
            let Some(pipeline) = pipeline.upgrade() else {
                return;
            };
            let Ok(decodebin) = gst::ElementFactory::make("decodebin").build() else {
                return;
            };
            let audio_target = audio_target.clone();
            let video_target = video_target.clone();
            decodebin.connect_pad_added(move |_decodebin, decoded_pad| {
                let caps = decoded_pad
                    .current_caps()
                    .unwrap_or_else(|| decoded_pad.query_caps(None));
                let media_type = caps.structure(0).map(|structure| structure.name());
                let target = match media_type.as_deref() {
                    Some(name) if name.starts_with("audio/") => &audio_target,
                    Some(name) if name.starts_with("video/") => &video_target,
                    _ => return,
                };
                let Some(sink_pad) = target.static_pad("sink") else {
                    return;
                };
                if !sink_pad.is_linked() {
                    let _ = decoded_pad.link(&sink_pad);
                }
            });
            if pipeline.add(&decodebin).is_err() || decodebin.sync_state_with_parent().is_err() {
                return;
            }
            let Some(sink_pad) = decodebin.static_pad("sink") else {
                return;
            };
            let _ = incoming_pad.link(&sink_pad);
        });
    }

    fn statistics_from_structure(
        reply: &gst::StructureRef,
    ) -> crate::huddles::state::HuddleSessionStatistics {
        let mut sample = MediaStatisticsSample::default();
        accumulate_statistics(reply, &mut sample, None);
        sample.into_session_statistics()
    }

    fn accumulate_statistics(
        structure: &gst::StructureRef,
        sample: &mut MediaStatisticsSample,
        inherited_media: Option<&str>,
    ) {
        let media = structure
            .get_optional::<&str>("media-type")
            .ok()
            .flatten()
            .or_else(|| structure.get_optional::<&str>("kind").ok().flatten())
            .or(inherited_media);
        for (name, value) in structure.iter() {
            let name = name.as_str();
            match name {
                "round-trip-time" | "current-round-trip-time" => {
                    sample.round_trip_seconds = max_float(sample.round_trip_seconds, number(value));
                }
                "jitter" => {
                    sample.jitter_seconds = max_float(sample.jitter_seconds, number(value));
                }
                "packets-lost" => {
                    let value = signed_number(value).unwrap_or_default();
                    sample.packets_lost = Some(
                        sample
                            .packets_lost
                            .unwrap_or_default()
                            .saturating_add(value),
                    );
                }
                "packets-received" => {
                    let value = unsigned_number(value).unwrap_or_default();
                    sample.packets_received = Some(
                        sample
                            .packets_received
                            .unwrap_or_default()
                            .saturating_add(value),
                    );
                }
                "bitrate" | "bitrate-mean" => {
                    let target = if media.is_some_and(|media| media.contains("video")) {
                        &mut sample.video_bitrate_bps
                    } else {
                        &mut sample.audio_bitrate_bps
                    };
                    *target = Some(target.unwrap_or_default() + number(value).unwrap_or_default());
                }
                _ => {}
            }
            if let Ok(nested) = value.get::<gst::Structure>() {
                accumulate_statistics(nested.as_ref(), sample, media);
            }
        }
    }

    fn number(value: &glib::SendValue) -> Option<f64> {
        value
            .get::<f64>()
            .ok()
            .or_else(|| value.get::<f32>().ok().map(f64::from))
            .or_else(|| value.get::<u64>().ok().map(|value| value as f64))
            .or_else(|| value.get::<u32>().ok().map(f64::from))
            .or_else(|| value.get::<i64>().ok().map(|value| value as f64))
            .or_else(|| value.get::<i32>().ok().map(f64::from))
    }

    fn signed_number(value: &glib::SendValue) -> Option<i64> {
        value
            .get::<i64>()
            .ok()
            .or_else(|| value.get::<i32>().ok().map(i64::from))
            .or_else(|| {
                value
                    .get::<u64>()
                    .ok()
                    .map(|value| value.min(i64::MAX as u64) as i64)
            })
    }

    fn unsigned_number(value: &glib::SendValue) -> Option<u64> {
        value
            .get::<u64>()
            .ok()
            .or_else(|| value.get::<u32>().ok().map(u64::from))
            .or_else(|| value.get::<i64>().ok().map(|value| value.max(0) as u64))
    }

    fn max_float(current: Option<f64>, next: Option<f64>) -> Option<f64> {
        match (current, next) {
            (Some(current), Some(next)) => Some(current.max(next)),
            (current, next) => current.or(next),
        }
    }
}

#[cfg(feature = "native-media")]
#[allow(unused_imports)]
pub use native::GStreamerMediaEngine;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::huddles::state::{HuddleControls, HuddleDeviceSelection};

    #[test]
    fn graph_plan_never_enables_visual_capture_implicitly() {
        let plan = MediaGraphPlan::for_session(&MediaSessionConfig::default());

        assert!(plan.audio_enabled);
        assert!(plan.echo_cancellation_enabled);
        assert!(!plan.camera_capture_enabled);
        assert!(!plan.screen_capture_enabled);
    }

    #[test]
    fn synthetic_sources_do_not_request_real_devices_or_echo_processing() {
        let config = MediaSessionConfig {
            source_mode: MediaSourceMode::Synthetic,
            sink_mode: MediaSinkMode::Fake,
            controls: HuddleControls::default(),
            devices: HuddleDeviceSelection::default(),
        };

        let plan = MediaGraphPlan::for_session(&config);
        assert!(!plan.uses_system_devices);
        assert!(!plan.echo_cancellation_enabled);
    }

    #[test]
    fn descriptions_and_candidates_are_redacted_from_debug_output() {
        let description = MediaDescription::offer("v=0\r\na=ice-ufrag:private\r\n").unwrap();
        let candidate = IceCandidate::new(0, "candidate:private-address").unwrap();

        let debug = format!("{description:?} {candidate:?}");
        assert!(!debug.contains("private"));
        assert!(debug.contains("<redacted>"));
        assert_eq!(description.sdp().unwrap(), "v=0\r\na=ice-ufrag:private\r\n");
        assert_eq!(candidate.value().unwrap(), "candidate:private-address");
    }

    #[test]
    fn synthetic_engine_exercises_negotiation_controls_statistics_and_teardown() {
        let mut engine = SyntheticMediaEngine::default();
        engine
            .start(MediaSessionConfig {
                source_mode: MediaSourceMode::Synthetic,
                sink_mode: MediaSinkMode::Fake,
                ..Default::default()
            })
            .unwrap();

        engine.create_offer().unwrap();
        let events = engine.drain_events();
        assert!(matches!(
            events.as_slice(),
            [MediaEvent::LocalDescription(_)]
        ));

        engine
            .set_remote_description(MediaDescription::answer("v=0\r\n").unwrap())
            .unwrap();
        engine
            .add_remote_ice_candidate(IceCandidate::new(0, "candidate:test").unwrap())
            .unwrap();
        engine
            .apply_controls(HuddleControls {
                microphone_muted: true,
                camera_enabled: true,
                screen_share_enabled: false,
            })
            .unwrap();
        engine
            .select_device(HuddleDeviceKind::Camera, "camera:synthetic")
            .unwrap();
        engine.request_statistics().unwrap();

        assert!(matches!(
            engine.drain_events().as_slice(),
            [MediaEvent::Statistics(_)]
        ));
        assert!(engine.is_running());
        engine.stop().unwrap();
        assert!(!engine.is_running());
    }

    #[test]
    fn media_statistics_saturate_untrusted_floating_point_values() {
        let statistics = MediaStatisticsSample {
            round_trip_seconds: Some(0.025),
            jitter_seconds: Some(0.004),
            packets_lost: Some(-7),
            packets_received: Some(u64::MAX),
            audio_bitrate_bps: Some(f64::INFINITY),
            video_bitrate_bps: Some(2_500_000.8),
        }
        .into_session_statistics();

        assert_eq!(statistics.round_trip_ms, 25);
        assert_eq!(statistics.jitter_ms, 4);
        assert_eq!(statistics.packets_lost, 0);
        assert_eq!(statistics.packets_received, u64::MAX);
        assert_eq!(statistics.audio_bitrate_bps, 0);
        assert_eq!(statistics.video_bitrate_bps, 2_500_000);
    }

    #[cfg(feature = "native-media")]
    #[test]
    fn native_synthetic_pipeline_keeps_camera_off_until_explicitly_enabled() {
        let mut engine = GStreamerMediaEngine::new().unwrap();
        engine
            .start(MediaSessionConfig {
                source_mode: MediaSourceMode::Synthetic,
                sink_mode: MediaSinkMode::Fake,
                ..Default::default()
            })
            .unwrap();

        assert!(!engine.camera_capture_active());
        engine
            .apply_controls(HuddleControls {
                microphone_muted: true,
                camera_enabled: true,
                screen_share_enabled: false,
            })
            .unwrap();
        assert!(engine.camera_capture_active());

        engine
            .apply_controls(HuddleControls {
                microphone_muted: true,
                camera_enabled: false,
                screen_share_enabled: false,
            })
            .unwrap();
        assert!(!engine.camera_capture_active());
        engine.stop().unwrap();
    }
}
