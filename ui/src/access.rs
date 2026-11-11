//! Who can open the control panel, and from which page.
//!
//! The control panel can change the configuration and add applications, which run commands as the
//! user. So besides the address ranges in `[ui] allowed_networks`, it refuses requests that
//! another website could make through the user's browser:
//!
//! - The `Host` header must name this computer (an IP address, `localhost`, the host name or a
//!   name in `[ui] allowed_hosts`). This stops DNS rebinding, where a website's own name starts to
//!   point at this computer.
//! - A request that changes something must carry the `X-Moonshine-UI` header. Browsers do not let
//!   another website add that header without asking first, and the control panel never agrees.
//! - A WebSocket must come from a page whose `Origin` is the control panel itself.

use std::net::IpAddr;

/// An address range, such as `192.168.1.0/24`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
	address: IpAddr,
	prefix: u8,
}

impl Network {
	/// Parse `address/prefix`, or a single address.
	pub fn parse(text: &str) -> Result<Self, String> {
		let text = text.trim();
		let (address, prefix) = match text.split_once('/') {
			Some((address, prefix)) => (address, Some(prefix)),
			None => (text, None),
		};
		let address: IpAddr = address
			.parse()
			.map_err(|_| format!("'{text}' is not an address range. Use for example 192.168.1.0/24."))?;
		let max = if address.is_ipv4() { 32 } else { 128 };
		let prefix = match prefix {
			Some(prefix) => prefix
				.parse::<u8>()
				.ok()
				.filter(|prefix| *prefix <= max)
				.ok_or_else(|| format!("'{text}' has a prefix length that is not between 0 and {max}."))?,
			None => max,
		};
		Ok(Self {
			address: address.to_canonical(),
			prefix,
		})
	}

	pub fn contains(&self, address: IpAddr) -> bool {
		match (self.address, address.to_canonical()) {
			(IpAddr::V4(network), IpAddr::V4(address)) => {
				let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
				u32::from(network) & mask == u32::from(address) & mask
			},
			(IpAddr::V6(network), IpAddr::V6(address)) => {
				let mask = u128::MAX.checked_shl(128 - u32::from(self.prefix)).unwrap_or(0);
				u128::from(network) & mask == u128::from(address) & mask
			},
			_ => false,
		}
	}

	/// Whether the range holds loopback addresses only.
	pub fn is_loopback(&self) -> bool {
		self.address.is_loopback() && self.prefix >= if self.address.is_ipv4() { 8 } else { 128 }
	}
}

/// Strip the port from a `Host` header value.
fn host_without_port(host: &str) -> &str {
	if let Some(rest) = host.strip_prefix('[') {
		return rest.split(']').next().unwrap_or(rest);
	}
	match host.rsplit_once(':') {
		Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
		_ => host,
	}
}

/// Whether a `Host` header value names this computer.
pub(crate) fn host_allowed(host: &str, host_names: &[String]) -> bool {
	let name = host_without_port(host);
	name.parse::<IpAddr>().is_ok()
		|| name.eq_ignore_ascii_case("localhost")
		|| host_names.iter().any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Whether an `Origin` header value is the control panel: the page at `host`, or a page at one of
/// the configured names. A reverse proxy can change the `Host` header, so the `Origin` then
/// names the proxy instead.
pub(crate) fn origin_allowed(origin: &str, host: &str, host_names: &[String]) -> bool {
	let Some(rest) = origin
		.strip_prefix("http://")
		.or_else(|| origin.strip_prefix("https://"))
	else {
		return false;
	};
	rest.eq_ignore_ascii_case(host) || {
		let name = host_without_port(rest);
		name.parse::<IpAddr>().is_err()
			&& !name.eq_ignore_ascii_case("localhost")
			&& host_names.iter().any(|allowed| allowed.eq_ignore_ascii_case(name))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn networks_contain_their_addresses() {
		let lan = Network::parse("192.168.1.0/24").unwrap();
		assert!(lan.contains("192.168.1.42".parse().unwrap()));
		assert!(lan.contains("::ffff:192.168.1.42".parse().unwrap()));
		assert!(!lan.contains("192.168.2.1".parse().unwrap()));
		assert!(!lan.contains("::1".parse().unwrap()));

		let all = Network::parse("0.0.0.0/0").unwrap();
		assert!(all.contains("8.8.8.8".parse().unwrap()));

		let single = Network::parse("::1").unwrap();
		assert!(single.contains("::1".parse().unwrap()));
		assert!(single.is_loopback());
		assert!(Network::parse("127.0.0.1/32").unwrap().is_loopback());
		assert!(!Network::parse("10.0.0.0/8").unwrap().is_loopback());
	}

	#[test]
	fn bad_networks_are_refused() {
		assert!(Network::parse("192.168.1.0/33").is_err());
		assert!(Network::parse("example.com").is_err());
		assert!(Network::parse("").is_err());
	}

	#[test]
	fn hosts_must_name_this_computer() {
		let names = vec!["gaming-pc".to_string(), "gaming-pc.local".to_string()];
		assert!(host_allowed("127.0.0.1:47990", &names));
		assert!(host_allowed("[::1]:47990", &names));
		assert!(host_allowed("localhost:47990", &names));
		assert!(host_allowed("Gaming-PC.local:47990", &names));
		assert!(!host_allowed("attacker.example:47990", &names));
		assert!(!host_allowed("localhost.attacker.example", &names));
	}

	#[test]
	fn origins_must_match_the_host() {
		let names = vec!["moonshine.example.com".to_string()];
		assert!(origin_allowed("http://localhost:47990", "localhost:47990", &names));
		assert!(!origin_allowed("http://attacker.example", "localhost:47990", &names));
		assert!(!origin_allowed("null", "localhost:47990", &names));
		// A reverse proxy that sends its own address as the host.
		assert!(origin_allowed(
			"https://moonshine.example.com",
			"100.95.89.123:47990",
			&names
		));
		assert!(!origin_allowed("http://localhost:8080", "100.95.89.123:47990", &names));
	}

	#[test]
	fn configured_names_are_hosts() {
		let names = vec!["moonshine.example.com".to_string()];
		assert!(host_allowed("moonshine.example.com", &names));
		assert!(host_allowed("MOONSHINE.example.com:443", &names));
		assert!(!host_allowed("example.com", &names));
	}
}
