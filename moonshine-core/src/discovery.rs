use async_shutdown::ShutdownManager;
use zeroconf::prelude::*;

use crate::ShutdownReason;

pub struct ZeroconfDiscovery {
	handle: Option<std::thread::JoinHandle<()>>,
}

impl ZeroconfDiscovery {
	pub fn spawn(port: u16, name: String, shutdown: ShutdownManager<ShutdownReason>) -> Self {
		let handle = std::thread::spawn(move || run(port, name, shutdown));
		Self { handle: Some(handle) }
	}
}

impl Drop for ZeroconfDiscovery {
	fn drop(&mut self) {
		// No timeout: the OS reclaims all thread resources on process exit anyway,
		// and a blocked join during shutdown would only delay SIGKILL escalation.
		if let Some(handle) = self.handle.take() {
			handle.join().ok();
		}
	}
}

/// Runs the mDNS publisher in a separate thread, advertising the service until shutdown is triggered.
///
/// Errors do not trigger a global shutdown as this service is not considered critical for the main functionality.
fn run(port: u16, name: String, shutdown: ShutdownManager<ShutdownReason>) {
	let service_type = match zeroconf::ServiceType::new("nvstream", "tcp") {
		Ok(service_type) => service_type,
		Err(e) => {
			tracing::error!("Failed to advertise Moonshine service: {e}");
			return;
		},
	};
	let mut service = zeroconf::MdnsService::new(service_type, port);

	service.set_registered_callback(Box::new(on_service_registered));
	service.set_name(&name);
	service.set_network_interface(zeroconf::NetworkInterface::Unspec);

	let event_loop = match service.register() {
		Ok(loop_) => loop_,
		Err(e) => {
			tracing::error!("Failed to register mDNS service: {e}");
			return;
		},
	};

	loop {
		if shutdown.is_shutdown_triggered() {
			tracing::debug!("Publisher received shutdown signal, stopping.");
			break;
		}

		if let Err(e) = event_loop.poll(std::time::Duration::from_secs(0)) {
			tracing::warn!("Failed to publish service: {e}");
		}
		std::thread::sleep(std::time::Duration::from_secs(1));
	}
}

fn on_service_registered(
	result: zeroconf::Result<zeroconf::ServiceRegistration>,
	_context: Option<std::sync::Arc<dyn std::any::Any + Send + Sync + 'static>>,
) {
	if let Err(e) = result {
		tracing::error!("Failed to register service: {e}");
	} else {
		tracing::debug!("Service successfully registered.");
	}
}

/// Which addresses to advertise over mDNS, derived from the configured bind address.
#[derive(Debug, PartialEq)]
enum Advertise {
	/// Advertise all host addresses (IPv4 and IPv6), tracking changes.
	All,
	/// Advertise all host IPv4 addresses, tracking changes.
	Ipv4Only,
	/// Advertise exactly this address.
	Fixed(std::net::IpAddr),
}

fn advertise_mode(address: &str) -> Advertise {
	match address.parse::<std::net::IpAddr>() {
		Ok(ip) if ip.is_unspecified() => {
			if ip.is_ipv4() {
				Advertise::Ipv4Only
			} else {
				Advertise::All
			}
		},
		Ok(ip) => Advertise::Fixed(ip),
		// The configured address is a hostname; we can't tell which addresses it
		// resolves to, so advertise everything.
		Err(_) => Advertise::All,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unspecified_ipv6_advertises_all() {
		assert_eq!(advertise_mode("::"), Advertise::All);
	}

	#[test]
	fn unspecified_ipv4_advertises_ipv4_only() {
		assert_eq!(advertise_mode("0.0.0.0"), Advertise::Ipv4Only);
	}

	#[test]
	fn specific_ipv4_is_fixed() {
		assert_eq!(advertise_mode("192.168.1.5"), Advertise::Fixed("192.168.1.5".parse().unwrap()));
	}

	#[test]
	fn specific_ipv6_is_fixed() {
		assert_eq!(advertise_mode("fd12:3456::1"), Advertise::Fixed("fd12:3456::1".parse().unwrap()));
	}

	#[test]
	fn hostname_falls_back_to_all() {
		assert_eq!(advertise_mode("localhost"), Advertise::All);
	}
}
