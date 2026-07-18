// Native devices are consumed only when the capability-gated media adapter is
// selected; keep the catalog available to feature builds and the harness.
#![allow(dead_code)]

use sha2::{Digest, Sha256};

use crate::huddles::state::HuddleDeviceKind;

pub fn stable_device_id(kind: HuddleDeviceKind, native_identity: &str) -> String {
    let prefix = match kind {
        HuddleDeviceKind::Microphone => "microphone",
        HuddleDeviceKind::Speaker => "speaker",
        HuddleDeviceKind::Camera => "camera",
    };
    let digest = Sha256::digest(format!("{prefix}\0{native_identity}").as_bytes());
    format!("{prefix}:{digest:x}")
}

pub fn huddle_device_kind(device_class: &str) -> Option<HuddleDeviceKind> {
    let classes = device_class.split('/').collect::<Vec<_>>();
    match classes.as_slice() {
        ["Audio", "Source", ..] => Some(HuddleDeviceKind::Microphone),
        ["Audio", "Sink", ..] => Some(HuddleDeviceKind::Speaker),
        ["Video", "Source", ..] => Some(HuddleDeviceKind::Camera),
        _ => None,
    }
}

#[cfg(feature = "native-media")]
mod native {
    use std::collections::HashMap;

    use gst::prelude::*;
    use gstreamer as gst;

    use super::{huddle_device_kind, stable_device_id};
    use crate::huddles::media::MediaError;
    use crate::huddles::state::{HuddleDevice, HuddleDeviceKind};

    struct NativeDevice {
        kind: HuddleDeviceKind,
        device: gst::Device,
    }

    pub struct NativeDeviceCatalog {
        descriptions: Vec<HuddleDevice>,
        devices: HashMap<String, NativeDevice>,
    }

    impl NativeDeviceCatalog {
        pub fn scan() -> Result<Self, MediaError> {
            gst::init().map_err(|_| MediaError::ComponentsUnavailable)?;
            let monitor = gst::DeviceMonitor::new();
            for class in ["Audio/Source", "Audio/Sink", "Video/Source"] {
                if monitor.add_filter(Some(class), None).is_none() {
                    return Err(MediaError::ComponentsUnavailable);
                }
            }
            monitor
                .start()
                .map_err(|_| MediaError::ComponentsUnavailable)?;

            let mut descriptions = Vec::new();
            let mut devices = HashMap::new();
            let mut have_default_microphone = false;
            let mut have_default_speaker = false;
            let mut have_default_camera = false;
            for device in monitor.devices() {
                let class = device.device_class();
                let Some(kind) = huddle_device_kind(class.as_str()) else {
                    continue;
                };
                let label = device.display_name().to_string();
                let properties = device
                    .properties()
                    .map(|properties| properties.to_string())
                    .unwrap_or_default();
                let id = stable_device_id(kind, &format!("{class}\0{label}\0{properties}"));
                let is_default = match kind {
                    HuddleDeviceKind::Microphone => first(&mut have_default_microphone),
                    HuddleDeviceKind::Speaker => first(&mut have_default_speaker),
                    HuddleDeviceKind::Camera => first(&mut have_default_camera),
                };
                descriptions.push(HuddleDevice {
                    id: id.clone(),
                    label,
                    kind,
                    is_default,
                });
                devices.insert(id, NativeDevice { kind, device });
            }
            monitor.stop();
            descriptions.sort_by(|left, right| {
                device_kind_order(left.kind)
                    .cmp(&device_kind_order(right.kind))
                    .then_with(|| right.is_default.cmp(&left.is_default))
                    .then_with(|| left.label.cmp(&right.label))
            });
            Ok(Self {
                descriptions,
                devices,
            })
        }

        pub fn descriptions(&self) -> &[HuddleDevice] {
            &self.descriptions
        }

        pub fn create_element(
            &self,
            kind: HuddleDeviceKind,
            id: &str,
            name: &str,
        ) -> Result<gst::Element, MediaError> {
            let native = self
                .devices
                .get(id)
                .filter(|native| native.kind == kind)
                .ok_or(MediaError::DeviceUnavailable)?;
            native
                .device
                .create_element(Some(name))
                .map_err(|_| MediaError::DeviceUnavailable)
        }

        pub fn reconfigure_element(
            &self,
            kind: HuddleDeviceKind,
            id: &str,
            element: &gst::Element,
        ) -> Result<(), MediaError> {
            let native = self
                .devices
                .get(id)
                .filter(|native| native.kind == kind)
                .ok_or(MediaError::DeviceUnavailable)?;
            native
                .device
                .reconfigure_element(element)
                .map_err(|_| MediaError::DeviceUnavailable)
        }
    }

    fn first(seen: &mut bool) -> bool {
        let first = !*seen;
        *seen = true;
        first
    }

    fn device_kind_order(kind: HuddleDeviceKind) -> u8 {
        match kind {
            HuddleDeviceKind::Microphone => 0,
            HuddleDeviceKind::Speaker => 1,
            HuddleDeviceKind::Camera => 2,
        }
    }
}

#[cfg(feature = "native-media")]
pub use native::NativeDeviceCatalog;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_device_ids_are_kind_scoped_and_do_not_expose_native_identity() {
        let identity = "alsa:device=/dev/snd/private-label";
        let microphone = stable_device_id(HuddleDeviceKind::Microphone, identity);
        let speaker = stable_device_id(HuddleDeviceKind::Speaker, identity);

        assert_ne!(microphone, speaker);
        assert!(microphone.starts_with("microphone:"));
        assert!(speaker.starts_with("speaker:"));
        assert!(!microphone.contains("alsa"));
        assert!(!microphone.contains("private-label"));
    }

    #[test]
    fn native_device_classes_map_only_to_supported_huddle_kinds() {
        assert_eq!(
            huddle_device_kind("Audio/Source"),
            Some(HuddleDeviceKind::Microphone)
        );
        assert_eq!(
            huddle_device_kind("Audio/Sink"),
            Some(HuddleDeviceKind::Speaker)
        );
        assert_eq!(
            huddle_device_kind("Video/Source"),
            Some(HuddleDeviceKind::Camera)
        );
        assert_eq!(huddle_device_kind("Video/Sink"), None);
    }
}
