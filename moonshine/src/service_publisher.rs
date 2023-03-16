use zeroconf::prelude::*;

pub(crate) async fn run(port: u16) -> Result<(), ()> {
	let mut service = zeroconf::MdnsService::new(
		zeroconf::ServiceType::new(
			"nvstream",
			"tcp"
		).map_err(|e| log::error!("Failed to publish: {}", e))?,
		port
	);

	service.set_registered_callback(Box::new(on_service_registered));
	service.set_name("Moonshine");
	service.set_network_interface(zeroconf::NetworkInterface::AtIndex(1));

	let event_loop = service.register()
		.map_err(|e| log::error!("Failed to register service: {}", e))?;

	loop {
		// Calling `poll()` will keep this service alive.
		event_loop.poll(std::time::Duration::from_secs(0)).unwrap();
		std::thread::sleep(std::time::Duration::from_millis(1000));
	}
}

fn on_service_registered(
	result: zeroconf::Result<zeroconf::ServiceRegistration>,
	_context: Option<std::sync::Arc<dyn std::any::Any>>,
) {
	if let Err(e) = result {
		log::error!("Failed to register service: {}", e);
	} else {
		log::info!("Service successfully registered.");
	}
}

