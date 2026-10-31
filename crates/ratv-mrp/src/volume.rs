//! Volume and mute state of the connected device and its output devices.

use crate::proto::{
    AvOutputDeviceDescriptor, DeviceInfoMessage, GetVolumeControlCapabilitiesResultMessage,
    GetVolumeMutedResultMessage, GetVolumeResultMessage, UpdateOutputDeviceMessage,
    VolumeControlAvailabilityMessage, VolumeControlCapabilitiesDidChangeMessage,
    VolumeDidChangeMessage, VolumeMutedDidChangeMessage,
};
use crate::{Envelope, Error};

/// Volume of the connected device.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Volume {
    /// Level from 0.0 to 1.0. `None` when volume control is not available.
    pub level: Option<f32>,
    /// True when muted. `None` when volume control is not available.
    pub muted: Option<bool>,
    /// False when the device cannot change the volume, for example with HDMI audio without CEC.
    pub available: Option<bool>,
    /// Capability flags as sent by the device.
    pub capabilities: Option<i32>,
    /// AirPlay output devices in the group of the connected device, for example `HomePods`.
    pub outputs: Vec<OutputDevice>,
}

/// AirPlay output device in a group.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct OutputDevice {
    /// UID used in volume and output messages.
    pub uid: String,
    /// Name shown to users.
    pub name: String,
    /// Level from 0.0 to 1.0, when known.
    pub volume: Option<f32>,
    /// True when muted, when known.
    pub muted: Option<bool>,
}

/// Output device UID used for volume messages: the cluster id if the device is in a group.
#[must_use]
pub fn output_device_uid(info: &DeviceInfoMessage) -> Option<&str> {
    info.cluster_id.as_deref().or(info.device_uid.as_deref())
}

/// Tracks the volume of the connected device from MRP messages.
#[derive(Debug, Default)]
pub struct VolumeState {
    uid: Option<String>,
    volume: Volume,
}

impl VolumeState {
    /// Creates an empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Output device UID from the last device info.
    #[must_use]
    pub fn output_device_uid(&self) -> Option<&str> {
        self.uid.as_deref()
    }

    /// Current volume.
    #[must_use]
    pub const fn volume(&self) -> &Volume {
        &self.volume
    }

    /// Sets the connected device and its output devices from its device info.
    pub fn set_device(&mut self, info: &DeviceInfoMessage) {
        self.uid = output_device_uid(info).map(str::to_owned);
        // The device itself plays audio only when it leads the group and is no proxy.
        let own = (info.is_group_leader == Some(true) && info.is_proxy_group_player != Some(true))
            .then_some((info.unique_identifier.as_deref(), info.name.as_str()));
        let grouped = info
            .grouped_devices
            .iter()
            .map(|d| (d.device_uid.as_deref(), d.name.as_str()));
        let old = std::mem::take(&mut self.volume.outputs);
        self.volume.outputs = own
            .into_iter()
            .chain(grouped)
            .filter_map(|(uid, name)| {
                let uid = uid?;
                // Keep known volumes of devices that stay in the group.
                let known = old.iter().find(|o| o.uid == uid);
                Some(OutputDevice {
                    uid: uid.to_owned(),
                    name: name.to_owned(),
                    volume: known.and_then(|o| o.volume),
                    muted: known.and_then(|o| o.muted),
                })
            })
            .collect();
    }

    /// Updates the state from a message. Returns true if the volume changed.
    pub fn apply(&mut self, env: &Envelope) -> Result<bool, Error> {
        let old = self.volume.clone();
        if env.is::<DeviceInfoMessage>() {
            self.set_device(&env.payload()?);
        } else if env.is::<VolumeDidChangeMessage>() {
            let msg: VolumeDidChangeMessage = env.payload()?;
            if self.is_ours(msg.output_device_uid.as_deref()) {
                self.volume.level = msg.volume.or(self.volume.level);
            }
            if let Some(o) = self.output(msg.output_device_uid.as_deref()) {
                o.volume = msg.volume.or(o.volume);
            }
        } else if env.is::<VolumeMutedDidChangeMessage>() {
            let msg: VolumeMutedDidChangeMessage = env.payload()?;
            if self.is_ours(msg.output_device_uid.as_deref()) {
                self.volume.muted = msg.is_muted.or(self.volume.muted);
            }
            if let Some(o) = self.output(msg.output_device_uid.as_deref()) {
                o.muted = msg.is_muted.or(o.muted);
            }
        } else if env.is::<VolumeControlAvailabilityMessage>() {
            self.set_capabilities(&env.payload()?);
        } else if env.is::<VolumeControlCapabilitiesDidChangeMessage>() {
            let msg: VolumeControlCapabilitiesDidChangeMessage = env.payload()?;
            if let Some(caps) = msg.capabilities
                && self.is_ours(msg.output_device_uid.as_deref())
            {
                self.set_capabilities(&caps);
            }
        } else if env.is::<UpdateOutputDeviceMessage>() {
            let msg: UpdateOutputDeviceMessage = env.payload()?;
            self.update_output_devices(&msg);
        } else if env.is::<GetVolumeControlCapabilitiesResultMessage>() {
            let msg: GetVolumeControlCapabilitiesResultMessage = env.payload()?;
            if let Some(caps) = msg.capabilities {
                self.set_capabilities(&caps);
            }
        } else if env.is::<GetVolumeResultMessage>() {
            let msg: GetVolumeResultMessage = env.payload()?;
            self.volume.level = msg.volume.or(self.volume.level);
        } else if env.is::<GetVolumeMutedResultMessage>() {
            let msg: GetVolumeMutedResultMessage = env.payload()?;
            self.volume.muted = msg.is_muted.or(self.volume.muted);
        }
        // The device reports level 0 when it has no volume control.
        if self.volume.available == Some(false) {
            self.volume.level = None;
            self.volume.muted = None;
        }
        Ok(self.volume != old)
    }

    /// Takes volumes from the output device list.
    fn update_output_devices(&mut self, msg: &UpdateOutputDeviceMessage) {
        let devices = if msg.cluster_aware_output_devices.is_empty() {
            &msg.output_devices
        } else {
            &msg.cluster_aware_output_devices
        };
        for d in devices {
            if let Some(o) = self.output(d.unique_identifier.as_deref()) {
                o.volume = d.volume.or(o.volume);
                o.muted = d.volume_muted.or(o.muted);
            }
        }
        let Some(ours) = self.uid.as_deref() else {
            return;
        };
        let Some(d) = devices
            .iter()
            .find(|d: &&AvOutputDeviceDescriptor| d.unique_identifier.as_deref() == Some(ours))
        else {
            return;
        };
        let v = &mut self.volume;
        v.level = d.volume.or(v.level);
        v.muted = d.volume_muted.or(v.muted);
        v.available = d.is_volume_control_available.or(v.available);
        v.capabilities = d.volume_capabilities.or(v.capabilities);
    }

    fn set_capabilities(&mut self, msg: &VolumeControlAvailabilityMessage) {
        self.volume.available = msg.volume_control_available.or(self.volume.available);
        self.volume.capabilities = msg.volume_capabilities.or(self.volume.capabilities);
    }

    /// Output device with this UID.
    fn output(&mut self, uid: Option<&str>) -> Option<&mut OutputDevice> {
        let uid = uid?;
        self.volume.outputs.iter_mut().find(|o| o.uid == uid)
    }

    /// Messages without a UID are for the connected device.
    fn is_ours(&self, uid: Option<&str>) -> bool {
        match (uid, &self.uid) {
            (Some(uid), Some(ours)) => uid == ours,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::*;
    use crate::{Payload, encode, frames};

    fn env<P: Payload>(p: &P) -> Envelope {
        let mut out = BytesMut::new();
        encode(p, None, &mut out);
        frames(out.freeze()).next().unwrap().unwrap()
    }

    fn connected() -> VolumeState {
        let mut s = VolumeState::new();
        let info = DeviceInfoMessage {
            device_uid: Some("tv".into()),
            ..Default::default()
        };
        s.apply(&env(&info)).unwrap();
        s
    }

    #[test]
    fn uid_prefers_cluster() {
        let mut info = DeviceInfoMessage {
            device_uid: Some("tv".into()),
            ..Default::default()
        };
        assert_eq!(output_device_uid(&info), Some("tv"));
        info.cluster_id = Some("group".into());
        assert_eq!(output_device_uid(&info), Some("group"));
    }

    #[test]
    fn volume_changes() {
        let mut s = connected();
        assert_eq!(s.output_device_uid(), Some("tv"));
        let msg = VolumeDidChangeMessage {
            volume: Some(0.4),
            output_device_uid: Some("tv".into()),
            ..Default::default()
        };
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(s.volume().level, Some(0.4));
        // Same value again is no change.
        assert!(!s.apply(&env(&msg)).unwrap());

        // Other output devices are ignored.
        let other = VolumeDidChangeMessage {
            volume: Some(0.9),
            output_device_uid: Some("speaker".into()),
            ..Default::default()
        };
        assert!(!s.apply(&env(&other)).unwrap());
        assert_eq!(s.volume().level, Some(0.4));
    }

    #[test]
    fn mute_and_results() {
        let mut s = connected();
        let muted = VolumeMutedDidChangeMessage {
            is_muted: Some(true),
            output_device_uid: None,
        };
        assert!(s.apply(&env(&muted)).unwrap());
        assert_eq!(s.volume().muted, Some(true));

        let level = GetVolumeResultMessage { volume: Some(0.7) };
        assert!(s.apply(&env(&level)).unwrap());
        let muted = GetVolumeMutedResultMessage {
            is_muted: Some(false),
        };
        assert!(s.apply(&env(&muted)).unwrap());
        assert_eq!(
            s.volume(),
            &Volume {
                level: Some(0.7),
                muted: Some(false),
                ..Volume::default()
            }
        );
    }

    #[test]
    fn output_devices() {
        let mut s = connected();
        let device = |uid: &str, volume| AvOutputDeviceDescriptor {
            unique_identifier: Some(uid.into()),
            volume: Some(volume),
            volume_muted: Some(false),
            is_volume_control_available: Some(true),
            volume_capabilities: Some(3),
            ..Default::default()
        };
        let msg = UpdateOutputDeviceMessage {
            output_devices: vec![device("speaker", 0.9), device("tv", 0.25)],
            ..Default::default()
        };
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(
            s.volume(),
            &Volume {
                level: Some(0.25),
                muted: Some(false),
                available: Some(true),
                capabilities: Some(3),
                outputs: Vec::new(),
            }
        );
    }

    #[test]
    fn group_outputs() {
        let mut s = VolumeState::new();
        let member = |uid: &str, name: &str| DeviceInfoMessage {
            device_uid: Some(uid.into()),
            name: name.into(),
            ..Default::default()
        };
        let info = DeviceInfoMessage {
            unique_identifier: Some("tv".into()),
            device_uid: Some("tv".into()),
            name: "Living Room".into(),
            is_group_leader: Some(true),
            grouped_devices: vec![member("hp1", "Kitchen")],
            ..Default::default()
        };
        assert!(s.apply(&env(&info)).unwrap());
        let names: Vec<_> = s.volume().outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Living Room", "Kitchen"]);

        // Volume of another device goes to its output, not to the connected device.
        let msg = VolumeDidChangeMessage {
            volume: Some(0.6),
            output_device_uid: Some("hp1".into()),
            ..Default::default()
        };
        assert!(s.apply(&env(&msg)).unwrap());
        assert_eq!(s.volume().level, None);
        assert_eq!(s.volume().outputs[1].volume, Some(0.6));

        // A second speaker joins; the first keeps its volume.
        let mut info = info;
        info.grouped_devices.push(member("hp2", "Bedroom"));
        assert!(s.apply(&env(&info)).unwrap());
        assert_eq!(s.volume().outputs.len(), 3);
        assert_eq!(s.volume().outputs[1].volume, Some(0.6));

        // A proxy is not an output itself.
        info.is_proxy_group_player = Some(true);
        assert!(s.apply(&env(&info)).unwrap());
        assert_eq!(s.volume().outputs[0].uid, "hp1");
    }

    #[test]
    fn capabilities() {
        let mut s = connected();
        let caps = VolumeControlAvailabilityMessage {
            volume_control_available: Some(true),
            volume_capabilities: Some(3),
        };
        assert!(s.apply(&env(&caps)).unwrap());
        let changed = VolumeControlCapabilitiesDidChangeMessage {
            capabilities: Some(VolumeControlAvailabilityMessage {
                volume_control_available: Some(false),
                volume_capabilities: Some(0),
            }),
            output_device_uid: Some("tv".into()),
            ..Default::default()
        };
        assert!(s.apply(&env(&changed)).unwrap());
        assert_eq!(s.volume().available, Some(false));
        assert_eq!(s.volume().capabilities, Some(0));

        // Without volume control the level is unknown, also when the device reports one.
        let level = GetVolumeResultMessage { volume: Some(0.0) };
        assert!(!s.apply(&env(&level)).unwrap());
        assert_eq!(s.volume().level, None);
    }
}
