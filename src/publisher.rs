use zeroconf::prelude::*;

pub fn spawn(port: u16, name: String) {
	tokio::task::spawn_blocking(move || { run(port, name) });
}

fn run(port: u16, name: String) -> Result<(), ()> {
	let mut service = zeroconf::MdnsService::new(
		zeroconf::ServiceType::new("nvstream", "tcp")
			.map_err(|e| tracing::error!("Failed to publish: {e}"))?,
		port
	);

	service.set_registered_callback(Box::new(on_service_registered));
	service.set_name(&name);
	service.set_network_interface(zeroconf::NetworkInterface::Unspec);

	let event_loop = service.register()
		.map_err(|e| tracing::error!("Failed to register service: {e}"))?;

	loop {
		// Calling `poll()` will keep this service alive.
		if let Err(e) = event_loop.poll(std::time::Duration::from_secs(0)) {
			tracing::warn!("Failed to publish service: {e}");
		}
		std::thread::sleep(std::time::Duration::from_secs(1));
	}
}

fn on_service_registered(
	result: zeroconf::Result<zeroconf::ServiceRegistration>,
	_context: Option<std::sync::Arc<dyn std::any::Any>>,
) {
	if let Err(e) = result {
		tracing::error!("Failed to register service: {e}");
	} else {
		tracing::debug!("Service successfully registered.");
	}
}

