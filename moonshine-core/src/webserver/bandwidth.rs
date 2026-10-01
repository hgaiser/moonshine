//! Fixed, bounded HTTPS calibration response. Admission/authentication belongs to
//! Webserver; this body holds the global probe permit until completed or dropped.
use futures_util::stream;
use http_body_util::{BodyExt, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
	Response,
	body::{Bytes, Frame},
	header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE},
};
use std::{convert::Infallible, net::IpAddr, sync::OnceLock, time::Instant};
use tokio::sync::OwnedSemaphorePermit;

pub(super) const PROBE_BYTES: usize = 32 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
pub(super) type ResponseBody = UnsyncBoxBody<Bytes, Infallible>;

pub(super) fn response(permit: OwnedSemaphorePermit) -> Response<ResponseBody> {
	static CHUNK: OnceLock<Bytes> = OnceLock::new();
	let chunk = CHUNK.get_or_init(|| Bytes::from(vec![0x50; CHUNK_BYTES])).clone();
	let chunks = stream::unfold(
		(PROBE_BYTES, chunk, permit, Instant::now()),
		|(left, chunk, permit, start)| async move {
			if left == 0 || start.elapsed().as_secs() >= 15 {
				return None;
			}
			Some((
				Ok(Frame::data(chunk.clone())),
				(left - CHUNK_BYTES, chunk, permit, start),
			))
		},
	);
	Response::builder()
		.header(CONTENT_TYPE, "application/octet-stream")
		.header(CONTENT_LENGTH, PROBE_BYTES)
		.header(CACHE_CONTROL, "no-store")
		.body(StreamBody::new(chunks).boxed_unsync())
		.expect("fixed response headers")
}

/// Identify the interface owning the selected route's source, then require a
/// physical, active, full-duplex Ethernet device. Bridges/VPNs stay unknown.
pub(super) fn routed_link_mbps(local: IpAddr, peer: IpAddr) -> u64 {
	use network_interface::{NetworkInterface, NetworkInterfaceConfig};
	let Ok(socket) = std::net::UdpSocket::bind((if local.is_ipv4() { "0.0.0.0" } else { "::" }, 0)) else {
		return 0;
	};
	if socket.connect((peer, 9)).is_err() {
		return 0;
	}
	let Ok(source) = socket.local_addr() else {
		return 0;
	};
	let Ok(interfaces) = NetworkInterface::show() else {
		return 0;
	};
	let Some(interface) = interfaces
		.into_iter()
		.find(|i| i.addr.iter().any(|a| a.ip() == source.ip()))
	else {
		return 0;
	};
	physical_link_mbps(&std::path::Path::new("/sys/class/net").join(interface.name))
}

fn physical_link_mbps(path: &std::path::Path) -> u64 {
	let read = |name| {
		std::fs::read_to_string(path.join(name))
			.unwrap_or_default()
			.trim()
			.to_owned()
	};
	if !path.join("device").exists()
		|| path.join("wireless").exists()
		|| path.join("phy80211").exists()
		|| read("type") != "1"
		|| read("operstate") != "up"
		|| read("duplex") != "full"
	{
		return 0;
	}
	read("speed")
		.parse::<u64>()
		.ok()
		.filter(|s| *s > 0 && *s <= 1_000_000)
		.unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;
	#[tokio::test]
	async fn probe_exact_length_and_permit_lifetime() {
		let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
		let reply = response(sem.clone().try_acquire_owned().unwrap());
		assert_eq!(reply.headers()[CONTENT_LENGTH], PROBE_BYTES.to_string());
		assert!(sem.clone().try_acquire_owned().is_err());
		let data = reply.into_body().collect().await.unwrap().to_bytes();
		assert_eq!(data.len(), PROBE_BYTES);
		assert!(data.iter().all(|b| *b == 0x50));
		assert!(sem.clone().try_acquire_owned().is_ok());
		drop(response(sem.clone().try_acquire_owned().unwrap()));
		assert!(sem.try_acquire_owned().is_ok());
	}
	#[test]
	fn virtual_route_stays_unknown() {
		assert_eq!(
			routed_link_mbps("127.0.0.1".parse().unwrap(), "127.0.0.1".parse().unwrap()),
			0
		);
		assert_eq!(physical_link_mbps(std::path::Path::new("/missing-interface")), 0);
	}
}
