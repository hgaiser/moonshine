//! Authorization carried from an authenticated HTTPS launch/resume to the
//! auxiliary RTSP, control and media sockets.
//!
//! Every `/launch` and `/resume` creates a new generation. A generation binds
//! the paired client's normalized IP address and fresh session identifiers.
//! Moonlight clients with Sunshine's `ML_FF_SESSION_ID_V1` echo those
//! identifiers back: the `X-SS-Ping-Payload` RTSP SETUP header becomes the
//! media PING payload, and `X-SS-Connect-Data` becomes the ENet connect data.
//! Older clients only send a literal `PING` and zero connect data; for them the
//! generation can only be enforced by source IP. Source ports are deliberately
//! unconstrained so NAT rebinding and dynamic client ports keep working.
//!
//! Neither identifier is cryptographic: RTSP is plaintext, so an on-path
//! observer can learn them. Control messages are additionally authenticated by
//! AES-GCM with the HTTPS-delivered session key (see the control stream).

use std::net::{IpAddr, SocketAddr};

use aws_lc_rs::rand::{SecureRandom, SystemRandom};

/// Moonlight `x-ml-general.featureFlags` bit for `X-SS-Ping-Payload` and
/// `X-SS-Connect-Data` support.
pub(crate) const ML_FF_SESSION_ID_V1: u32 = 0x02;

/// Length of the `X-SS-Ping-Payload` value.
pub(crate) const PING_PAYLOAD_LENGTH: usize = 16;

/// Sunshine `SS_PING`: the payload followed by a big-endian ping counter.
const SESSION_PING_LENGTH: usize = PING_PAYLOAD_LENGTH + 4;

const LEGACY_PING: &[u8] = b"PING";

const PAYLOAD_ALPHABET: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// Media socket that received an endpoint discovery datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaStream {
	Audio,
	Video,
}

/// Authorization of one launch/resume generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamAuthorization {
	generation: u64,
	client_ip: IpAddr,
	audio_ping_payload: [u8; PING_PAYLOAD_LENGTH],
	video_ping_payload: [u8; PING_PAYLOAD_LENGTH],
	control_connect_data: u32,
	session_id_v1: bool,
}

impl StreamAuthorization {
	pub(crate) fn new(generation: u64, client_ip: IpAddr) -> Result<Self, ()> {
		let mut random = [0u8; 2 * PING_PAYLOAD_LENGTH + 4];
		SystemRandom::new()
			.fill(&mut random)
			.map_err(|_| tracing::error!("Failed to generate stream session identifiers"))?;
		let payload = |bytes: &[u8]| -> [u8; PING_PAYLOAD_LENGTH] {
			std::array::from_fn(|i| PAYLOAD_ALPHABET[usize::from(bytes[i]) % PAYLOAD_ALPHABET.len()])
		};
		let connect_data = u32::from_le_bytes(random[2 * PING_PAYLOAD_LENGTH..].try_into().unwrap_or_default());
		Ok(Self {
			generation,
			client_ip: canonical_ip(client_ip),
			audio_ping_payload: payload(&random[..PING_PAYLOAD_LENGTH]),
			video_ping_payload: payload(&random[PING_PAYLOAD_LENGTH..2 * PING_PAYLOAD_LENGTH]),
			// Moonlight reads the value with `strtoul(.., 0)` and treats zero as
			// "no connect data", so keep it nonzero (a decimal without a leading 0).
			control_connect_data: connect_data.max(1),
			session_id_v1: false,
		})
	}

	/// Monotonic launch/resume generation; never reused within a server process.
	pub fn generation(&self) -> u64 {
		self.generation
	}

	/// Normalized IP address of the client that authenticated this generation.
	pub fn client_ip(&self) -> IpAddr {
		self.client_ip
	}

	/// `X-SS-Ping-Payload` value for a media stream's RTSP SETUP response.
	pub(crate) fn ping_payload(&self, stream: MediaStream) -> &str {
		let payload = match stream {
			MediaStream::Audio => &self.audio_ping_payload,
			MediaStream::Video => &self.video_ping_payload,
		};
		// Generated from an ASCII alphabet.
		std::str::from_utf8(payload).unwrap_or_default()
	}

	/// `X-SS-Connect-Data` value for the control stream's RTSP SETUP response.
	pub(crate) fn control_connect_data(&self) -> u32 {
		self.control_connect_data
	}

	/// Record that the client announced `ML_FF_SESSION_ID_V1`. This is sticky
	/// for the generation so a later request cannot downgrade discovery to
	/// legacy IP-only matching.
	pub(crate) fn require_session_id(&mut self) {
		self.session_id_v1 = true;
	}

	/// Whether a TCP/UDP peer address belongs to the authorized client.
	pub fn admits_peer(&self, ip: IpAddr) -> bool {
		canonical_ip(ip) == self.client_ip
	}

	/// Whether a datagram may (re)discover the client endpoint of a media stream.
	pub(crate) fn admits_media_ping(&self, stream: MediaStream, from: SocketAddr, datagram: &[u8]) -> bool {
		if !self.admits_peer(from.ip()) {
			return false;
		}
		let expected = match stream {
			MediaStream::Audio => &self.audio_ping_payload,
			MediaStream::Video => &self.video_ping_payload,
		};
		match datagram.len() {
			SESSION_PING_LENGTH => {
				aws_lc_rs::constant_time::verify_slices_are_equal(&datagram[..PING_PAYLOAD_LENGTH], expected).is_ok()
			},
			_ => datagram == LEGACY_PING && !self.session_id_v1,
		}
	}

	/// Whether a new ENet peer may become a control candidate for this generation.
	pub(crate) fn admits_control_connect(&self, from: SocketAddr, data: u32) -> bool {
		self.admits_peer(from.ip()) && (data == self.control_connect_data || (data == 0 && !self.session_id_v1))
	}
}

/// Collapse IPv4-mapped IPv6 (`::ffff:a.b.c.d`) to IPv4 so dual-stack sockets
/// and IPv4 sockets compare equal for the same client.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
	ip.to_canonical()
}

#[cfg(test)]
mod tests {
	use std::net::{Ipv4Addr, Ipv6Addr};

	use super::*;

	fn authorization() -> StreamAuthorization {
		StreamAuthorization::new(7, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))).unwrap()
	}

	fn from(ip: &str, port: u16) -> SocketAddr {
		SocketAddr::new(ip.parse().unwrap(), port)
	}

	fn session_ping(payload: &str, counter: u32) -> Vec<u8> {
		let mut ping = payload.as_bytes().to_vec();
		ping.extend(counter.to_be_bytes());
		ping
	}

	#[test]
	fn identifiers_are_moonlight_compatible_and_distinct() {
		let auth = authorization();
		for stream in [MediaStream::Audio, MediaStream::Video] {
			let payload = auth.ping_payload(stream);
			assert_eq!(payload.len(), PING_PAYLOAD_LENGTH);
			assert!(payload.bytes().all(|b| b.is_ascii_alphanumeric()));
		}
		assert_ne!(
			auth.ping_payload(MediaStream::Audio),
			auth.ping_payload(MediaStream::Video)
		);
		assert_ne!(auth.control_connect_data(), 0);
		let next = StreamAuthorization::new(8, auth.client_ip()).unwrap();
		assert_ne!(
			next.ping_payload(MediaStream::Video),
			auth.ping_payload(MediaStream::Video)
		);
	}

	#[test]
	fn mapped_ipv6_is_the_same_client() {
		let auth = authorization();
		let mapped = IpAddr::V6(Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped());
		assert!(auth.admits_peer(mapped));
		assert!(!auth.admits_peer(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 21))));
		assert!(!auth.admits_peer(IpAddr::V6(Ipv6Addr::LOCALHOST)));

		let v6 = StreamAuthorization::new(1, "fd00::20".parse().unwrap()).unwrap();
		assert!(v6.admits_peer("fd00::20".parse().unwrap()));
		assert!(!v6.admits_peer("fd00::21".parse().unwrap()));
	}

	#[test]
	fn session_pings_require_the_stream_payload_and_client_address() {
		let auth = authorization();
		let video = session_ping(auth.ping_payload(MediaStream::Video), 1);
		let audio = session_ping(auth.ping_payload(MediaStream::Audio), 1);
		// Source ports are dynamic (client sockets, NAT rebinding).
		for port in [1, 40_000, 65_535] {
			assert!(auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", port), &video));
			assert!(auth.admits_media_ping(MediaStream::Video, from("::ffff:192.168.1.20", port), &video));
		}
		assert!(auth.admits_media_ping(MediaStream::Audio, from("192.168.1.20", 9), &audio));
		// Wrong stream, address, payload or framing.
		assert!(!auth.admits_media_ping(MediaStream::Audio, from("192.168.1.20", 9), &video));
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.99", 9), &video));
		let mut forged = video.clone();
		forged[0] ^= 1;
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", 9), &forged));
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", 9), &video[..19]));
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", 9), b""));
	}

	#[test]
	fn legacy_ping_is_ip_bound_and_disabled_for_session_id_clients() {
		let mut auth = authorization();
		assert!(auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", 5), b"PING"));
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.30", 5), b"PING"));
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", 5), b"PONG"));
		auth.require_session_id();
		assert!(!auth.admits_media_ping(MediaStream::Video, from("192.168.1.20", 5), b"PING"));
	}

	#[test]
	fn stale_generation_payload_is_rejected() {
		let old = authorization();
		let new = StreamAuthorization::new(old.generation() + 1, old.client_ip()).unwrap();
		let stale = session_ping(old.ping_payload(MediaStream::Video), 99);
		assert!(!new.admits_media_ping(MediaStream::Video, from("192.168.1.20", 5), &stale));
	}

	#[test]
	fn control_connect_requires_address_and_connect_data() {
		let mut auth = authorization();
		let data = auth.control_connect_data();
		assert!(auth.admits_control_connect(from("192.168.1.20", 1), data));
		assert!(auth.admits_control_connect(from("192.168.1.20", 1), 0));
		assert!(!auth.admits_control_connect(from("192.168.1.20", 1), data.wrapping_add(1)));
		assert!(!auth.admits_control_connect(from("192.168.1.21", 1), data));
		auth.require_session_id();
		assert!(auth.admits_control_connect(from("192.168.1.20", 1), data));
		assert!(!auth.admits_control_connect(from("192.168.1.20", 1), 0));
	}
}
