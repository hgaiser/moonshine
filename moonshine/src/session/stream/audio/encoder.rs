use cpal::StreamConfig;
use openssl::cipher::Cipher;
use tokio::sync::mpsc;

use crate::{session::{stream::RtpHeader, SessionKeys}, crypto::encrypt};

struct AudioFecHeader {
	pub shard_index: u8,
	pub payload_type: u8,
	pub base_sequence_number: u16,
	pub base_timestamp: u32,
	pub ssrc: u32,
}

impl AudioFecHeader {
	fn serialize(&self, buffer: &mut Vec<u8>) {
		buffer.extend(self.shard_index.to_be_bytes());
		buffer.extend(self.payload_type.to_be_bytes());
		buffer.extend(self.base_sequence_number.to_be_bytes());
		buffer.extend(self.base_timestamp.to_be_bytes());
		buffer.extend(self.ssrc.to_be_bytes());
	}
}

enum AudioEncoderCommand {
	UpdateKeys(SessionKeys),
}

pub struct AudioEncoder {
	command_tx: mpsc::Sender<AudioEncoderCommand>,
}

impl AudioEncoder {
	pub fn new(
		config: StreamConfig,
		audio_rx: mpsc::Receiver<Vec<i16>>,
		keys: SessionKeys,
		packet_tx: mpsc::Sender<Vec<u8>>
	) -> Result<Self, ()> {
		log::debug!("Creating audio encoder with the following settings: {:?}", config);
		let mut encoder = opus::Encoder::new(
			config.sample_rate.0,
			if config.channels > 1 { opus::Channels::Stereo } else { opus::Channels::Mono },
			opus::Application::LowDelay,
		)
			.map_err(|e| log::error!("Failed to create audio encoder: {e}"))?;

		// Moonlight expects a constant bitrate.
		encoder.set_vbr(false)
			.map_err(|e| log::error!("Failed to disable variable bitrate: {e}"))?;

		let (command_tx, command_rx) = mpsc::channel(10);
		let inner = AudioEncoderInner { };
		tokio::spawn(inner.run(command_rx, audio_rx, encoder, keys, packet_tx));

		Ok(Self { command_tx })
	}

	pub async fn update_keys(&self, keys: SessionKeys) -> Result<(), ()> {
		self.command_tx.send(AudioEncoderCommand::UpdateKeys(keys)).await
			.map_err(|e| log::error!("Failed to send UpdateKeys command: {e}"))
	}
}

struct AudioEncoderInner {
}

impl AudioEncoderInner {
	async fn run(
		self,
		mut command_rx: mpsc::Receiver<AudioEncoderCommand>,
		mut audio_rx: mpsc::Receiver<Vec<i16>>,
		mut encoder: opus::Encoder,
		mut keys: SessionKeys,
		packet_tx: mpsc::Sender<Vec<u8>>,
	) -> Result<(), ()> {
		let mut sequence_number = 0u16;
		let stream_start_time = std::time::Instant::now();

		const NR_DATA_SHARDS: usize = 4;
		const NR_PARITY_SHARDS: usize = 2;
		const NR_TOTAL_SHARDS: usize = NR_DATA_SHARDS + NR_PARITY_SHARDS;
		const MAX_BLOCK_SIZE: usize = ((2048 + 15) / 16) * 16; // Where does this come from?
		let fec_encoder = reed_solomon::ReedSolomon::new(NR_DATA_SHARDS, NR_PARITY_SHARDS)
			.map_err(|e| log::error!("Failed to create FEC encoder: {e}"))?;

		let mut shards: [Vec<u8>; NR_TOTAL_SHARDS] = Default::default();
		for shard in shards.iter_mut() {
			shard.extend(std::iter::repeat(0).take(MAX_BLOCK_SIZE));
		}

		let mut fec_header = AudioFecHeader {
			shard_index: 0u8,
			payload_type: 97,
			base_sequence_number: 0u16,
			base_timestamp: 0,
			ssrc: 0,
		};

		loop {
			tokio::select! {
				command = command_rx.recv() => {
					let Some(command) = command else {
						break;
					};

					match command {
						AudioEncoderCommand::UpdateKeys(new_keys) => {
							log::debug!("Updating session keys.");
							keys = new_keys;
						}
					}
				},

				audio_fragment = audio_rx.recv() => {
					let Some(audio_fragment) = audio_fragment else {
						break;
					};

					let timestamp = ((std::time::Instant::now() - stream_start_time).as_micros() / (1000 / 90)) as u32;
					let encoded = match encoder.encode_vec(&audio_fragment, 1_000_000) {
						Ok(encoded) => encoded,
						Err(e) => {
							log::warn!("Failed to encode audio payload: {e}");
							continue;
						}
					};

					let iv = keys.remote_input_key_id as u32 + sequence_number as u32;
					let mut iv = iv.to_be_bytes().to_vec();
					iv.extend([0u8; 12]);
					let payload = match encrypt(Cipher::aes_128_cbc(), &encoded, Some(&keys.remote_input_key), Some(&iv), true) {
						Ok(payload) => payload,
						Err(e) => {
							log::error!("Failed to encrypt audio: {e}");
							continue;
						},
					};

					let rtp_header = RtpHeader {
						header: 0x80, // What is this?
						packet_type: 97, // RTP_PAYLOAD_TYPE_AUDIO
						sequence_number,
						timestamp,
						ssrc: 0,
					};
					sequence_number += 1;

					let mut buffer = Vec::with_capacity(
						std::mem::size_of::<RtpHeader>()
						+ payload.len()
					);
					rtp_header.serialize(&mut buffer);
					buffer.extend(payload.clone());

					// log::debug!("Sending audio packet of {} bytes.", buffer.len());
					if packet_tx.send(buffer).await.is_err() {
						log::debug!("Failed to send packet over channel, channel is likely closed.");
						return Ok::<(), ()>(());
					}

					// For FEC, copy the sequence number and timestamp of the first of the sequence of audio packets.
					if (sequence_number - 1) as usize % NR_DATA_SHARDS == 0 {
						fec_header.base_sequence_number = sequence_number - 1;
						fec_header.base_timestamp = timestamp;
					}

					// Copy the audio into the list of (data) shards.
					shards[(sequence_number - 1) as usize % NR_DATA_SHARDS][..payload.len()].copy_from_slice(&payload);

					// If the last packet, compute and send parity shards.
					if sequence_number as usize % NR_DATA_SHARDS == 0 {
						if let Err(e) = fec_encoder.encode_fixed_length(&mut shards, payload.len()) {
							log::error!("Failed to create FEC block for audio: {e}");
							continue;
						}

						for shard_index in 0u8..NR_PARITY_SHARDS as u8 {
							let rtp_header = RtpHeader {
								header: 0x80,
								packet_type: 127,
								sequence_number: sequence_number + shard_index as u16,
								timestamp: 0,
								ssrc: 0,
							};

							fec_header.shard_index = shard_index;

							let shard_size = std::mem::size_of::<RtpHeader>()
								+ std::mem::size_of::<AudioFecHeader>()
								+ payload.len();
							let mut buffer = Vec::with_capacity(shard_size);
							rtp_header.serialize(&mut buffer);
							fec_header.serialize(&mut buffer);
							buffer.extend(&shards[(NR_DATA_SHARDS as u8 + shard_index) as usize][..payload.len()]);

							// log::debug!("Sending audio FEC packet of {} bytes.", buffer.len());
							if packet_tx.send(buffer).await.is_err() {
								log::debug!("Failed to send packet over channel, channel is likely closed.");
								return Ok::<(), ()>(());
							}
						}
					}
				}
			}
		}

		log::debug!("Audio capture channel closed.");
		Ok(())
	}
}
