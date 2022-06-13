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

#ifndef _UTILS_H_
#define _UTILS_H_

#include <stdint.h>

#include <NvFBC.h>

#ifdef __cplusplus
extern "C" {
#endif

/*!
 * Converts a frame to BMP then saves it on the hard drive.
 *
 * This function figures out which util function to call from to the passed
 * buffer format.
 *
 * \param [in] format
 *   Buffer format of the data.
 * \param [in] data
 *   Raw data, obtained from capturing a frame using NvFBC.
 * \param [in] width
 *   Width of the frame.
 * \param [in] height
 *   Height of the frame.
 *
 * \return
 *   0 in case of success, 1 otherwise.
 */
int NvFBCUtilsSaveFrame(NVFBC_BUFFER_FORMAT format,
                        const char *filename,
                        unsigned char *data,
                        const int width,
                        const int height);

/*!
 * Converts an R diffmap to a BMP then saves it on the hard drive.
 *
 * \param [in] filename
 *   Name of the file to create that will contain the image.
 * \param [in] data
 *   Raw data, obtained from generating a diffmap using NvFBC.
 * \param [in] width
 *   Width of the frame.
 * \param [in] height
 *   Height of the frame.
 *
 * \return
 *   0 in case of success, 1 otherwise.
 */
int NvFBCUtilsSaveDiffMap(const char *filename,
                          unsigned char *data,
                          const int width,
                          const int height);

/*!
 * Returns the current time in microseconds.
 */
uint64_t NvFBCUtilsGetTimeInMicros(void);

/*!
 * Returns the current time in milliseconds.
 */
uint64_t NvFBCUtilsGetTimeInMillis(void);

/*!
 * Print information returned by an NvFBCGetStatus() API call.
 */
void NvFBCUtilsPrintStatus(NVFBC_GET_STATUS_PARAMS *status);

/*!
 * Parses optarg and set trackingType and outputName accordingly.
 *
 * \param [in] optarg
 *   String to parse, should be 'default', 'screen' or the name of an RandR
 *   output.
 *
 * \param [out] trackingType
 *   Parsed tracking type.
 * \param [out] outputName
 *   Parsed output name, if any.
 */
void NvFBCUtilsParseTrackingType(const char* optarg,
                                 NVFBC_TRACKING_TYPE *trackingType,
                                 char* outputName);

/*!
 * Finds an RandR output id from its name.
 *
 * \param [in] outputs
 *   List of RandR outputs returned by the NvFBCGetStatus() API call.
 * \param [in] outputNum
 *   Number of returned outputs.
 * \param [in] outputName
 *   Name of the output to look for.
 *
 * \return
 *   The output id if found, 0 otherwise.
 */
uint32_t NvFBCUtilsGetOutputId(NVFBC_RANDR_OUTPUT_INFO *outputs,
                               uint32_t outputNum,
                               const char* outputName);

/*!
 * Prints the application and the local NvFBC API versions.
 *
 * \param [in] appVersion
 *   Application version.
 */
void NvFBCUtilsPrintVersions(const unsigned int appVersion);

#ifdef __cplusplus
}
#endif

#endif // _UTILS_H_
