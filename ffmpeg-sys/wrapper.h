#pragma once

#include <libavutil/pixfmt.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_cuda.h>
#include <libavutil/opt.h>
#include <libswresample/swresample.h>
#include <libavutil/avutil.h>
#include <libavutil/time.h>

#include <stdbool.h>

enum VideoQuality {
	LOW = 0,
	MEDIUM = 1,
	HIGH = 2
};

CUcontext init_cuda();

AVCodecContext * create_video_codec_context(
	AVFormatContext * av_format_context,
	enum VideoQuality video_quality,
	uint32_t record_width, uint32_t record_height,
	uint32_t fps, bool use_hevc
);

void open_video(
	AVCodecContext * codec_context,
	AVBufferRef ** device_ctx,
	CUgraphicsResource * cuda_graphics_resource,
	CUcontext cuda_context
);

AVStream * create_stream(
	AVFormatContext * av_format_context,
	AVCodecContext * codec_context
);

void receive_frames(
	AVCodecContext * av_codec_context,
	int stream_index,
	AVStream * stream,
	AVFrame * frame,
	AVFormatContext * av_format_context
	// std::mutex &write_output_mutex
);
