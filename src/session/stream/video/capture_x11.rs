use std::{fs::File, os::fd::FromRawFd, sync::{Arc, Mutex}};

use ffmpeg::Frame;
use memmap::{MmapOptions, Mmap};
use xcb::{shm, x};

pub struct FrameCapturer {
	/// A copy of the GetImage request.
	/// This request is sent to the X server whenever we need a new image.
	get_image_request: shm::GetImage,

	/// The connection to the X server.
	connection: xcb::Connection,

	/// The context used for scaling frames (and converting from BGRA to YUV420P).
	scale_context: ffmpeg::SwsContext,

	/// The buffer that is shared with the X server.
	/// The data written is in BGRA format.
	shared_buffer: Mmap,

	/// The width of the screen.
	pub screen_width: u16,

	/// The height of the screen.
	pub screen_height: u16,
}

impl FrameCapturer {
	pub fn new(
		output_width: u32,
		output_height: u32,
	) -> Result<Self, ()> {
		let (connection, screen_num) = xcb::Connection::connect(None).unwrap();
		let setup = connection.get_setup();
		let screen = setup.roots().nth(screen_num as usize).unwrap();

		let screen_width = screen.width_in_pixels();
		let screen_height = screen.height_in_pixels();

		let shared_memory_segment = connection.generate_id();
		let cookie = connection.send_request(&shm::CreateSegment {
			shmseg: shared_memory_segment,
			size: screen_width as u32 * screen_height as u32 * 4,
			read_only: false,
		});
		let segment = connection.wait_for_reply(cookie).unwrap();

		let shared_file = unsafe { File::from_raw_fd(segment.shm_fd()) };
		let shared_buffer = unsafe { MmapOptions::new().map(&shared_file).unwrap() };

		let get_image_request = shm::GetImage {
			format: x::ImageFormat::ZPixmap as u8,
			drawable: x::Drawable::Window(screen.root()),
			x: 0,
			y: 0,
			width: screen_width,
			height: screen_height,
			plane_mask: u32::MAX,
			shmseg: shared_memory_segment,
			offset: 0,
		};

		let scale_context = ffmpeg::SwsContext::new(
			(screen_width as u32, screen_height as u32), ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_BGRA,
			(output_width, output_height), ffmpeg_sys::AVPixelFormat_AV_PIX_FMT_YUV420P,
			ffmpeg_sys::SWS_FAST_BILINEAR,
		);

		Ok(Self {
			get_image_request,
			scale_context,
			connection,
			shared_buffer,
			screen_width,
			screen_height,
		})
	}

	pub async fn run(
		self,
		framerate: u32,
		mut capture_buffer: Frame,
		intermediate_buffer: Arc<Mutex<Frame>>,
		notifier: Arc<tokio::sync::Notify>,
	) -> Result<(), ()> {
		let frame_time = std::time::Duration::from_millis(1000u64 / framerate as u64);
		log::info!("Time between frames is {}ms.", frame_time.as_millis());
		loop {
			let deadline = tokio::time::Instant::now() + frame_time;
			let cookie = self.connection.send_request(&self.get_image_request);
			self.connection.wait_for_reply(cookie)
				.map_err(|e| log::error!("Failed to retrieve frame from X server: {e}"))?;

			// Scale the frame to the desired output size and convert from BGRA to YUV420P.
			self.scale_context.scale(
				[self.shared_buffer.as_ptr()].as_ptr(),
				&[self.screen_width as i32 * 4],
				self.screen_height as i32,
				capture_buffer.as_raw_mut().data.as_mut_ptr(),
				capture_buffer.as_raw().linesize.as_slice(),
			);

			// Swap the intermediate buffer with the output buffer and signal that we have a new frame.
			// Note that the lock is only held while swapping buffers, to minimize wait time for others locking the buffer.
			{
				let mut lock = intermediate_buffer.lock()
					.map_err(|e| log::error!("Failed to lock intermediate buffer: {e}"))?;
				std::mem::swap(&mut *lock, &mut capture_buffer);
			}
			notifier.notify_one();

			// Sleep to approximately get the desired framerate.
			tokio::time::sleep_until(deadline).await;
		}
	}
}
