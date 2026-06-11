use std::collections::HashMap;
use std::net::IpAddr;

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};
use network_interface::NetworkInterfaceConfig;

const SERVICE_TYPE: &str = "_nvstream._tcp.local.";

/// Advertises the Moonshine service over mDNS until dropped.
///
/// This embeds its own responder (no avahi-daemon required). It coexists with a
/// running avahi-daemon: both bind UDP 5353 with SO_REUSEPORT, and we advertise a
/// hostname distinct from the machine's `<hostname>.local` so avahi never sees
/// conflicting unique records (which would trigger its conflict resolution and a
/// `<hostname>-2.local` rename).
pub struct MdnsDiscovery {
	daemon: Option<ServiceDaemon>,
	fullname: String,
}

impl MdnsDiscovery {
	/// Start advertising. Errors are logged and otherwise ignored, as discovery is
	/// not critical: clients can still add the host manually.
	pub fn spawn(address: &str, port: u16, name: &str) -> Self {
		match register(address, port, name) {
			Ok((daemon, fullname)) => Self {
				daemon: Some(daemon),
				fullname,
			},
			Err(e) => {
				tracing::error!("Failed to advertise Moonshine service: {e}");
				Self {
					daemon: None,
					fullname: String::new(),
				}
			},
		}
	}
}

fn register(address: &str, port: u16, name: &str) -> Result<(ServiceDaemon, String), mdns_sd::Error> {
	let daemon = ServiceDaemon::new()?;

	let machine_name = gethostname::gethostname();
	let machine_name = machine_name.to_string_lossy();
	let machine_name = machine_name.split('.').next().unwrap_or("host");
	let hostname = format!("{machine_name}-moonshine.local.");

	let mode = advertise_mode(address);
	let mut service = match mode {
		Advertise::Fixed(ip) => ServiceInfo::new(
			SERVICE_TYPE,
			name,
			&hostname,
			ip,
			port,
			HashMap::<String, String>::new(),
		)?,
		_ => ServiceInfo::new(
			SERVICE_TYPE,
			name,
			&hostname,
			"",
			port,
			HashMap::<String, String>::new(),
		)?
		.enable_addr_auto(),
	};

	match mode {
		Advertise::All => service.set_interfaces(auto_service_interfaces(true)),
		Advertise::Ipv4Only => service.set_interfaces(auto_service_interfaces(false)),
		Advertise::Fixed(_) => {},
	}

	let fullname = service.get_fullname().to_string();
	daemon.register(service)?;
	tracing::debug!("Advertising service '{fullname}' with hostname '{hostname}'.");

	Ok((daemon, fullname))
}

fn auto_service_interfaces(include_ipv6: bool) -> Vec<IfKind> {
	let mut ipv4_indexes = Vec::new();
	let mut ipv6_indexes = Vec::new();

	match network_interface::NetworkInterface::show() {
		Ok(interfaces) => {
			for interface in interfaces {
				if interface.internal {
					continue;
				}

				for addr in interface.addr {
					match addr.ip() {
						IpAddr::V4(ip) if !ip.is_loopback() && !ipv4_indexes.contains(&interface.index) => {
							ipv4_indexes.push(interface.index);
						},
						IpAddr::V6(ip)
							if include_ipv6 && !ip.is_loopback() && !ipv6_indexes.contains(&interface.index) =>
						{
							ipv6_indexes.push(interface.index);
						},
						_ => {},
					}
				}
			}
		},
		Err(e) => tracing::warn!("Failed to retrieve network interfaces for mDNS advertisement: {e}"),
	}

	if ipv4_indexes.is_empty() && ipv6_indexes.is_empty() {
		let fallback = if include_ipv6 { IfKind::All } else { IfKind::IPv4 };
		tracing::warn!(
			"No non-loopback interface found for mDNS advertisement; falling back to {:?} interfaces.",
			fallback
		);
		return vec![fallback];
	}

	ipv4_indexes
		.into_iter()
		.map(IfKind::IndexV4)
		.chain(ipv6_indexes.into_iter().map(IfKind::IndexV6))
		.collect()
}

impl Drop for MdnsDiscovery {
	fn drop(&mut self) {
		if let Some(daemon) = self.daemon.take() {
			// Unregister first so goodbye packets are sent, then stop the daemon thread.
			if let Ok(receiver) = daemon.unregister(&self.fullname) {
				let _ = receiver.recv_timeout(std::time::Duration::from_secs(2));
			}
			let _ = daemon.shutdown();
		}
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
	Fixed(IpAddr),
}

fn advertise_mode(address: &str) -> Advertise {
	match address.parse::<IpAddr>() {
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
		assert_eq!(
			advertise_mode("192.168.1.5"),
			Advertise::Fixed("192.168.1.5".parse().unwrap())
		);
	}

	#[test]
	fn specific_ipv6_is_fixed() {
		assert_eq!(
			advertise_mode("fd12:3456::1"),
			Advertise::Fixed("fd12:3456::1".parse().unwrap())
		);
	}

	#[test]
	fn hostname_falls_back_to_all() {
		assert_eq!(advertise_mode("localhost"), Advertise::All);
	}
}
