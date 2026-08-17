use strum_macros::FromRepr;

const POINTER_PAYLOAD_SIZE: usize = 28;

#[derive(Debug, Clone, Copy, Eq, PartialEq, FromRepr)]
#[repr(u8)]
pub(crate) enum PointerEventKind {
	Hover = 0x00,
	Down = 0x01,
	Up = 0x02,
	Move = 0x03,
	Cancel = 0x04,
	ButtonOnly = 0x05,
	HoverLeave = 0x06,
	CancelAll = 0x07,
}

impl PointerEventKind {
	fn has_position(self) -> bool {
		matches!(self, Self::Hover | Self::Down | Self::Up | Self::Move)
	}
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, FromRepr)]
#[repr(u8)]
pub(crate) enum PenToolKind {
	Unknown = 0x00,
	Pen = 0x01,
	Eraser = 0x02,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Touch {
	pub event_kind: PointerEventKind,
	pub pointer_id: u32,
	pub x: f32,
	pub y: f32,
}

impl Touch {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < POINTER_PAYLOAD_SIZE {
			tracing::warn!(
				"Expected at least {POINTER_PAYLOAD_SIZE} bytes for Touch, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		let event_kind = PointerEventKind::from_repr(buffer[0])
			.ok_or_else(|| tracing::warn!(event_type = buffer[0], "Unknown touch event type"))?;
		let x = f32::from_le_bytes(buffer[8..12].try_into().unwrap());
		let y = f32::from_le_bytes(buffer[12..16].try_into().unwrap());

		if event_kind.has_position() && (!x.is_finite() || !y.is_finite()) {
			tracing::warn!(?x, ?y, "Ignoring touch event with non-finite coordinates");
			return Err(());
		}

		Ok(Self {
			event_kind,
			pointer_id: u32::from_le_bytes(buffer[4..8].try_into().unwrap()),
			x,
			y,
		})
	}
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Pen {
	pub event_kind: PointerEventKind,
	pub tool_kind: PenToolKind,
	pub buttons: u8,
	pub x: f32,
	pub y: f32,
	pub pressure_or_distance: f32,
	pub rotation: u16,
	pub tilt: u8,
}

impl Pen {
	pub fn from_bytes(buffer: &[u8]) -> Result<Self, ()> {
		if buffer.len() < POINTER_PAYLOAD_SIZE {
			tracing::warn!(
				"Expected at least {POINTER_PAYLOAD_SIZE} bytes for Pen, got {} bytes.",
				buffer.len()
			);
			return Err(());
		}

		let event_kind = PointerEventKind::from_repr(buffer[0])
			.ok_or_else(|| tracing::warn!(event_type = buffer[0], "Unknown pen event type"))?;
		let tool_kind = PenToolKind::from_repr(buffer[1])
			.ok_or_else(|| tracing::warn!(tool_type = buffer[1], "Unknown pen tool type"))?;
		let x = f32::from_le_bytes(buffer[4..8].try_into().unwrap());
		let y = f32::from_le_bytes(buffer[8..12].try_into().unwrap());
		let pressure_or_distance = f32::from_le_bytes(buffer[12..16].try_into().unwrap());

		if event_kind.has_position() && (!x.is_finite() || !y.is_finite() || !pressure_or_distance.is_finite()) {
			tracing::warn!(
				?x,
				?y,
				?pressure_or_distance,
				"Ignoring pen event with non-finite values"
			);
			return Err(());
		}

		Ok(Self {
			event_kind,
			tool_kind,
			buttons: buffer[2],
			x,
			y,
			pressure_or_distance,
			rotation: u16::from_le_bytes(buffer[16..18].try_into().unwrap()),
			tilt: buffer[18],
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{Pen, PenToolKind, PointerEventKind, Touch};

	#[test]
	fn parses_touch_payload() {
		let mut payload = [0_u8; 28];
		payload[0] = PointerEventKind::Down as u8;
		payload[4..8].copy_from_slice(&42_u32.to_le_bytes());
		payload[8..12].copy_from_slice(&0.25_f32.to_le_bytes());
		payload[12..16].copy_from_slice(&0.75_f32.to_le_bytes());

		let touch = Touch::from_bytes(&payload).unwrap();
		assert_eq!(touch.event_kind, PointerEventKind::Down);
		assert_eq!(touch.pointer_id, 42);
		assert_eq!(touch.x, 0.25);
		assert_eq!(touch.y, 0.75);
	}

	#[test]
	fn parses_pen_payload() {
		let mut payload = [0_u8; 28];
		payload[0] = PointerEventKind::Move as u8;
		payload[1] = PenToolKind::Pen as u8;
		payload[2] = 0x02;
		payload[4..8].copy_from_slice(&0.25_f32.to_le_bytes());
		payload[8..12].copy_from_slice(&0.75_f32.to_le_bytes());
		payload[12..16].copy_from_slice(&0.6_f32.to_le_bytes());
		payload[16..18].copy_from_slice(&270_u16.to_le_bytes());
		payload[18] = 35;

		let pen = Pen::from_bytes(&payload).unwrap();
		assert_eq!(pen.event_kind, PointerEventKind::Move);
		assert_eq!(pen.tool_kind, PenToolKind::Pen);
		assert_eq!(pen.buttons, 0x02);
		assert_eq!(pen.x, 0.25);
		assert_eq!(pen.y, 0.75);
		assert_eq!(pen.pressure_or_distance, 0.6);
		assert_eq!(pen.rotation, 270);
		assert_eq!(pen.tilt, 35);
	}

	#[test]
	fn rejects_invalid_pointer_payloads() {
		assert!(Touch::from_bytes(&[0; 27]).is_err());

		let mut touch = [0_u8; 28];
		touch[0] = PointerEventKind::Move as u8;
		touch[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
		assert!(Touch::from_bytes(&touch).is_err());

		let mut pen = [0_u8; 28];
		pen[0] = PointerEventKind::Move as u8;
		pen[1] = PenToolKind::Pen as u8;
		pen[12..16].copy_from_slice(&f32::INFINITY.to_le_bytes());
		assert!(Pen::from_bytes(&pen).is_err());
	}
}
