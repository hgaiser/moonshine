#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

mod generated;
use std::ptr::null_mut;

pub use generated::*;

// Pixel formats

const fn av_pix_fmt_ne(be: AVPixelFormat, le: AVPixelFormat) -> AVPixelFormat {
	if AV_HAVE_BIGENDIAN != 0 {
		be
	} else {
		le
	}
}

pub const AV_PIX_FMT_RGB32: AVPixelFormat   = av_pix_fmt_ne(AVPixelFormat_AV_PIX_FMT_ARGB, AVPixelFormat_AV_PIX_FMT_BGRA);
pub const AV_PIX_FMT_RGB32_1: AVPixelFormat = av_pix_fmt_ne(AVPixelFormat_AV_PIX_FMT_RGBA, AVPixelFormat_AV_PIX_FMT_ABGR);
pub const AV_PIX_FMT_BGR32: AVPixelFormat   = av_pix_fmt_ne(AVPixelFormat_AV_PIX_FMT_ABGR, AVPixelFormat_AV_PIX_FMT_RGBA);
pub const AV_PIX_FMT_BGR32_1: AVPixelFormat = av_pix_fmt_ne(AVPixelFormat_AV_PIX_FMT_BGRA, AVPixelFormat_AV_PIX_FMT_ARGB);
pub const AV_PIX_FMT_0RGB32: AVPixelFormat  = av_pix_fmt_ne(AVPixelFormat_AV_PIX_FMT_0RGB, AVPixelFormat_AV_PIX_FMT_BGR0);
pub const AV_PIX_FMT_0BGR32: AVPixelFormat  = av_pix_fmt_ne(AVPixelFormat_AV_PIX_FMT_0BGR, AVPixelFormat_AV_PIX_FMT_RGB0);

// Errors

pub const fn av_error(error: i32) -> i32 { -error }
pub const fn mktag(a: char, b: char, c: char, d: char) -> i32 {
	(a as i32) | ((b as i32) << 8) | ((c as i32) << 16) | ((d as i32) << 24)
}
pub const fn fferrtag(a: char, b: char, c: char, d: char) -> i32 {
	-mktag(a, b, c, d)
}

pub const AVERROR_BSF_NOT_FOUND: i32      = fferrtag(0xF8 as char,'B','S','F'); ///< Bitstream filter not found
pub const AVERROR_BUG: i32                = fferrtag( 'B','U','G','!'); ///< Internal bug, also see AVERROR_BUG2
pub const AVERROR_BUFFER_TOO_SMALL: i32   = fferrtag( 'B','U','F','S'); ///< Buffer too small
pub const AVERROR_DECODER_NOT_FOUND: i32  = fferrtag(0xF8 as char,'D','E','C'); ///< Decoder not found
pub const AVERROR_DEMUXER_NOT_FOUND: i32  = fferrtag(0xF8 as char,'D','E','M'); ///< Demuxer not found
pub const AVERROR_ENCODER_NOT_FOUND: i32  = fferrtag(0xF8 as char,'E','N','C'); ///< Encoder not found
pub const AVERROR_EOF: i32                = fferrtag( 'E','O','F',' '); ///< End of file
pub const AVERROR_EXIT: i32               = fferrtag( 'E','X','I','T'); ///< Immediate exit was requested; the called function should not be restarted
pub const AVERROR_EXTERNAL: i32           = fferrtag( 'E','X','T',' '); ///< Generic error in an external library
pub const AVERROR_FILTER_NOT_FOUND: i32   = fferrtag(0xF8 as char,'F','I','L'); ///< Filter not found
pub const AVERROR_INVALIDDATA: i32        = fferrtag( 'I','N','D','A'); ///< Invalid data found when processing input
pub const AVERROR_MUXER_NOT_FOUND: i32    = fferrtag(0xF8 as char,'M','U','X'); ///< Muxer not found
pub const AVERROR_OPTION_NOT_FOUND: i32   = fferrtag(0xF8 as char,'O','P','T'); ///< Option not found
pub const AVERROR_PATCHWELCOME: i32       = fferrtag( 'P','A','W','E'); ///< Not yet implemented in FFmpeg, patches welcome
pub const AVERROR_PROTOCOL_NOT_FOUND: i32 = fferrtag(0xF8 as char,'P','R','O'); ///< Protocol not found

pub const AVERROR_STREAM_NOT_FOUND: i32   = fferrtag(0xF8 as char,'S','T','R'); ///< Stream not found
/**
 * This is semantically identical to AVERROR_BUG
 * it has been introduced in Libav after our AVERROR_BUG and with a modified value.
 */
pub const AVERROR_BUG2: i32               = fferrtag( 'B','U','G',' ');
pub const AVERROR_UNKNOWN: i32            = fferrtag( 'U','N','K','N'); ///< Unknown error, typically from an external library
/* HTTP & RTSP errors */
pub const AVERROR_HTTP_BAD_REQUEST: i32   = fferrtag(0xF8 as char,'4','0','0');
pub const AVERROR_HTTP_UNAUTHORIZED: i32  = fferrtag(0xF8 as char,'4','0','1');
pub const AVERROR_HTTP_FORBIDDEN: i32     = fferrtag(0xF8 as char,'4','0','3');
pub const AVERROR_HTTP_NOT_FOUND: i32     = fferrtag(0xF8 as char,'4','0','4');
pub const AVERROR_HTTP_OTHER_4XX: i32     = fferrtag(0xF8 as char,'4','X','X');
pub const AVERROR_HTTP_SERVER_ERROR: i32  = fferrtag(0xF8 as char,'5','X','X');

// Channel layout

pub const fn av_channel_layout_mask(nb: i32, m: u64) -> AVChannelLayout {
	AVChannelLayout {
		order: AVChannelOrder_AV_CHANNEL_ORDER_NATIVE,
		nb_channels: nb,
		u: AVChannelLayout__bindgen_ty_1 { mask: m },
		opaque: null_mut(),
	}
}

pub const AV_CH_FRONT_LEFT: u64            = 1u64 << AVChannel_AV_CHAN_FRONT_LEFT;
pub const AV_CH_FRONT_RIGHT: u64           = 1u64 << AVChannel_AV_CHAN_FRONT_RIGHT;
pub const AV_CH_FRONT_CENTER: u64          = 1u64 << AVChannel_AV_CHAN_FRONT_CENTER;
pub const AV_CH_LOW_FREQUENCY: u64         = 1u64 << AVChannel_AV_CHAN_LOW_FREQUENCY;
pub const AV_CH_BACK_LEFT: u64             = 1u64 << AVChannel_AV_CHAN_BACK_LEFT;
pub const AV_CH_BACK_RIGHT: u64            = 1u64 << AVChannel_AV_CHAN_BACK_RIGHT;
pub const AV_CH_FRONT_LEFT_OF_CENTER: u64  = 1u64 << AVChannel_AV_CHAN_FRONT_LEFT_OF_CENTER;
pub const AV_CH_FRONT_RIGHT_OF_CENTER: u64 = 1u64 << AVChannel_AV_CHAN_FRONT_RIGHT_OF_CENTER;
pub const AV_CH_BACK_CENTER: u64           = 1u64 << AVChannel_AV_CHAN_BACK_CENTER;
pub const AV_CH_SIDE_LEFT: u64             = 1u64 << AVChannel_AV_CHAN_SIDE_LEFT;
pub const AV_CH_SIDE_RIGHT: u64            = 1u64 << AVChannel_AV_CHAN_SIDE_RIGHT;
pub const AV_CH_TOP_CENTER: u64            = 1u64 << AVChannel_AV_CHAN_TOP_CENTER;
pub const AV_CH_TOP_FRONT_LEFT: u64        = 1u64 << AVChannel_AV_CHAN_TOP_FRONT_LEFT;
pub const AV_CH_TOP_FRONT_CENTER: u64      = 1u64 << AVChannel_AV_CHAN_TOP_FRONT_CENTER;
pub const AV_CH_TOP_FRONT_RIGHT: u64       = 1u64 << AVChannel_AV_CHAN_TOP_FRONT_RIGHT;
pub const AV_CH_TOP_BACK_LEFT: u64         = 1u64 << AVChannel_AV_CHAN_TOP_BACK_LEFT;
pub const AV_CH_TOP_BACK_CENTER: u64       = 1u64 << AVChannel_AV_CHAN_TOP_BACK_CENTER;
pub const AV_CH_TOP_BACK_RIGHT: u64        = 1u64 << AVChannel_AV_CHAN_TOP_BACK_RIGHT;
pub const AV_CH_STEREO_LEFT: u64           = 1u64 << AVChannel_AV_CHAN_STEREO_LEFT;
pub const AV_CH_STEREO_RIGHT: u64          = 1u64 << AVChannel_AV_CHAN_STEREO_RIGHT;
pub const AV_CH_WIDE_LEFT: u64             = 1u64 << AVChannel_AV_CHAN_WIDE_LEFT;
pub const AV_CH_WIDE_RIGHT: u64            = 1u64 << AVChannel_AV_CHAN_WIDE_RIGHT;
pub const AV_CH_SURROUND_DIRECT_LEFT: u64  = 1u64 << AVChannel_AV_CHAN_SURROUND_DIRECT_LEFT;
pub const AV_CH_SURROUND_DIRECT_RIGHT: u64 = 1u64 << AVChannel_AV_CHAN_SURROUND_DIRECT_RIGHT;
pub const AV_CH_LOW_FREQUENCY_2: u64       = 1u64 << AVChannel_AV_CHAN_LOW_FREQUENCY_2;
pub const AV_CH_TOP_SIDE_LEFT: u64         = 1u64 << AVChannel_AV_CHAN_TOP_SIDE_LEFT;
pub const AV_CH_TOP_SIDE_RIGHT: u64        = 1u64 << AVChannel_AV_CHAN_TOP_SIDE_RIGHT;
pub const AV_CH_BOTTOM_FRONT_CENTER: u64   = 1u64 << AVChannel_AV_CHAN_BOTTOM_FRONT_CENTER;
pub const AV_CH_BOTTOM_FRONT_LEFT: u64     = 1u64 << AVChannel_AV_CHAN_BOTTOM_FRONT_LEFT;
pub const AV_CH_BOTTOM_FRONT_RIGHT: u64    = 1u64 << AVChannel_AV_CHAN_BOTTOM_FRONT_RIGHT;

pub const AV_CH_LAYOUT_MONO: u64              = AV_CH_FRONT_CENTER;
pub const AV_CH_LAYOUT_STEREO: u64            = AV_CH_FRONT_LEFT|AV_CH_FRONT_RIGHT;
pub const AV_CH_LAYOUT_2POINT1: u64           = AV_CH_LAYOUT_STEREO|AV_CH_LOW_FREQUENCY;
pub const AV_CH_LAYOUT_2_1: u64               = AV_CH_LAYOUT_STEREO|AV_CH_BACK_CENTER;
pub const AV_CH_LAYOUT_SURROUND: u64          = AV_CH_LAYOUT_STEREO|AV_CH_FRONT_CENTER;
pub const AV_CH_LAYOUT_3POINT1: u64           = AV_CH_LAYOUT_SURROUND|AV_CH_LOW_FREQUENCY;
pub const AV_CH_LAYOUT_4POINT0: u64           = AV_CH_LAYOUT_SURROUND|AV_CH_BACK_CENTER;
pub const AV_CH_LAYOUT_4POINT1: u64           = AV_CH_LAYOUT_4POINT0|AV_CH_LOW_FREQUENCY;
pub const AV_CH_LAYOUT_2_2: u64               = AV_CH_LAYOUT_STEREO|AV_CH_SIDE_LEFT|AV_CH_SIDE_RIGHT;
pub const AV_CH_LAYOUT_QUAD: u64              = AV_CH_LAYOUT_STEREO|AV_CH_BACK_LEFT|AV_CH_BACK_RIGHT;
pub const AV_CH_LAYOUT_5POINT0: u64           = AV_CH_LAYOUT_SURROUND|AV_CH_SIDE_LEFT|AV_CH_SIDE_RIGHT;
pub const AV_CH_LAYOUT_5POINT1: u64           = AV_CH_LAYOUT_5POINT0|AV_CH_LOW_FREQUENCY;
pub const AV_CH_LAYOUT_5POINT0_BACK: u64      = AV_CH_LAYOUT_SURROUND|AV_CH_BACK_LEFT|AV_CH_BACK_RIGHT;
pub const AV_CH_LAYOUT_5POINT1_BACK: u64      = AV_CH_LAYOUT_5POINT0_BACK|AV_CH_LOW_FREQUENCY;
pub const AV_CH_LAYOUT_6POINT0: u64           = AV_CH_LAYOUT_5POINT0|AV_CH_BACK_CENTER;
pub const AV_CH_LAYOUT_6POINT0_FRONT: u64     = AV_CH_LAYOUT_2_2|AV_CH_FRONT_LEFT_OF_CENTER|AV_CH_FRONT_RIGHT_OF_CENTER;
pub const AV_CH_LAYOUT_HEXAGONAL: u64         = AV_CH_LAYOUT_5POINT0_BACK|AV_CH_BACK_CENTER;
pub const AV_CH_LAYOUT_6POINT1: u64           = AV_CH_LAYOUT_5POINT1|AV_CH_BACK_CENTER;
pub const AV_CH_LAYOUT_6POINT1_BACK: u64      = AV_CH_LAYOUT_5POINT1_BACK|AV_CH_BACK_CENTER;
pub const AV_CH_LAYOUT_6POINT1_FRONT: u64     = AV_CH_LAYOUT_6POINT0_FRONT|AV_CH_LOW_FREQUENCY;
pub const AV_CH_LAYOUT_7POINT0: u64           = AV_CH_LAYOUT_5POINT0|AV_CH_BACK_LEFT|AV_CH_BACK_RIGHT;
pub const AV_CH_LAYOUT_7POINT0_FRONT: u64     = AV_CH_LAYOUT_5POINT0|AV_CH_FRONT_LEFT_OF_CENTER|AV_CH_FRONT_RIGHT_OF_CENTER;
pub const AV_CH_LAYOUT_7POINT1: u64           = AV_CH_LAYOUT_5POINT1|AV_CH_BACK_LEFT|AV_CH_BACK_RIGHT;
pub const AV_CH_LAYOUT_7POINT1_WIDE: u64      = AV_CH_LAYOUT_5POINT1|AV_CH_FRONT_LEFT_OF_CENTER|AV_CH_FRONT_RIGHT_OF_CENTER;
pub const AV_CH_LAYOUT_7POINT1_WIDE_BACK: u64 = AV_CH_LAYOUT_5POINT1_BACK|AV_CH_FRONT_LEFT_OF_CENTER|AV_CH_FRONT_RIGHT_OF_CENTER;
pub const AV_CH_LAYOUT_7POINT1_TOP_BACK: u64  = AV_CH_LAYOUT_5POINT1_BACK|AV_CH_TOP_FRONT_LEFT|AV_CH_TOP_FRONT_RIGHT;
pub const AV_CH_LAYOUT_OCTAGONAL: u64         = AV_CH_LAYOUT_5POINT0|AV_CH_BACK_LEFT|AV_CH_BACK_CENTER|AV_CH_BACK_RIGHT;
pub const AV_CH_LAYOUT_CUBE: u64              = AV_CH_LAYOUT_QUAD|AV_CH_TOP_FRONT_LEFT|AV_CH_TOP_FRONT_RIGHT|AV_CH_TOP_BACK_LEFT|AV_CH_TOP_BACK_RIGHT;
pub const AV_CH_LAYOUT_HEXADECAGONAL: u64     = AV_CH_LAYOUT_OCTAGONAL|AV_CH_WIDE_LEFT|AV_CH_WIDE_RIGHT|AV_CH_TOP_BACK_LEFT|AV_CH_TOP_BACK_RIGHT|AV_CH_TOP_BACK_CENTER|AV_CH_TOP_FRONT_CENTER|AV_CH_TOP_FRONT_LEFT|AV_CH_TOP_FRONT_RIGHT;
pub const AV_CH_LAYOUT_STEREO_DOWNMIX: u64    = AV_CH_STEREO_LEFT|AV_CH_STEREO_RIGHT;
pub const AV_CH_LAYOUT_22POINT2: u64          = AV_CH_LAYOUT_5POINT1_BACK|AV_CH_FRONT_LEFT_OF_CENTER|AV_CH_FRONT_RIGHT_OF_CENTER|AV_CH_BACK_CENTER|AV_CH_LOW_FREQUENCY_2|AV_CH_SIDE_LEFT|AV_CH_SIDE_RIGHT|AV_CH_TOP_FRONT_LEFT|AV_CH_TOP_FRONT_RIGHT|AV_CH_TOP_FRONT_CENTER|AV_CH_TOP_CENTER|AV_CH_TOP_BACK_LEFT|AV_CH_TOP_BACK_RIGHT|AV_CH_TOP_SIDE_LEFT|AV_CH_TOP_SIDE_RIGHT|AV_CH_TOP_BACK_CENTER|AV_CH_BOTTOM_FRONT_CENTER|AV_CH_BOTTOM_FRONT_LEFT|AV_CH_BOTTOM_FRONT_RIGHT;

pub const AV_CHANNEL_LAYOUT_MONO: AVChannelLayout              = av_channel_layout_mask(1, AV_CH_LAYOUT_MONO);
pub const AV_CHANNEL_LAYOUT_STEREO: AVChannelLayout            = av_channel_layout_mask(2,  AV_CH_LAYOUT_STEREO);
pub const AV_CHANNEL_LAYOUT_2POINT1: AVChannelLayout           = av_channel_layout_mask(3,  AV_CH_LAYOUT_2POINT1);
pub const AV_CHANNEL_LAYOUT_2_1: AVChannelLayout               = av_channel_layout_mask(3,  AV_CH_LAYOUT_2_1);
pub const AV_CHANNEL_LAYOUT_SURROUND: AVChannelLayout          = av_channel_layout_mask(3,  AV_CH_LAYOUT_SURROUND);
pub const AV_CHANNEL_LAYOUT_3POINT1: AVChannelLayout           = av_channel_layout_mask(4,  AV_CH_LAYOUT_3POINT1);
pub const AV_CHANNEL_LAYOUT_4POINT0: AVChannelLayout           = av_channel_layout_mask(4,  AV_CH_LAYOUT_4POINT0);
pub const AV_CHANNEL_LAYOUT_4POINT1: AVChannelLayout           = av_channel_layout_mask(5,  AV_CH_LAYOUT_4POINT1);
pub const AV_CHANNEL_LAYOUT_2_2: AVChannelLayout               = av_channel_layout_mask(4,  AV_CH_LAYOUT_2_2);
pub const AV_CHANNEL_LAYOUT_QUAD: AVChannelLayout              = av_channel_layout_mask(4,  AV_CH_LAYOUT_QUAD);
pub const AV_CHANNEL_LAYOUT_5POINT0: AVChannelLayout           = av_channel_layout_mask(5,  AV_CH_LAYOUT_5POINT0);
pub const AV_CHANNEL_LAYOUT_5POINT1: AVChannelLayout           = av_channel_layout_mask(6,  AV_CH_LAYOUT_5POINT1);
pub const AV_CHANNEL_LAYOUT_5POINT0_BACK: AVChannelLayout      = av_channel_layout_mask(5,  AV_CH_LAYOUT_5POINT0_BACK);
pub const AV_CHANNEL_LAYOUT_5POINT1_BACK: AVChannelLayout      = av_channel_layout_mask(6,  AV_CH_LAYOUT_5POINT1_BACK);
pub const AV_CHANNEL_LAYOUT_6POINT0: AVChannelLayout           = av_channel_layout_mask(6,  AV_CH_LAYOUT_6POINT0);
pub const AV_CHANNEL_LAYOUT_6POINT0_FRONT: AVChannelLayout     = av_channel_layout_mask(6,  AV_CH_LAYOUT_6POINT0_FRONT);
pub const AV_CHANNEL_LAYOUT_HEXAGONAL: AVChannelLayout         = av_channel_layout_mask(6,  AV_CH_LAYOUT_HEXAGONAL);
pub const AV_CHANNEL_LAYOUT_6POINT1: AVChannelLayout           = av_channel_layout_mask(7,  AV_CH_LAYOUT_6POINT1);
pub const AV_CHANNEL_LAYOUT_6POINT1_BACK: AVChannelLayout      = av_channel_layout_mask(7,  AV_CH_LAYOUT_6POINT1_BACK);
pub const AV_CHANNEL_LAYOUT_6POINT1_FRONT: AVChannelLayout     = av_channel_layout_mask(7,  AV_CH_LAYOUT_6POINT1_FRONT);
pub const AV_CHANNEL_LAYOUT_7POINT0: AVChannelLayout           = av_channel_layout_mask(7,  AV_CH_LAYOUT_7POINT0);
pub const AV_CHANNEL_LAYOUT_7POINT0_FRONT: AVChannelLayout     = av_channel_layout_mask(7,  AV_CH_LAYOUT_7POINT0_FRONT);
pub const AV_CHANNEL_LAYOUT_7POINT1: AVChannelLayout           = av_channel_layout_mask(8,  AV_CH_LAYOUT_7POINT1);
pub const AV_CHANNEL_LAYOUT_7POINT1_WIDE: AVChannelLayout      = av_channel_layout_mask(8,  AV_CH_LAYOUT_7POINT1_WIDE);
pub const AV_CHANNEL_LAYOUT_7POINT1_WIDE_BACK: AVChannelLayout = av_channel_layout_mask(8,  AV_CH_LAYOUT_7POINT1_WIDE_BACK);
pub const AV_CHANNEL_LAYOUT_7POINT1_TOP_BACK: AVChannelLayout  = av_channel_layout_mask(8,  AV_CH_LAYOUT_7POINT1_TOP_BACK);
pub const AV_CHANNEL_LAYOUT_OCTAGONAL: AVChannelLayout         = av_channel_layout_mask(8,  AV_CH_LAYOUT_OCTAGONAL);
pub const AV_CHANNEL_LAYOUT_CUBE: AVChannelLayout              = av_channel_layout_mask(8,  AV_CH_LAYOUT_CUBE);
pub const AV_CHANNEL_LAYOUT_HEXADECAGONAL: AVChannelLayout     = av_channel_layout_mask(16, AV_CH_LAYOUT_HEXADECAGONAL);
pub const AV_CHANNEL_LAYOUT_STEREO_DOWNMIX: AVChannelLayout    = av_channel_layout_mask(2,  AV_CH_LAYOUT_STEREO_DOWNMIX);
pub const AV_CHANNEL_LAYOUT_22POINT2: AVChannelLayout          = av_channel_layout_mask(24, AV_CH_LAYOUT_22POINT2);

unsafe impl Sync for AVPacket {}
unsafe impl Send for AVPacket {}
