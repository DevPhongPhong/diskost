//! UDP sender for encrypted samples.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};

use crate::packet::{encode, split_sample};
use crate::sample::MediaSample;

pub struct UdpUplink {
    socket: UdpSocket,
    remote: SocketAddr,
    room_id: String,
    participant_id: String,
}

impl UdpUplink {
    pub fn connect(remote: SocketAddr, room_id: String, participant_id: String) -> Result<Self, String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|err| err.to_string())?;
        Ok(Self {
            socket,
            remote,
            room_id,
            participant_id,
        })
    }

    pub fn send_sample(&self, sample: &MediaSample) -> Result<(), String> {
        let packets = split_sample(sample, &self.room_id, &self.participant_id).map_err(|err| err.to_string())?;
        for packet in packets {
            let bytes = encode(&packet).map_err(|err| err.to_string())?;
            self.socket
                .send_to(&bytes, self.remote)
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }
}

/// Host of a `ws://` or `wss://` URL, with the recording port.
pub fn udp_target(ws_url: &str, port: u16) -> Result<SocketAddr, String> {
    let trimmed = ws_url.trim();
    let rest = trimmed
        .strip_prefix("ws://")
        .or_else(|| trimmed.strip_prefix("wss://"))
        .ok_or_else(|| format!("not a websocket url: {ws_url}"))?;
    let hostport = rest.split('/').next().unwrap_or(rest);
    let host = if let Some(inside) = hostport.strip_prefix('[') {
        inside.split(']').next().unwrap_or(inside)
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    if host.is_empty() {
        return Err("websocket url has no host".into());
    }
    let target = format!("{host}:{port}");
    target
        .to_socket_addrs()
        .map_err(|err| err.to_string())?
        .next()
        .ok_or_else(|| format!("could not resolve {target}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{MediaSample, SourceKind, Stage};
    use std::net::UdpSocket;
    use std::time::Duration;

    #[test]
    fn parses_the_host_from_the_signaling_url() {
        let addr = udp_target("ws://127.0.0.1:7880/ws", 7881).unwrap();
        assert_eq!(addr.port(), 7881);
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn loopback_delivers_one_datagram() {
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let uplink = UdpUplink::connect(listener.local_addr().unwrap(), "AB".into(), "p1".into()).unwrap();
        let mut sample = MediaSample::video(SourceKind::Display, 4, 1, 1, vec![9, 8, 7]);
        sample.stage = Stage::Encrypted;
        uplink.send_sample(&sample).unwrap();
        let mut buf = [0u8; 1500];
        let (size, _) = listener.recv_from(&mut buf).unwrap();
        let packet = crate::packet::decode(&buf[..size]).unwrap();
        assert_eq!(packet.room_id, "AB");
        assert_eq!(packet.participant_id, "p1");
        assert_eq!(packet.payload, sample.bytes);
    }
}
