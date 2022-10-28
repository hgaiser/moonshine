#pragma once

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext_cuda.h>
#include <libavutil/opt.h>

int ff_rtp_get_local_rtp_port(struct URLContext *h);
int ff_rtp_get_local_rtcp_port(struct URLContext *h);
