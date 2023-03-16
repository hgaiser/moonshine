use enet::{
	Address,
	BandwidthLimit,
	ChannelLimit,
	Enet,
	Event,
	Host,
};

pub(super) struct ControlStream {
	host: Host<()>,
}

impl ControlStream {
	pub(super) fn new(address: &str, port: u16) -> Result<Self, ()> {
		let enet = Enet::new()
			.map_err(|e| log::error!("Failed to initialize Enet session: {e}"))?;

		let local_addr = Address::new(
			address.parse()
				.map_err(|e| log::error!("Failed to parse address: {e}"))?,
			port,
		);
		let host = enet
			.create_host::<()>(
				Some(&local_addr),
				10,
				ChannelLimit::Maximum,
				BandwidthLimit::Unlimited,
				BandwidthLimit::Unlimited,
			)
			.unwrap();

		Ok(Self { host })
	}

	pub(super) fn run(mut self) -> Result<(), ()> {
		log::info!("Listening for control messages on {:?}", self.host.address());

		loop {
			match self.host.service(1000).unwrap() {
				Some(Event::Connect(_)) => {}, //println!("new connection!"),
				Some(Event::Disconnect(..)) => {}, //println!("disconnect!"),
				Some(Event::Receive {
					channel_id,
					ref packet,
					..
				}) => {
					// println!(
					// 	"got packet on channel {}, content: '{:?}'",
					// 	channel_id,
					// 	std::str::from_utf8(packet.data())
					// );
				}
				_ => (),
			}
		}
	}
}
