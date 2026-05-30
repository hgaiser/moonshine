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
