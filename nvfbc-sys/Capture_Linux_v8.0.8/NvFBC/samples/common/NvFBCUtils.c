/*
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
#define _POSIX_C_SOURCE 199309L

#include <stdlib.h>
#include <stdio.h>
#include <stddef.h>
#include <string.h>
#include <strings.h>
#include <time.h>
#include <sys/time.h>

#include "NvFBCUtils.h"

#define BITMAP_ROW_SIZE(width) (((width * Bpp) + 3) & ~3)
#define BITMAP_INDEX(x, y, rowSize) ((y * rowSize) + (x * Bpp))

typedef struct __attribute__((packed))
{
    uint8_t blue;
    uint8_t green;
    uint8_t red;
} BitmapPixel;

typedef struct __attribute__((packed))
{
    uint8_t alpha;
    uint8_t red;
    uint8_t green;
    uint8_t blue;
} ARGBPixel;

typedef struct __attribute__((packed))
{
    uint8_t red;
    uint8_t green;
    uint8_t blue;
    uint8_t alpha;
} RGBAPixel;

typedef struct __attribute__((packed))
{
    uint8_t blue;
    uint8_t green;
    uint8_t red;
    uint8_t alpha;
} BGRAPixel;

typedef struct __attribute__((packed))
{
    uint8_t red;
    uint8_t green;
    uint8_t blue;
} RGBPixel;

typedef struct __attribute__((packed))
{
    uint8_t red;
} RPixel;

// Bytes per pixel
static int Bpp = sizeof(BitmapPixel);

static int NvFBCUtilsSaveBitmap(const char *filename,
                                const unsigned char *data,
                                const int width,
                                const int height)
{
    struct __attribute__((packed))
    {
        uint16_t type;
        uint32_t size;
        uint16_t reserved1;
        uint16_t reserved2;
        uint32_t off_bits;
    } fileHeader;

    struct __attribute__((packed))
    {
        uint32_t size;
        int32_t  width;
        int32_t  height;
        uint16_t planes;
        uint16_t bit_count;
        uint32_t compression;
        uint32_t size_image;
        int32_t  x_pels_per_meter;
        int32_t  y_pels_per_meter;
        uint32_t clr_used;
        uint32_t clr_important;
    } infoHeader;

    int size;
    FILE *fd = NULL;

    if (data == NULL) {
        fprintf(stderr, "%s: There is no data to write\n", __FUNCTION__);
        return 1;
    }

    fd = fopen(filename, "wb");
    if (fd == NULL) {
        fprintf(stderr, "%s: Unable to open file '%s'\n", __FUNCTION__, filename);
        return 1;
    }

    size = BITMAP_ROW_SIZE(width) * height;

    fileHeader.type     = 0x4D42;
    fileHeader.size     = sizeof(fileHeader) + sizeof(infoHeader) + size;
    fileHeader.off_bits = sizeof(fileHeader) + sizeof(infoHeader);

    infoHeader.size             = sizeof(infoHeader);
    infoHeader.width            = width;
    infoHeader.height           = height;
    infoHeader.planes           = 1;
    infoHeader.bit_count        = Bpp * 8;
    infoHeader.compression      = 0;
    infoHeader.size_image       = 0;
    infoHeader.x_pels_per_meter = 0;
    infoHeader.y_pels_per_meter = 0;
    infoHeader.clr_used         = 0;
    infoHeader.clr_important    = 0;

    fwrite((unsigned char *) &fileHeader, sizeof(fileHeader), 1, fd);
    fwrite((unsigned char *) &infoHeader, sizeof(infoHeader), 1, fd);
    fwrite(data, size, 1, fd);

    fclose(fd);

    return 0;
}

static int NvFBCUtilsSaveRGBFormats(NVFBC_BUFFER_FORMAT format,
                                    const char *filename,
                                    unsigned char *data,
                                    const int width,
                                    const int height)
{
    int result = 1;

    int row, col;
    ARGBPixel *argbInput = (ARGBPixel *)data;
    RGBAPixel *rgbaInput = (RGBAPixel *)data;
    BGRAPixel *bgraInput = (BGRAPixel *)data;
    RGBPixel *rgbInput = (RGBPixel *)data;
    size_t rowSize = BITMAP_ROW_SIZE(width);
    size_t size = rowSize * height;
    unsigned char *output = malloc(size);
    if (!output) {
        return result;
    }

    // Pad bytes need to be set to zero, it's easier to just set the entire chunk of memory
    memset(output, 0, size);

    for (row = 0; row < height; ++row) {
        for (col = 0; col < width; ++col) {
            uint8_t r, g, b;

            // In a bitmap (0,0) is at the bottom left, in the frame buffer it is the top left.
            int outputIdx = BITMAP_INDEX(col, row, rowSize);
            int inputIdx = ((height - row - 1) * width) + col;

            switch (format) {
                case NVFBC_BUFFER_FORMAT_ARGB:
                    r = argbInput[inputIdx].red;
                    g = argbInput[inputIdx].green;
                    b = argbInput[inputIdx].blue;
                    break;
                case NVFBC_BUFFER_FORMAT_RGBA:
                    r = rgbaInput[inputIdx].red;
                    g = rgbaInput[inputIdx].green;
                    b = rgbaInput[inputIdx].blue;
                    break;
                case NVFBC_BUFFER_FORMAT_BGRA:
                    r = bgraInput[inputIdx].red;
                    g = bgraInput[inputIdx].green;
                    b = bgraInput[inputIdx].blue;
                    break;
                case NVFBC_BUFFER_FORMAT_RGB:
                    r = rgbInput[inputIdx].red;
                    g = rgbInput[inputIdx].green;
                    b = rgbInput[inputIdx].blue;
                    break;
                default:
                    return 1;
            }

            BitmapPixel *pixelOut = (BitmapPixel *)&output[outputIdx];
            pixelOut->red   = r;
            pixelOut->green = g;
            pixelOut->blue  = b;
        }
    }

    result = NvFBCUtilsSaveBitmap(filename, output, width, height);

    free(output);

    return result;
}

int NvFBCUtilsSaveDiffMap(const char *filename,
                          unsigned char *data,
                          const int width,
                          const int height)
{
    int result = 1;

    int row, col;
    RPixel *input = (RPixel *)data;
    size_t rowSize = BITMAP_ROW_SIZE(width);
    size_t size = rowSize * height;
    unsigned char *output = malloc(size);
    if (!output) {
        return result;
    }

    // Pad bytes need to be set to zero, it's easier to just set the entire chunk of memory
    memset(output, 0, size);

    for (row = 0; row < height; ++row)
    {
        for (col = 0; col < width; ++col)
        {
            // In a bitmap (0,0) is at the bottom left, in the frame buffer it is the top left.
            int outputIdx = BITMAP_INDEX(col, row, rowSize);
            int inputIdx = ((height - row - 1) * width) + col;

            BitmapPixel *pixelOut = (BitmapPixel *)&output[outputIdx];
            pixelOut->red   = input[inputIdx].red;
            pixelOut->green = input[inputIdx].red;
            pixelOut->blue  = input[inputIdx].red;
        }
    }

    result = NvFBCUtilsSaveBitmap(filename, output, width, height);

    free(output);

    return result;
}

static int NvFBCUtilsSaveYUVPlanar(NVFBC_BUFFER_FORMAT format,
                                   const char *fileName,
                                   unsigned char *data,
                                   const int width,
                                   const int height)
{
    int result = 1;

    int row, col;
    int uv_width;
    int uv_height;

    switch (format) {
        case NVFBC_BUFFER_FORMAT_NV12:
            uv_width = width / 2;
            uv_height = height / 2;
            break;
        case NVFBC_BUFFER_FORMAT_YUV444P:
            uv_width = width;
            uv_height = height;
            break;
        default:
            return 1;
    }

    char *fileNameExt = malloc(strlen(fileName) + 1);

    size_t lumaRowSize = BITMAP_ROW_SIZE(width);
    size_t lumaSize = lumaRowSize * height;
    size_t chromRowSize = BITMAP_ROW_SIZE(uv_width);
    size_t chromSize = chromRowSize * uv_height;

    unsigned char *luma = malloc(lumaSize);
    unsigned char *chrom = malloc(chromSize);

    if (!fileNameExt || !luma || !chrom) {
        return result;
    }

    memset(luma, 0, lumaSize);
    memset(chrom, 0, chromSize);

    for (row = 0; row < height; row++) {
        for (col = 0; col < width; col++) {
            int outputIdx = BITMAP_INDEX(col, row, lumaRowSize);
            int inputIdx = ((height - row - 1) * width) + col;

            BitmapPixel *pixelOut = (BitmapPixel *)&luma[outputIdx];
            pixelOut->red = data[inputIdx];
            pixelOut->green = data[inputIdx];
            pixelOut->blue = data[inputIdx];
        }
    }

    data += width * height;
    sprintf(fileNameExt, "%s.Y", fileName);

    result = NvFBCUtilsSaveBitmap(fileNameExt, luma, width, height);
    if (result) {
        goto done;
    }

    for (row = 0; row < uv_height; row++) {
        for (col = 0; col < uv_width; col++) {
            int outputIdx = BITMAP_INDEX(col, row, chromRowSize);
            int inputIdx = ((uv_height - row - 1) * uv_width) + col;

            BitmapPixel *pixelOut = (BitmapPixel *)&chrom[outputIdx];
            pixelOut->red = data[inputIdx];
            pixelOut->green = 255 - data[inputIdx];
            pixelOut->blue = 0;
        }
    }

    data += uv_width * uv_height;
    sprintf(fileNameExt, "%s.U", fileName);

    result = NvFBCUtilsSaveBitmap(fileNameExt, chrom, uv_width, uv_height);
    if (result) {
        goto done;
    }

    for (row = 0; row < uv_height; row++) {
        for (col = 0; col < uv_width; col++) {
            int outputIdx = BITMAP_INDEX(col, row, chromRowSize);
            int inputIdx = ((uv_height - row - 1) * uv_width) + col;

            BitmapPixel *pixelOut = (BitmapPixel *)&chrom[outputIdx];
            pixelOut->red = 0;
            pixelOut->green = 255 - data[inputIdx];
            pixelOut->blue = data[inputIdx];
        }
    }

    data += uv_width * uv_height;
    sprintf(fileNameExt, "%s.V", fileName);

    result = NvFBCUtilsSaveBitmap(fileNameExt, chrom, uv_width, uv_height);

done:
    free(fileNameExt);
    free(luma);
    free(chrom);

    return result;
}

int NvFBCUtilsSaveFrame(NVFBC_BUFFER_FORMAT format,
                        const char *filename,
                        unsigned char *data,
                        const int width,
                        const int height)
{
    switch (format) {
        case NVFBC_BUFFER_FORMAT_ARGB:
        case NVFBC_BUFFER_FORMAT_RGBA:
        case NVFBC_BUFFER_FORMAT_BGRA:
        case NVFBC_BUFFER_FORMAT_RGB:
            return NvFBCUtilsSaveRGBFormats(format, filename, data, width, height);
        case NVFBC_BUFFER_FORMAT_NV12:
        case NVFBC_BUFFER_FORMAT_YUV444P:
            return NvFBCUtilsSaveYUVPlanar(format, filename, data, width, height);
        default:
            fprintf(stderr, "%s: Unknown buffer format\n", __FUNCTION__);
            return 1;
    }
}

uint64_t NvFBCUtilsGetTimeInMicros()
{
    struct timeval tv;

    gettimeofday(&tv, NULL);

    return (uint64_t)tv.tv_sec * 1000000ULL + (uint64_t)tv.tv_usec;
}

uint64_t NvFBCUtilsGetTimeInMillis()
{
    return NvFBCUtilsGetTimeInMicros() / 1000;
}

void NvFBCUtilsPrintStatus(NVFBC_GET_STATUS_PARAMS *status)
{
    if (status == NULL) {
        return;
    }

    printf("Status:\n");
    printf("- NvFBC library API version: %u.%u\n",
           status->dwNvFBCVersion >> 8 & 0xf,
           status->dwNvFBCVersion & 0xf);
    printf("- This system supports FBC: %s\n",
           status->bIsCapturePossible ? "Yes" : "No");
    printf("- Curently capturing: %s\n",
           status->bCurrentlyCapturing ? "Yes" : "No");
    printf("- Can create an FBC instance: %s\n",
           status->bCanCreateNow ? "Yes" : "No");
    printf("- X screen (framebuffer) size: %ux%u\n",
           status->screenSize.w, status->screenSize.h);
    printf("- XrandR extension available: %s\n",
           status->bXRandRAvailable ? "Yes" : "No");

    if (status->bXRandRAvailable) {
        int i;

        printf("- Connected RandR outputs with CRTC:\n");

        for (i = 0; i < status->dwOutputNum; i++) {
            NVFBC_RANDR_OUTPUT_INFO output;

            output = status->outputs[i];

            printf("  * '%s' (id: 0x%x), CRTC: %ux%u+%u+%u\n",
                   output.name, output.dwId,
                   output.trackedBox.w, output.trackedBox.h,
                   output.trackedBox.x, output.trackedBox.y);
        }

    }
}

void NvFBCUtilsParseTrackingType(const char* optarg,
                                 NVFBC_TRACKING_TYPE *trackingType,
                                 char* outputName)
{
    if ((trackingType == NULL) || (outputName == NULL)) {
        return;
    }

    if (!strcasecmp(optarg, "default")) {
        *trackingType = NVFBC_TRACKING_DEFAULT;
    } else if (!strcasecmp(optarg, "screen")) {
        *trackingType = NVFBC_TRACKING_SCREEN;
    } else {
        *trackingType = NVFBC_TRACKING_OUTPUT;
        strncpy(outputName, optarg, NVFBC_OUTPUT_NAME_LEN);
    }
}

uint32_t NvFBCUtilsGetOutputId(NVFBC_RANDR_OUTPUT_INFO *outputs,
                               uint32_t outputNum,
                               const char* outputName)
{
    int i;
    uint32_t outputId = 0;

    if ((outputs == NULL) || (outputName == NULL)) {
        return 0;
    }

    for (i = 0; i < outputNum; i++) {
        if (!strcasecmp(outputs[i].name, outputName)) {
            outputId = outputs[i].dwId;
            break;
        }
    }

    return outputId;
}

void NvFBCUtilsPrintVersions(const unsigned int appVersion)
{
    printf("Application version: %u\n", appVersion);
    printf("NvFBC API version: %u.%u\n", NVFBC_VERSION_MAJOR, NVFBC_VERSION_MINOR);
    printf("\n");
}
