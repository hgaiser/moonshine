use std::{cell::RefCell, ops::{Deref, DerefMut}, rc::Rc};

use pulse::{channelmap, context::{self, introspect, Context, FlagSet as ContextFlagSet}, def::BufferAttr, mainloop::standard::{IterateResult, Mainloop}, operation::{self, Operation}, proplist::Proplist, sample::{self, Spec}, stream::{FlagSet as StreamFlagSet, PeekResult, State, Stream}};

#[derive(Debug)]
pub struct ServerInfo {
	/// User name of the daemon process.
	pub user_name: Option<String>,
	/// Host name the daemon is running on.
	pub host_name: Option<String>,
	/// Version string of the daemon.
	pub server_version: Option<String>,
	/// Server package name (usually “pulseaudio”).
	pub server_name: Option<String>,
	/// Default sample specification.
	pub sample_spec: sample::Spec,
	/// Name of default sink.
	pub default_sink_name: Option<String>,
	/// Name of default source.
	pub default_source_name: Option<String>,
	/// A random cookie for identifying this instance of PulseAudio.
	pub cookie: u32,
	/// Default channel map.
	pub channel_map: channelmap::Map,
}

impl<'a> From<&'a introspect::ServerInfo<'a>> for ServerInfo {
	fn from(info: &'a introspect::ServerInfo<'a>) -> Self {
		ServerInfo {
			user_name: info.user_name.as_ref().map(|cow| cow.to_string()),
			host_name: info.host_name.as_ref().map(|cow| cow.to_string()),
			server_version: info.server_version.as_ref().map(|cow| cow.to_string()),
			server_name: info.server_name.as_ref().map(|cow| cow.to_string()),
			sample_spec: info.sample_spec,
			default_sink_name: info.default_sink_name.as_ref().map(|cow| cow.to_string()),
			default_source_name: info.default_source_name.as_ref().map(|cow| cow.to_string()),
			cookie: info.cookie,
			channel_map: info.channel_map,
		}
	}
}

fn iterate(mainloop: &mut Mainloop) -> Result<(), ()> {
	match mainloop.iterate(false) {
		IterateResult::Quit(_) | IterateResult::Err(_) => {
			log::error!("Failed to run pulseaudio main loop.");
			Err(())
		},
		IterateResult::Success(_) => Ok(())
	}
}

pub struct PulseAudio {
	mainloop: Rc<RefCell<Mainloop>>,
	context: Rc<RefCell<Context>>,
	stream: Option<Rc<RefCell<Stream>>>,
}

impl PulseAudio {
	pub fn new(name: &str) -> Result<Self, ()> {
		// Create a new PulseAudio context.
		let mainloop = Rc::new(RefCell::new(Mainloop::new()
			.ok_or_else(|| log::error!("Failed to create pulseaudio client."))?));

		let mut proplist = Proplist::new()
			.ok_or_else(|| log::error!("Failed to create pulseaudio proplist."))?;
		proplist.set_str(pulse::proplist::properties::APPLICATION_NAME, name)
			.map_err(|()| log::error!("Failed to set pulseaudio application name."))?;
		let context = Rc::new(RefCell::new(
			Context::new_with_proplist(mainloop.borrow().deref(), "Moonshine context", &proplist)
				.ok_or_else(|| log::error!("Failed to create pulseaudio context."))?
		));

		context.borrow_mut().connect(None, ContextFlagSet::NOFLAGS, None)
			.map_err(|e| log::error!("Failed to connect to pulseaudio server: {e}"))?;

		// Wait for context to be ready.
		loop {
			iterate(mainloop.borrow_mut().deref_mut())?;

			match context.borrow().get_state() {
				context::State::Unconnected
				| context::State::Connecting
				| context::State::Authorizing
				| context::State::SettingName => {}
				context::State::Failed | context::State::Terminated => {
					log::error!("Failed to run context.");
					return Err(());
				}
				context::State::Ready => break
			}
		}

		Ok(Self { mainloop, context, stream: None })
	}

	fn wait_for_operation<G: ?Sized>(&self, op: Operation<G>) -> Result<(), ()> {
		loop {
			iterate(self.mainloop.borrow_mut().deref_mut())?;

			match op.get_state() {
				operation::State::Done => break,
				operation::State::Running => {},
				operation::State::Cancelled => {
					log::error!("Operation cancelled unexpectedly.");
					return Err(());
				}
			}
		}
		Ok(())
	}

	pub fn start_recording(&mut self, source_name: &str, spec: Spec) -> Result<(), ()> {
		let stream = Rc::new(RefCell::new(Stream::new(
			&mut self.context.borrow_mut(),
			"Moonshine",
			&spec,
			None
		).ok_or_else(|| log::error!("Failed to create new stream."))?));

		stream.borrow_mut().connect_record(
			Some(source_name),
			Some(&BufferAttr {
				maxlength: std::mem::size_of::<i16>() as u32 * spec.rate * spec.channels as u32 * 5 / 1000,
				tlength: std::u32::MAX,
				prebuf: std::u32::MAX,
				minreq: std::u32::MAX,
				fragsize: std::u32::MAX,
			}),
			StreamFlagSet::START_CORKED
		).map_err(|e| log::error!("Failed to connect record device: {e}"))?;

		// Wait for stream to be ready
		loop {
			iterate(self.mainloop.borrow_mut().deref_mut())?;

			match stream.borrow().get_state() {
				State::Ready => break,
				State::Failed | State::Terminated => {
					log::error!("Failed to start stream.");
					return Err(());
				},
				_ => {},
			}
		}

		self.stream = Some(stream);

		Ok(())
	}

	pub fn get_server_info(&self) -> Result<ServerInfo, ()> {
		let server_info = Rc::new(RefCell::new(Some(None)));
		let server_info_ref = server_info.clone();

		let op = self.context.borrow().introspect().get_server_info(move |res| {
			server_info_ref
				.borrow_mut()
				.as_mut()
				.unwrap_or(&mut None)
				.replace(res.into());
		});
		self.wait_for_operation(op)?;
		let mut result = server_info.borrow_mut();
		result
			.take()
			.ok_or_else(|| log::error!("Failed to get server info."))?
			.ok_or_else(|| log::error!("Failed to get server info."))
	}

	pub fn read(&mut self) -> Result<Vec<u8>, ()> {
		let stream = self.stream
			.as_ref()
			.ok_or_else(|| log::error!("Can't read from stream with no stream."))?;

		loop {
			iterate(self.mainloop.borrow_mut().deref_mut())?;

			if stream.borrow().is_corked().map_err(|e| log::error!("Failed to check stream corked status: {e}"))? {
				self.wait_for_operation(stream.borrow_mut().uncork(None))?;
			}

			let fragment = stream.borrow_mut().peek()
				.map_err(|e| log::error!("Failed to record next fragment: {e}"))?;

			match fragment {
				PeekResult::Empty => {
					// log::debug!("Empty stream.");
				},
				PeekResult::Hole(_) => {
					log::debug!("Hole in stream.");
					stream.borrow_mut().discard()
						.map_err(|e| log::error!("Failed to discard fragment from recording stream: {e}"))?;
					continue;
				},
				PeekResult::Data(buffer) => {
					stream.borrow_mut().discard()
						.map_err(|e| log::error!("Failed to advance audio buffer: {e}"))?;

					return Ok(buffer.to_vec());
				},
			}
		}
	}
}

impl Drop for PulseAudio {
    fn drop(&mut self) {
        self.context.borrow_mut().disconnect();
        self.mainloop.borrow_mut().quit(pulse::def::Retval(0));
		self.stream.as_ref().map(|stream| stream.borrow_mut().disconnect());
    }
}
