use anyhow::{Context, Result};
use async_shutdown::ShutdownManager;
use zeroconf::prelude::*;

pub fn spawn(port: u16, name: String, shutdown: ShutdownManager<i32>) {
	tokio::task::spawn_blocking(move || {
		if run(port, name, &shutdown).is_err() {
			shutdown
				.trigger_shutdown(1)
				.map_err(|e| tracing::error!("Failed to trigger shutdown: {e}"))
				.ok();
		}
	});
}

fn run(port: u16, name: String, shutdown: &ShutdownManager<i32>) -> Result<()> {
	let service_type = zeroconf::ServiceType::new("nvstream", "tcp").context("Failed to publish")?;
	let mut service = zeroconf::MdnsService::new(service_type, port);

	service.set_registered_callback(Box::new(on_service_registered));
	service.set_name(&name);
	service.set_network_interface(zeroconf::NetworkInterface::Unspec);

	let event_loop = service.register().context("Failed to register service")?;

	while !shutdown.is_shutdown_triggered() {
		// Calling `poll()` will keep this service alive.
		if let Err(e) = event_loop.poll(std::time::Duration::from_secs(0)) {
			tracing::warn!("Failed to publish service: {e}");
		}
		std::thread::sleep(std::time::Duration::from_secs(1));
	}

	Ok(())
}

fn on_service_registered(
	result: zeroconf::Result<zeroconf::ServiceRegistration>,
	_context: Option<std::sync::Arc<dyn std::any::Any>>,
) {
	if let Err(e) = result {
		tracing::error!("Failed to register service: {e}");
	} else {
		tracing::info!("Service successfully registered.");
	}
}
