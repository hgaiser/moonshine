#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

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
