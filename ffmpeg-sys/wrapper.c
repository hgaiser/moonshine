#include "wrapper.h"
#include <assert.h>

CUcontext init_cuda() {
	CUresult res;

	res = cuInit(0);
	if (res != CUDA_SUCCESS) {
		const char *err_str;
		cuGetErrorString(res, &err_str);
		fprintf(stderr, "Error: cuInit failed, error %s (result: %d)\n", err_str, res);
		return 1;
	}

	int nGpu = 0;
	cuDeviceGetCount(&nGpu);
	if (nGpu <= 0) {
		fprintf(stderr, "Error: no cuda supported devices found\n");
		return 1;
	}

	CUdevice cu_dev;
	res = cuDeviceGet(&cu_dev, 0);
	if (res != CUDA_SUCCESS) {
		const char *err_str;
		cuGetErrorString(res, &err_str);
		fprintf(stderr, "Error: unable to get CUDA device, error: %s (result: %d)\n", err_str, res);
		return 1;
	}

	CUcontext cu_ctx;
	res = cuCtxCreate_v2(&cu_ctx, CU_CTX_SCHED_AUTO, cu_dev);
	if (res != CUDA_SUCCESS) {
		const char *err_str;
		cuGetErrorString(res, &err_str);
		fprintf(stderr, "Error: unable to create CUDA context, error: %s (result: %d)\n", err_str, res);
		return 1;
	}

	return cu_ctx;
}

AVCodecContext * create_video_codec_context(
	AVFormatContext * av_format_context,
	enum VideoQuality video_quality,
	uint32_t record_width, uint32_t record_height,
	uint32_t fps, bool use_hevc
) {
	const AVCodec *codec = avcodec_find_encoder_by_name(use_hevc ? "hevc_nvenc" : "h264_nvenc");
	if (!codec) {
		codec = avcodec_find_encoder_by_name(use_hevc ? "nvenc_hevc" : "nvenc_h264");
	}
	if (!codec) {
		fprintf(
				stderr,
				"Error: Could not find %s encoder\n", use_hevc ? "hevc" : "h264");
		exit(1);
	}

	AVCodecContext *codec_context = avcodec_alloc_context3(codec);

	//double fps_ratio = (double)fps / 30.0;

	assert(codec->type == AVMEDIA_TYPE_VIDEO);
	codec_context->codec_id = codec->id;
	codec_context->width = record_width & ~1;
	codec_context->height = record_height & ~1;
	codec_context->bit_rate = 12500000 + (codec_context->width * codec_context->height) / 2;
	// Timebase: This is the fundamental unit of time (in seconds) in terms
	// of which frame timestamps are represented. For fixed-fps content,
	// timebase should be 1/framerate and timestamp increments should be
	// identical to 1
	codec_context->time_base.num = 1;
	codec_context->time_base.den = AV_TIME_BASE;
	codec_context->framerate.num = fps;
	codec_context->framerate.den = 1;
	codec_context->sample_aspect_ratio.num = 0;
	codec_context->sample_aspect_ratio.den = 0;
	codec_context->gop_size = fps * 2;
	codec_context->max_b_frames = 0;
	codec_context->pix_fmt = AV_PIX_FMT_CUDA;
	codec_context->color_range = AVCOL_RANGE_JPEG;
	switch(video_quality) {
		case LOW:
			codec_context->bit_rate = 10000000 + (codec_context->width * codec_context->height) / 2;
			if(use_hevc) {
				codec_context->qmin = 20;
				codec_context->qmax = 35;
			} else {
				codec_context->qmin = 5;
				codec_context->qmax = 20;
			}
			//av_opt_set(codec_context->priv_data, "preset", "slow", 0);
			//av_opt_set(codec_context->priv_data, "profile", "high", 0);
			//codec_context->profile = FF_PROFILE_H264_HIGH;
			//av_opt_set(codec_context->priv_data, "preset", "p4", 0);
			break;
		case MEDIUM:
			if(use_hevc) {
				codec_context->qmin = 17;
				codec_context->qmax = 30;
			} else {
				codec_context->qmin = 5;
				codec_context->qmax = 15;
			}
			//av_opt_set(codec_context->priv_data, "preset", "slow", 0);
			//av_opt_set(codec_context->priv_data, "profile", "high", 0);
			//codec_context->profile = FF_PROFILE_H264_HIGH;
			//av_opt_set(codec_context->priv_data, "preset", "p5", 0);
			break;
		case HIGH:
			codec_context->bit_rate = 15000000 + (codec_context->width * codec_context->height) / 2;
			if(use_hevc) {
				codec_context->qmin = 16;
				codec_context->qmax = 25;
			} else {
				codec_context->qmin = 3;
				codec_context->qmax = 13;
			}
			//av_opt_set(codec_context->priv_data, "preset", "veryslow", 0);
			//av_opt_set(codec_context->priv_data, "profile", "high", 0);
			//codec_context->profile = FF_PROFILE_H264_HIGH;
			//av_opt_set(codec_context->priv_data, "preset", "p7", 0);
			break;
	}
	if (codec_context->codec_id == AV_CODEC_ID_MPEG1VIDEO)
		codec_context->mb_decision = 2;

	// stream->time_base = codec_context->time_base;
	// codec_context->ticks_per_frame = 30;
	//av_opt_set(codec_context->priv_data, "tune", "hq", 0);
	//av_opt_set(codec_context->priv_data, "rc", "vbr", 0);

	// Some formats want stream headers to be seperate
	if (av_format_context->oformat->flags & AVFMT_GLOBALHEADER)
		av_format_context->flags |= AV_CODEC_FLAG_GLOBAL_HEADER;

	return codec_context;
}

void open_video(
	AVCodecContext * codec_context,
	AVBufferRef ** device_ctx,
	CUgraphicsResource * cuda_graphics_resource,
	CUcontext cuda_context
) {
	int ret;

	*device_ctx = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_CUDA);
	if(!*device_ctx) {
		fprintf(stderr, "Error: Failed to create hardware device context\n");
		exit(1);
	}

	AVHWDeviceContext *hw_device_context = (AVHWDeviceContext *)(*device_ctx)->data;
	AVCUDADeviceContext *cuda_device_context = (AVCUDADeviceContext *)hw_device_context->hwctx;
	cuda_device_context->cuda_ctx = cuda_context;
	if(av_hwdevice_ctx_init(*device_ctx) < 0) {
		fprintf(stderr, "Error: Failed to create hardware device context\n");
		exit(1);
	}

	AVBufferRef *frame_context = av_hwframe_ctx_alloc(*device_ctx);
	if (!frame_context) {
		fprintf(stderr, "Error: Failed to create hwframe context\n");
		exit(1);
	}

	AVHWFramesContext *hw_frame_context =
		(AVHWFramesContext *)frame_context->data;
	hw_frame_context->width = codec_context->width;
	hw_frame_context->height = codec_context->height;
	hw_frame_context->sw_format = AV_PIX_FMT_0RGB32;
	hw_frame_context->format = codec_context->pix_fmt;
	hw_frame_context->device_ref = *device_ctx;
	hw_frame_context->device_ctx = (AVHWDeviceContext *)(*device_ctx)->data;

	if (av_hwframe_ctx_init(frame_context) < 0) {
		fprintf(stderr, "Error: Failed to initialize hardware frame context "
				"(note: ffmpeg version needs to be > 4.0\n");
		exit(1);
	}

	codec_context->hw_device_ctx = *device_ctx;
	codec_context->hw_frames_ctx = frame_context;

	ret = avcodec_open2(codec_context, codec_context->codec, NULL);
	if (ret < 0) {
		fprintf(stderr, "Error: Could not open video codec: %s\n",
				"blabla"); // av_err2str(ret));
		exit(1);
	}
}

AVStream * create_stream(
	AVFormatContext * av_format_context,
	AVCodecContext * codec_context
) {
	AVStream *stream = avformat_new_stream(av_format_context, NULL);
	if (!stream) {
		fprintf(stderr, "Error: Could not allocate stream\n");
		exit(1);
	}
	stream->id = av_format_context->nb_streams - 1;
	stream->time_base = codec_context->time_base;
	stream->avg_frame_rate = codec_context->framerate;
	return stream;
}

void receive_frames(
	AVCodecContext * av_codec_context,
	int stream_index,
	AVStream * stream,
	AVFrame * frame,
	AVFormatContext * av_format_context
	// std::mutex &write_output_mutex
) {
	AVPacket av_packet;
	memset(&av_packet, 0, sizeof(av_packet));
	for (;;) {
		av_packet.data = NULL;
		av_packet.size = 0;
		int res = avcodec_receive_packet(av_codec_context, &av_packet);
		if (res == 0) { // we have a packet, send the packet to the muxer
			av_packet.stream_index = stream_index;
			av_packet.pts = av_packet.dts = frame->pts;

			// std::lock_guard<std::mutex> lock(write_output_mutex);
			av_packet_rescale_ts(&av_packet, av_codec_context->time_base, stream->time_base);
			av_packet.stream_index = stream->index;
			int ret = av_interleaved_write_frame(av_format_context, &av_packet);
			if(ret < 0) {
				/* fprintf(stderr, "Error: Failed to write frame index %d to muxer, reason: %s (%d)\n", av_packet.stream_index, av_error_to_string(ret), ret); */
				fprintf(stderr, "Error: Failed to write frame index %d to muxer, reason: %d\n", av_packet.stream_index, ret);
			}
			av_packet_unref(&av_packet);
		} else if (res == AVERROR(EAGAIN)) { // we have no packet
			// fprintf(stderr, "No packet!\n");
			break;
		} else if (res == AVERROR_EOF) { // this is the end of the stream
			fprintf(stderr, "End of stream!\n");
			break;
		} else {
			fprintf(stderr, "Unexpected error: %d\n", res);
			break;
		}
	}
	//av_packet_unref(&av_packet);
}
