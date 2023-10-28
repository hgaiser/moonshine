use async_shutdown::ShutdownManager;
use zeroconf::prelude::*;

pub fn spawn(port: u16, name: String, shutdown: ShutdownManager<i32>) {
	tokio::task::spawn_blocking(move || {
		if run(port, name, &shutdown).is_err() {
			shutdown.trigger_shutdown(1)
				.map_err(|e| log::error!("Failed to trigger shutdown: {e}")).ok();
		}
	});
}

fn run(port: u16, name: String, shutdown: &ShutdownManager<i32>) -> Result<(), ()> {
	let mut service = zeroconf::MdnsService::new(
		zeroconf::ServiceType::new("nvstream", "tcp")
			.map_err(|e| log::error!("Failed to publish: {e}"))?,
		port
	);

	service.set_registered_callback(Box::new(on_service_registered));
	service.set_name(&name);
	service.set_network_interface(zeroconf::NetworkInterface::Unspec);

	let event_loop = service.register()
		.map_err(|e| log::error!("Failed to register service: {e}"))?;

	while !shutdown.is_shutdown_triggered() {
		// Calling `poll()` will keep this service alive.
		event_loop.poll(std::time::Duration::from_secs(0)).unwrap();
		std::thread::sleep(std::time::Duration::from_secs(1));
	}

	Ok(())
}

fn on_service_registered(
	result: zeroconf::Result<zeroconf::ServiceRegistration>,
	_context: Option<std::sync::Arc<dyn std::any::Any>>,
) {
	if let Err(e) = result {
		log::error!("Failed to register service: {e}");
	} else {
		log::info!("Service successfully registered.");
	}
}

