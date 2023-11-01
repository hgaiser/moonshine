use cpal::StreamConfig;
use openssl::cipher::Cipher;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::{session::rtsp::stream::RtpHeader, crypto::encrypt};

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

pub struct AudioEncoder {
}

impl AudioEncoder {
	pub fn new(
		config: StreamConfig,
		mut audio_rx: Receiver<Vec<i16>>,
		remote_input_key: Vec<u8>,
		remote_input_key_id: i64,
		packet_tx: Sender<Vec<u8>>
	) -> Result<Self, ()> {
		log::debug!("Creating audio encoder with the following settings: {:?}", config);
		let mut encoder = opus::Encoder::new(
			config.sample_rate.0,
			if config.channels > 1 { opus::Channels::Stereo } else { opus::Channels::Mono },
			opus::Application::LowDelay,
		)
			.map_err(|e| log::error!("Failed to create audio encoder: {e}"))?;

		tokio::spawn(async move {
			let mut sequence_number = 0usize;
			let stream_start_time = std::time::Instant::now();

			const NR_DATA_SHARDS: usize = 4;
			const NR_PARITY_SHARDS: usize = 2;
			const NR_TOTAL_SHARDS: usize = NR_DATA_SHARDS + NR_PARITY_SHARDS;
			const BLOCK_SIZE: usize = ((2048 + 15) / 16) * 16;
			let fec_encoder = reed_solomon::ReedSolomon::new(NR_DATA_SHARDS, NR_PARITY_SHARDS)
				.map_err(|e| log::error!("Failed to create FEC encoder: {e}"))?;

			let mut shards: [Vec<u8>; NR_TOTAL_SHARDS] = Default::default();
			for shard in shards.iter_mut() {
				shard.extend(std::iter::repeat(0).take(BLOCK_SIZE));
			}

			let mut fec_header = AudioFecHeader {
				shard_index: 0u8,
				payload_type: 97,
				base_sequence_number: (sequence_number - NR_DATA_SHARDS) as u16,
				base_timestamp: 0,
				ssrc: 0,
			};

			while let Some(audio_fragment) = audio_rx.recv().await {
				let timestamp = ((std::time::Instant::now() - stream_start_time).as_micros() / (1000 / 90)) as u32;
				let encoded = match encoder.encode_vec(&audio_fragment, 1_000_000) {
					Ok(encoded) => encoded,
					Err(e) => {
						log::warn!("Failed to encode audio payload: {e}");
						continue;
					}
				};

				let iv = remote_input_key_id as u32 + sequence_number as u32;
				let mut iv = iv.to_be_bytes().to_vec();
				iv.extend([0u8; 12]);
				let payload = match encrypt(Cipher::aes_128_cbc(), &encoded, Some(&remote_input_key), Some(&iv), true) {
					Ok(payload) => payload,
					Err(e) => {
						log::error!("Failed to encrypt audio: {e}");
						continue;
					},
				};

				let rtp_header = RtpHeader {
					header: 0x80, // What is this?
					packet_type: 97, // RTP_PAYLOAD_TYPE_AUDIO
					sequence_number: sequence_number as u16,
					timestamp: 0,
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
				if (sequence_number - 1) % NR_DATA_SHARDS == 0 {
					fec_header.base_sequence_number = (sequence_number - 1) as u16;
					fec_header.base_timestamp = timestamp;
				}

				// Copy the audio into the list of (data) shards.
				shards[(sequence_number - 1) % NR_DATA_SHARDS][..payload.len()].copy_from_slice(&payload);

				if sequence_number % NR_DATA_SHARDS == 0 {
					if let Err(e) = fec_encoder.encode(&mut shards) {
						log::error!("Failed to create FEC block for audio: {e}");
						continue;
					}

					for shard_index in 0u8..NR_PARITY_SHARDS as u8 {
						let rtp_header = RtpHeader {
							header: 0x80,
							packet_type: 127,
							sequence_number: (sequence_number + shard_index as usize) as u16,
							timestamp,
							ssrc: 0,
						};

						fec_header.shard_index = shard_index;

						let mut buffer = Vec::with_capacity(
							std::mem::size_of::<RtpHeader>()
							+ std::mem::size_of::<AudioFecHeader>()
							+ payload.len()
						);
						rtp_header.serialize(&mut buffer);
						fec_header.serialize(&mut buffer);
						buffer.extend(&shards[(NR_DATA_SHARDS as u8 + shard_index) as usize]);

						// log::debug!("Sending audio FEC packet of {} bytes.", buffer.len());
						if packet_tx.send(buffer).await.is_err() {
							log::debug!("Failed to send packet over channel, channel is likely closed.");
							return Ok::<(), ()>(());
						}
					}
				}
			}

			log::debug!("Audio capture channel closed.");
			Ok(())
		});

		Ok(Self {  })
	}
}
