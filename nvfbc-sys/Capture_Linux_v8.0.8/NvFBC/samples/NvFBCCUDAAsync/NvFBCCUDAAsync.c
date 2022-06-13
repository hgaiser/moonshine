/*!
 * \brief
 * Demonstrates how to use NvFBC to asynchronously grab frames to
 * video memory then save them to the disk.
 *
 * \file
 * This sample demonstrates the following features:
 * - Capture to video memory using CUDA interop;
 * - Select an output (monitor) to track;
 * - Select and test buffer formats;
 * - Frame scaling;
 * - Asynchronous (non blocking) capture.
 *
 * This copyright notice applies to this file only:
 *
 * \copyright
 * Copyright (c) 2013-2017, NVIDIA CORPORATION. All rights reserved.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a
 * copy of this software and associated documentation files (the "Software"),
 * to deal in the Software without restriction, including without limitation
 * the rights to use, copy, modify, merge, publish, distribute, sublicense,
 * and/or sell copies of the Software, and to permit persons to whom the
 * Software is furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
 * THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
 * FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
 * DEALINGS IN THE SOFTWARE.
 */

#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <dlfcn.h>
#include <string.h>
#include <getopt.h>
#include <unistd.h>

#include <NvFBC.h>
#include <cuda.h>

#include "NvFBCUtils.h"

#define APP_VERSION 4

#define LIB_NVFBC_NAME "libnvidia-fbc.so.1"
#define LIB_CUDA_NAME  "libcuda.so.1"

#define N_FRAMES 10

/*
 * CUDA entry points
 */
typedef CUresult (* CUINITPROC) (unsigned int Flags);
typedef CUresult (* CUDEVICEGETPROC) (CUdevice *device, int ordinal);
typedef CUresult (* CUCTXCREATEV2PROC) (CUcontext *pctx, unsigned int flags, CUdevice dev);
typedef CUresult (* CUMEMCPYDTOHV2PROC) (void *dstHost, CUdeviceptr srcDevice, size_t ByteCount);

static CUINITPROC cuInit_ptr = NULL;
static CUDEVICEGETPROC cuDeviceGet_ptr = NULL;
static CUCTXCREATEV2PROC cuCtxCreate_v2_ptr = NULL;
static CUMEMCPYDTOHV2PROC cuMemcpyDtoH_v2_ptr = NULL;

/**
 * Dynamically opens the CUDA library and resolves the symbols that are
 * needed for this application.
 *
 * \param [out] libCUDA
 *   A pointer to the opened CUDA library.
 *
 * \return
 *   NVFBC_TRUE in case of success, NVFBC_FALSE otherwise.
 */
static NVFBC_BOOL cuda_load_library(void *libCUDA)
{
    libCUDA = dlopen(LIB_CUDA_NAME, RTLD_NOW);
    if (libCUDA == NULL) {
        fprintf(stderr, "Unable to open '%s'\n", LIB_CUDA_NAME);
        return NVFBC_FALSE;
    }

    cuInit_ptr = (CUINITPROC) dlsym(libCUDA, "cuInit");
    if (cuInit_ptr == NULL) {
        fprintf(stderr, "Unable to resolve symbol 'cuInit'\n");
        return NVFBC_FALSE;
    }

    cuDeviceGet_ptr = (CUDEVICEGETPROC) dlsym(libCUDA, "cuDeviceGet");
    if (cuDeviceGet_ptr == NULL) {
        fprintf(stderr, "Unable to resolve symbol 'cuDeviceGet'\n");
        return NVFBC_FALSE;
    }

    cuCtxCreate_v2_ptr = (CUCTXCREATEV2PROC) dlsym(libCUDA, "cuCtxCreate_v2");
    if (cuCtxCreate_v2_ptr == NULL) {
        fprintf(stderr, "Unable to resolve symbol 'cuCtxCreate_v2'\n");
        return NVFBC_FALSE;
    }

    cuMemcpyDtoH_v2_ptr = (CUMEMCPYDTOHV2PROC) dlsym(libCUDA, "cuMemcpyDtoH_v2");
    if (cuMemcpyDtoH_v2_ptr == NULL) {
        fprintf(stderr, "Unable to resolve symbol 'cuMemcpyDtoH_v2'\n");
        return NVFBC_FALSE;
    }

    return NVFBC_TRUE;
}

/**
 * Initializes CUDA and creates a CUDA context.
 *
 * \param [in] cuCtx
 *   A pointer to the created CUDA context.
 *
 * \return
 *   NVFBC_TRUE in case of success, NVFBC_FALSE otherwise.
 */
static NVFBC_BOOL cuda_init(CUcontext *cuCtx)
{
    CUresult cuRes;
    CUdevice cuDev;

    cuRes = cuInit_ptr(0);
    if (cuRes != CUDA_SUCCESS) {
        fprintf(stderr, "Unable to initialize CUDA (result: %d)\n", cuRes);
        return NVFBC_FALSE;
    }

    cuRes = cuDeviceGet_ptr(&cuDev, 0);
    if (cuRes != CUDA_SUCCESS) {
        fprintf(stderr, "Unable to get CUDA device (result: %d)\n", cuRes);
        return NVFBC_FALSE;
    }

    cuRes = cuCtxCreate_v2_ptr(cuCtx, CU_CTX_SCHED_AUTO, cuDev);
    if (cuRes != CUDA_SUCCESS) {
        fprintf(stderr, "Unable to create CUDA context (result: %d)\n", cuRes);
        return NVFBC_FALSE;
    }

    return NVFBC_TRUE;
}

/**
 * Prints usage information.
 */
static void usage(const char *pname)
{
    printf("Usage: %s [options]\n", pname);
    printf("\n");
    printf("Options:\n");
    printf("  --help|-h         This message\n");
    printf("  --get-status|-g   Print status and exit\n");
    printf("  --track|-t <str>  Region of the screen to track.\n");
    printf("                    Can be 'default', 'screen' or '<output name>'\n");
    printf("                    as returned by --get-status\n");
    printf("  --frames|-f <n>   Number of frames to capture (default: %u)\n",
           N_FRAMES);
    printf("  --size|-s <w>x<h> Size of the captured frames\n");
    printf("                    (default: size of the framebuffer)\n");
    printf("  --format|-o <fmt> Buffer format to use.\n");
    printf("                    Can be 'rgb', 'argb', 'nv12' or 'yuv444p'\n");
    printf("                    Default: 'rgb'\n");
}

/**
 * Initializes the NvFBC and CUDA libraries and creates an NvFBC instance.
 *
 * Creates and sets up a capture session to video memory using the CUDA interop.
 *
 * Captures a bunch of frames every second, converts them to BMP and saves them
 * to the disk.
 */
int main(int argc, char *argv[])
{
    static struct option longopts[] = {
        { "get-status", no_argument, NULL, 'g' },
        { "track", required_argument, NULL, 't' },
        { "frames", required_argument, NULL, 'f' },
        { "size", required_argument, NULL, 's' },
        { "format", required_argument, NULL, 'o' },
        { NULL, 0, NULL, 0 }
    };

    int opt, ret;
    unsigned int i, nFrames = N_FRAMES;
    NVFBC_SIZE frameSize = { 0, 0 };
    NVFBC_BOOL printStatusOnly = NVFBC_FALSE;

    NVFBC_TRACKING_TYPE trackingType = NVFBC_TRACKING_DEFAULT;
    char outputName[NVFBC_OUTPUT_NAME_LEN];
    uint32_t outputId = 0;

    void *libNVFBC = NULL, *libCUDA = NULL;
    PNVFBCCREATEINSTANCE NvFBCCreateInstance_ptr = NULL;
    NVFBC_API_FUNCTION_LIST pFn;

    CUcontext cuCtx;

    NVFBCSTATUS fbcStatus;
    NVFBC_BOOL fbcBool;

    NVFBC_SESSION_HANDLE fbcHandle;
    NVFBC_CREATE_HANDLE_PARAMS createHandleParams;
    NVFBC_GET_STATUS_PARAMS statusParams;
    NVFBC_CREATE_CAPTURE_SESSION_PARAMS createCaptureParams;
    NVFBC_DESTROY_CAPTURE_SESSION_PARAMS destroyCaptureParams;
    NVFBC_DESTROY_HANDLE_PARAMS destroyHandleParams;
    NVFBC_TOCUDA_SETUP_PARAMS setupParams;

    NVFBC_BUFFER_FORMAT bufferFormat = NVFBC_BUFFER_FORMAT_RGB;

    /*
     * Parse the command line.
     */
    while ((opt = getopt_long(argc, argv, "hgt:f:s:o:", longopts, NULL)) != -1) {
        switch (opt) {
            case 'g':
                printStatusOnly = NVFBC_TRUE;
                break;
            case 't':
                NvFBCUtilsParseTrackingType(optarg, &trackingType, outputName);
                break;
            case 'f':
                nFrames = (unsigned int) atoi(optarg);
                break;
            case 's':
                ret = sscanf(optarg, "%ux%u", &frameSize.w, &frameSize.h);
                if (ret != 2) {
                    fprintf(stderr, "Invalid size format: '%s'\n", optarg);
                    return EXIT_FAILURE;
                }
                break;
            case 'o':
                if (!strcasecmp(optarg, "rgb")) {
                    bufferFormat = NVFBC_BUFFER_FORMAT_RGB;
                } else if (!strcasecmp(optarg, "argb")) {
                    bufferFormat = NVFBC_BUFFER_FORMAT_ARGB;
                } else if (!strcasecmp(optarg, "nv12")) {
                    bufferFormat = NVFBC_BUFFER_FORMAT_NV12;
                } else if (!strcasecmp(optarg, "yuv444p")) {
                    bufferFormat = NVFBC_BUFFER_FORMAT_YUV444P;
                } else {
                    fprintf(stderr, "Unknown buffer format: '%s'\n", optarg);
                    return EXIT_FAILURE;
                }
                break;
            case 'h':
            default:
                usage(argv[0]);
                return EXIT_SUCCESS;
        }
    }

    NvFBCUtilsPrintVersions(APP_VERSION);

    /*
     * Dynamically load the NvFBC library.
     */
    libNVFBC = dlopen(LIB_NVFBC_NAME, RTLD_NOW);
    if (libNVFBC == NULL) {
        fprintf(stderr, "Unable to open '%s'\n", LIB_NVFBC_NAME);
        return EXIT_FAILURE;
    }

    fbcBool = cuda_load_library(libCUDA);
    if (fbcBool != NVFBC_TRUE) {
        return EXIT_FAILURE;
    }

    fbcBool = cuda_init(&cuCtx);
    if (fbcBool != NVFBC_TRUE) {
        return EXIT_FAILURE;
    }

    /*
     * Resolve the 'NvFBCCreateInstance' symbol that will allow us to get
     * the API function pointers.
     */
    NvFBCCreateInstance_ptr =
        (PNVFBCCREATEINSTANCE) dlsym(libNVFBC, "NvFBCCreateInstance");
    if (NvFBCCreateInstance_ptr == NULL) {
        fprintf(stderr, "Unable to resolve symbol 'NvFBCCreateInstance'\n");
        return EXIT_FAILURE;
    }

    /*
     * Create an NvFBC instance.
     *
     * API function pointers are accessible through pFn.
     */
    memset(&pFn, 0, sizeof(pFn));

    pFn.dwVersion = NVFBC_VERSION;

    fbcStatus = NvFBCCreateInstance_ptr(&pFn);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "Unable to create NvFBC instance (status: %d)\n",
                fbcStatus);
        return EXIT_FAILURE;
    }

    /*
     * Create a session handle that is used to identify the client.
     */
    memset(&createHandleParams, 0, sizeof(createHandleParams));

    createHandleParams.dwVersion = NVFBC_CREATE_HANDLE_PARAMS_VER;

    fbcStatus = pFn.nvFBCCreateHandle(&fbcHandle, &createHandleParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    /*
     * Get information about the state of the display driver.
     *
     * This call is optional but helps the application decide what it should
     * do.
     */
    memset(&statusParams, 0, sizeof(statusParams));

    statusParams.dwVersion = NVFBC_GET_STATUS_PARAMS_VER;

    fbcStatus = pFn.nvFBCGetStatus(fbcHandle, &statusParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    if (printStatusOnly) {
        NvFBCUtilsPrintStatus(&statusParams);
        return EXIT_SUCCESS;
    }

    if (statusParams.bCanCreateNow == NVFBC_FALSE) {
        fprintf(stderr, "It is not possible to create a capture session "
                        "on this system.\n");
        return EXIT_FAILURE;
    }

    if (trackingType == NVFBC_TRACKING_OUTPUT) {
        if (!statusParams.bXRandRAvailable) {
            fprintf(stderr, "The XRandR extension is not available.\n");
            fprintf(stderr, "It is therefore not possible to track an RandR output.\n");
            return EXIT_FAILURE;
        }

        outputId = NvFBCUtilsGetOutputId(statusParams.outputs,
                                         statusParams.dwOutputNum,
                                         outputName);
        if (outputId == 0) {
            fprintf(stderr, "RandR output '%s' not found.\n", outputName);
            return EXIT_FAILURE;
        }
    }

    /*
     * Create a capture session.
     */
    printf("Creating an asynchronous capture session of %u frames with 1 "
           "second internal between captures.\n", nFrames);

    memset(&createCaptureParams, 0, sizeof(createCaptureParams));

    createCaptureParams.dwVersion     = NVFBC_CREATE_CAPTURE_SESSION_PARAMS_VER;
    createCaptureParams.eCaptureType  = NVFBC_CAPTURE_SHARED_CUDA;
    createCaptureParams.bWithCursor   = NVFBC_TRUE;
    createCaptureParams.frameSize     = frameSize;
    createCaptureParams.eTrackingType = trackingType;

    if (trackingType == NVFBC_TRACKING_OUTPUT) {
        createCaptureParams.dwOutputId = outputId;
    }

    fbcStatus = pFn.nvFBCCreateCaptureSession(fbcHandle, &createCaptureParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    /*
     * Set up the capture session.
     */
    memset(&setupParams, 0, sizeof(setupParams));

    setupParams.dwVersion     = NVFBC_TOCUDA_SETUP_PARAMS_VER;
    setupParams.eBufferFormat = bufferFormat;

    fbcStatus = pFn.nvFBCToCudaSetUp(fbcHandle, &setupParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    /*
     * We are now ready to start grabbing frames.
     */
    for (i = 0; i < nFrames; i++) {
        static CUdeviceptr cuDevicePtr;
        static unsigned char *frame = NULL;
        static uint32_t lastByteSize = 0;

        char filename[64];

        int res;
        uint64_t t1, t2, t1_total, t2_total, t_delta, wait_time_ms;

        CUresult cuRes;

        NVFBC_TOCUDA_GRAB_FRAME_PARAMS grabParams;

        NVFBC_FRAME_GRAB_INFO frameInfo;

        t1 = NvFBCUtilsGetTimeInMillis();
        t1_total = t1;

        memset(&grabParams, 0, sizeof(grabParams));
        memset(&frameInfo, 0, sizeof(frameInfo));

        grabParams.dwVersion = NVFBC_TOCUDA_GRAB_FRAME_PARAMS_VER;

        /*
         * Use asynchronous calls.
         *
         * The application will not wait for a new frame to be ready.  It will
         * capture a frame that is already available.  This might result in
         * capturing several times the same frame.  This can be detected by
         * checking the frameInfo.bIsNewFrame structure member.
         */
        grabParams.dwFlags = NVFBC_TOCUDA_GRAB_FLAGS_NOWAIT;

        /*
         * This structure will contain information about the captured frame.
         */
        grabParams.pFrameGrabInfo = &frameInfo;

        /*
         * The frame will be mapped in video memory through this CUDA
         * device pointer.
         */
        grabParams.pCUDADeviceBuffer = &cuDevicePtr;

        /*
         * Capture a frame.
         */
        fbcStatus = pFn.nvFBCToCudaGrabFrame(fbcHandle, &grabParams);
        if (fbcStatus != NVFBC_SUCCESS) {
            fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
            return EXIT_FAILURE;
        }

        t2 = NvFBCUtilsGetTimeInMillis();

        /*
         * Allocate or re-allocate the destination buffer in system memory
         * when necessary.
         *
         * This is to handle change of resolution.
         */
        if (lastByteSize < frameInfo.dwByteSize) {
            frame = (unsigned char *)realloc(frame, frameInfo.dwByteSize);
            if (frame == NULL) {
                fprintf(stderr, "Unable to allocate system memory\n");
                return EXIT_FAILURE;
            }

            printf("Reallocated %u KB of system memory\n",
                   frameInfo.dwByteSize / 1024);

            lastByteSize = frameInfo.dwByteSize;
        }

        printf("%s id %u grabbed in %llu ms",
               (frameInfo.bIsNewFrame ? "New frame" : "Frame"),
               frameInfo.dwCurrentFrame,
               (unsigned long long) (t2 - t1));

        /*
         * Download frame from video memory to system memory.
         */
        t1 = NvFBCUtilsGetTimeInMillis();

        cuRes = cuMemcpyDtoH_v2_ptr((void *) frame, cuDevicePtr,
                                    frameInfo.dwByteSize);
        if (cuRes != CUDA_SUCCESS) {
            fprintf(stderr, "CUDA memcpy failure (result: %d)\n", cuRes);
            return EXIT_FAILURE;
        }

        t2 = NvFBCUtilsGetTimeInMillis();

        printf(", downloaded in %llu ms", (unsigned long long) (t2 - t1));

        /*
         * Convert RGB frame to BMP and save it on the disk.
         *
         * This operation can be quite slow.
         */
        t1 = NvFBCUtilsGetTimeInMillis();

        sprintf(filename, "frame%u.bmp", frameInfo.dwCurrentFrame);

        res = NvFBCUtilsSaveFrame(bufferFormat, filename, frame,
                                  frameInfo.dwWidth, frameInfo.dwHeight);
        if (res > 0) {
            fprintf(stderr, "Unable to save frame\n");
            return EXIT_FAILURE;
        }

        t2 = NvFBCUtilsGetTimeInMillis();
        t2_total = t2;

        printf(", saved in %llu ms", (unsigned long long) (t2 - t1));

        /*
         * Compute how much time to sleep before capturing the next frame.
         */
        t_delta = t2_total - t1_total;
        wait_time_ms = t_delta < 1000 ? 1000 - t_delta : 0;

        printf(", now sleeping for %llu ms\n",
               (unsigned long long) wait_time_ms);
        usleep(wait_time_ms * 1000);
    }

    /*
     * Destroy capture session, tear down resources.
     */
    memset(&destroyCaptureParams, 0, sizeof(destroyCaptureParams));

    destroyCaptureParams.dwVersion = NVFBC_DESTROY_CAPTURE_SESSION_PARAMS_VER;

    fbcStatus = pFn.nvFBCDestroyCaptureSession(fbcHandle, &destroyCaptureParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    /*
     * Destroy session handle, tear down more resources.
     */
    memset(&destroyHandleParams, 0, sizeof(destroyHandleParams));

    destroyHandleParams.dwVersion = NVFBC_DESTROY_HANDLE_PARAMS_VER;

    fbcStatus = pFn.nvFBCDestroyHandle(fbcHandle, &destroyHandleParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    return EXIT_SUCCESS;
}
