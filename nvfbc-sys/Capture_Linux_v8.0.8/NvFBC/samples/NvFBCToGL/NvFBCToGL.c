/*!
 * \brief
 * Demonstrates how to use NvFBC to grab frames to an OpenGL texture in video
 * memory.
 *
 * \file
 * This sample demonstrates the following features:
 * - Capture to video memory using OpenGL interop;
 * - Manage a GL context externally, and pass it to NvFBC;
 * - Disable automatic modeset recovery;
 * - Select an output (monitor) to track;
 * - Select and test buffer formats;
 *
 *
 * \copyright
 * Copyright (c) 2013-2016, NVIDIA CORPORATION. All rights reserved.
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

#include <NvFBC.h>
#include <GL/gl.h>
#include <GL/glx.h>
#include <X11/Xlib.h>

#include "NvFBCUtils.h"

#define APP_VERSION 1

#define LIB_NVFBC_NAME "libnvidia-fbc.so.1"

/*
 * Global variables
 */
Display *dpy            = None;
Pixmap pixmap           = None;
Window window           = None;
GLXContext glxCtx       = None;
GLXFBConfig glxFBConfig = None;
GLXPixmap glxPixmap     = None;
GLXWindow glxWindow     = None;

/*
 * OpenGL entry points
 */
static PFNGLGENFRAMEBUFFERSPROC glGenFramebuffers_ptr;
static PFNGLBINDFRAMEBUFFERPROC glBindFramebuffer_ptr;
static PFNGLFRAMEBUFFERTEXTURE2DPROC glFramebufferTexture2D_ptr;
static PFNGLBLITFRAMEBUFFERPROC glBlitFramebuffer_ptr;
static PFNGLDELETEFRAMEBUFFERSPROC glDeleteFramebuffers_ptr;

/*
 * Helper to resolve OpenGL entry points
 */
#define NVFBC_GLX_RESOLVE(type, proc)                                   \
    proc ## _ptr = (type) glXGetProcAddressARB((const GLubyte *)#proc); \
    if (proc ## _ptr == NULL) {                                         \
        printf("Unable to resolve symbol '%s'", #proc);                 \
        return NVFBC_FALSE;                                             \
    }

/**
 * Creates an OpenGL context.
 *
 * This context will then be passed to NvFBC for its internal use.
 *
 * \param [out] *glxCtx
 *   The created OpenGL context.
 * \param [out] *glxFbConfig
 *   The used framebuffer configuration.
 *
 * \return
 *   NVFBC_TRUE in case of success, NVFBC_FALSE otherwise.
 */
static NVFBC_BOOL gl_init(void)
{
    GLXFBConfig *fbConfigs;
    Bool res;
    int n;

    int attribs[] = {
        GLX_DRAWABLE_TYPE, GLX_PIXMAP_BIT | GLX_WINDOW_BIT,
        GLX_BIND_TO_TEXTURE_RGBA_EXT, 1,
        GLX_BIND_TO_TEXTURE_TARGETS_EXT, GLX_TEXTURE_2D_BIT_EXT,
        None
    };

    dpy = XOpenDisplay(NULL);
    if (dpy == None) {
        fprintf(stderr, "Unable to open display\n");
        return NVFBC_FALSE;
    }

    fbConfigs = glXChooseFBConfig(dpy, DefaultScreen(dpy), attribs, &n);
    if (!fbConfigs) {
        fprintf(stderr, "Unable to find FB configs\n");
        return NVFBC_FALSE;
    }

    glxCtx = glXCreateNewContext(dpy, fbConfigs[0], GLX_RGBA_TYPE, None, True);
    if (glxCtx == None) {
        fprintf(stderr, "Unable to create GL context\n");
        return NVFBC_FALSE;
    }

    pixmap = XCreatePixmap(dpy, XDefaultRootWindow(dpy), 1, 1, DisplayPlanes(dpy, XDefaultScreen(dpy)));
    if (pixmap == None) {
        fprintf(stderr, "Unable to create pixmap\n");
        return NVFBC_FALSE;
    }

    glxPixmap = glXCreatePixmap(dpy, fbConfigs[0], pixmap, NULL);
    if (glxPixmap == None) {
        fprintf(stderr, "Unable to create GLX pixmap\n");
        return NVFBC_FALSE;
    }

    res = glXMakeCurrent(dpy, glxPixmap, glxCtx);
    if (!res) {
        fprintf(stderr, "Unable to make context current\n");
        return NVFBC_FALSE;
    }

    glxFBConfig = fbConfigs[0];

    XFree(fbConfigs);

    NVFBC_GLX_RESOLVE(PFNGLGENFRAMEBUFFERSPROC, glGenFramebuffers);
    NVFBC_GLX_RESOLVE(PFNGLBINDFRAMEBUFFERPROC, glBindFramebuffer);
    NVFBC_GLX_RESOLVE(PFNGLFRAMEBUFFERTEXTURE2DPROC, glFramebufferTexture2D);
    NVFBC_GLX_RESOLVE(PFNGLBLITFRAMEBUFFERPROC, glBlitFramebuffer);
    NVFBC_GLX_RESOLVE(PFNGLDELETEFRAMEBUFFERSPROC, glDeleteFramebuffers);

    return NVFBC_TRUE;
}

/**
 * Creates X and GLX windows, then make the context current on the GLX window.
 *
 * \param [in] size
 *   Size of the window.
 *
 * \return
 *   NVFBC_TRUE in case of success, NVFBC_FALSE otherwise.
 */
static NVFBC_BOOL create_window(NVFBC_SIZE size)
{
    int screen;
    XVisualInfo *visualInfo;
    Bool xBool;
    Window root;
    Colormap colormap;
    XSetWindowAttributes attributes;
    Atom wm_delete_window;

    visualInfo = glXGetVisualFromFBConfig(dpy, glxFBConfig);
    if (!visualInfo) {
        fprintf(stderr, "Unable to retrieve X visual\n");
        return NVFBC_FALSE;
    }

    screen = DefaultScreen(dpy);
    root   = XRootWindow(dpy, screen);

    colormap = XCreateColormap(dpy, root, visualInfo->visual, AllocNone);
    if (colormap == None) {
        fprintf(stderr, "Unable to create colormap\n");
        return NVFBC_FALSE;
    }

    attributes.colormap         = colormap;
    attributes.event_mask       = StructureNotifyMask;
    attributes.background_pixel = 0xFFFFFFFF;

    window = XCreateWindow(dpy, root, 0, 0, size.w, size.h, 0, visualInfo->depth,
                           InputOutput, visualInfo->visual,
                           CWColormap | CWEventMask | CWBackPixel | CWBorderPixel,
                           &attributes);
    if (window == None) {
        fprintf(stderr, "Unable to create X window\n");
        return NVFBC_FALSE;
    }

    glxWindow = glXCreateWindow(dpy, glxFBConfig, window, NULL);
    if (glxWindow == None) {
        fprintf(stderr, "Unable to create GLX window\n");
        return NVFBC_FALSE;
    }

    xBool = glXMakeCurrent(dpy, glxWindow, glxCtx);
    if (!xBool) {
        fprintf(stderr, "Unable to make context current\n");
        return NVFBC_FALSE;
    }

    XMapWindow(dpy, window);
    XFlush(dpy);

    wm_delete_window = XInternAtom(dpy, "WM_DELETE_WINDOW", False);
    XSetWMProtocols(dpy, window, &wm_delete_window, 1);

    return NVFBC_TRUE;
}

/**
 * Destroys X and GLX windows, then make the context current on the dummy pixmap.
 */
static void destroy_window(void)
{
    glXMakeCurrent(dpy, glxPixmap, glxCtx);
    glXDestroyWindow(dpy, glxWindow);
    XDestroyWindow(dpy, window);
}

/**
 * Initializes the NvFBC library and creates an NvFBC instance.
 *
 * Creates and sets up a capture session to video memory.
 */
int main(int argc, char *argv[])
{
    void *libNVFBC = NULL;
    PNVFBCCREATEINSTANCE NvFBCCreateInstance_ptr = NULL;
    NVFBC_API_FUNCTION_LIST pFn;

    NVFBCSTATUS fbcStatus;
    NVFBC_BOOL fbcBool;
    NVFBC_BOOL done;

    NVFBC_SESSION_HANDLE fbcHandle;
    NVFBC_CREATE_HANDLE_PARAMS createHandleParams;

    NVFBC_DESTROY_HANDLE_PARAMS destroyHandleParams;

    NvFBCUtilsPrintVersions(APP_VERSION);

    /*
     * Dynamically load the NvFBC library.
     */
    libNVFBC = dlopen(LIB_NVFBC_NAME, RTLD_NOW);
    if (libNVFBC == NULL) {
        fprintf(stderr, "Unable to open '%s'\n", LIB_NVFBC_NAME);
        return EXIT_FAILURE;
    }

    /*
     * Initialize OpenGL.
     */
    fbcBool = gl_init();
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
     *
     * Request that the GL context is externally managed.
     */
    memset(&createHandleParams, 0, sizeof(createHandleParams));

    createHandleParams.dwVersion                 = NVFBC_CREATE_HANDLE_PARAMS_VER;
    createHandleParams.bExternallyManagedContext = NVFBC_TRUE;
    createHandleParams.glxCtx                    = glxCtx;
    createHandleParams.glxFBConfig               = glxFBConfig;

    fbcStatus = pFn.nvFBCCreateHandle(&fbcHandle, &createHandleParams);
    if (fbcStatus != NVFBC_SUCCESS) {
        fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
        return EXIT_FAILURE;
    }

    done = NVFBC_FALSE;

    do {
        NVFBC_GET_STATUS_PARAMS statusParams;
        NVFBC_CREATE_CAPTURE_SESSION_PARAMS createCaptureParams;
        NVFBC_TOGL_SETUP_PARAMS setupParams;
        NVFBC_DESTROY_CAPTURE_SESSION_PARAMS destroyCaptureParams;

        NVFBC_SIZE frameSize;

        GLenum glTexTarget;
        GLuint fbo;

        int i;

        /*
         * Retrieve the size of framebuffer.
         */
        memset(&statusParams, 0, sizeof(statusParams));

        statusParams.dwVersion = NVFBC_GET_STATUS_PARAMS_VER;

        fbcStatus = pFn.nvFBCGetStatus(fbcHandle, &statusParams);
        if (fbcStatus != NVFBC_SUCCESS) {
            fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
            done = NVFBC_TRUE;
            break;
        }

        if (statusParams.bCanCreateNow == NVFBC_FALSE) {
            fprintf(stderr, "It is not possible to create a capture session "
            "on this system.\n");
            done = NVFBC_TRUE;
            break;
        }

        /*
         * Capture frames that are half the size of the framebuffer.
         */
        frameSize.w = statusParams.screenSize.w / 2;
        frameSize.h = statusParams.screenSize.h / 2;
        printf("Screen size is %dx%d.\n",
               statusParams.screenSize.w, statusParams.screenSize.h);

        /*
         * Create a capture session.
         */
        memset(&createCaptureParams, 0, sizeof(createCaptureParams));

        createCaptureParams.dwVersion                   = NVFBC_CREATE_CAPTURE_SESSION_PARAMS_VER;
        createCaptureParams.eCaptureType                = NVFBC_CAPTURE_TO_GL;
        createCaptureParams.bWithCursor                 = NVFBC_TRUE;
        createCaptureParams.frameSize                   = frameSize;
        createCaptureParams.eTrackingType               = NVFBC_TRACKING_DEFAULT;

        fbcStatus = pFn.nvFBCCreateCaptureSession(fbcHandle, &createCaptureParams);
        if (fbcStatus != NVFBC_SUCCESS) {
            fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
            done = NVFBC_TRUE;
            break;
        }

        /*
         * Set up the capture session.
         */
        memset(&setupParams, 0, sizeof(setupParams));

        setupParams.dwVersion     = NVFBC_TOGL_SETUP_PARAMS_VER;
        setupParams.eBufferFormat = NVFBC_BUFFER_FORMAT_RGB;

        fbcStatus = pFn.nvFBCToGLSetUp(fbcHandle, &setupParams);
        if (fbcStatus != NVFBC_SUCCESS) {
            fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
            done = NVFBC_TRUE;
            break;
        }

        /*
         * Store relevant texture information.
         */
        glTexTarget = setupParams.dwTexTarget;

        /*
         * Setup X and GLX windows.
         */
        fbcBool = create_window(frameSize);
        if (!fbcBool) {
            done = NVFBC_TRUE;
            break;
        }
        printf("Created %dx%d window.\n", frameSize.w, frameSize.h);

        /*
         * Create FBO and attach the texture(s) that will hold the frames.
         */
        glGenFramebuffers_ptr(1, &fbo);
        glBindFramebuffer_ptr(GL_READ_FRAMEBUFFER, fbo);

        for (i = 0; i < NVFBC_TOGL_TEXTURES_MAX; i++) {
            GLuint texture = setupParams.dwTextures[i];

            if (texture == 0) {
                break;
            }

            glFramebufferTexture2D_ptr(GL_READ_FRAMEBUFFER,
                                       GL_COLOR_ATTACHMENT0 + i,
                                       glTexTarget, texture, 0);
        }

        /*
         * Start capturing frames.
         */
        printf("Capturing %dx%d frames...\n", frameSize.w, frameSize.h);
        while (1) {
            XEvent event;
            GLint glError;
            NVFBC_TOGL_GRAB_FRAME_PARAMS grabParams;

            memset(&grabParams, 0, sizeof(grabParams));

            grabParams.dwVersion = NVFBC_TOGL_GRAB_FRAME_PARAMS_VER;

            /*
             * Capture a frame.
             */
            fbcStatus = pFn.nvFBCToGLGrabFrame(fbcHandle, &grabParams);
            if (fbcStatus == NVFBC_ERR_MUST_RECREATE) {
                printf("Capture session must be recreated!\n");
                break;
            } else if (fbcStatus != NVFBC_SUCCESS) {
                fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
                done = NVFBC_TRUE;
                break;
            }

            /*
             * Handle X events.
             */
            while (XPending(dpy)) {
                XNextEvent(dpy, &event);

                switch (event.type) {
                    case ClientMessage:
                        printf("Window closed...\n");
                        done = NVFBC_TRUE;
                        break;
                    case ConfigureNotify:
                        if ((event.xconfigure.width != frameSize.w) ||
                            (event.xconfigure.height != frameSize.h)) {
                            printf("Window resize not supported!\n");
                            done = NVFBC_TRUE;
                        }
                        break;
                }
            }

            if (done) {
                break;
            }

            /*
             * Bind the framebuffer that we created earlier.
             *
             * Set the read buffer to the color attachment corresponding to
             * the texture holding the frame.  Keep in mind that dwTextureIndex
             * is an index in the setupParams.dwTextures array.
             *
             * Bind the default draw framebuffer which is the glxWindow we
             * made current, then blit the read buffer to the draw buffer.
             */
            glBindFramebuffer_ptr(GL_READ_FRAMEBUFFER, fbo);
            glReadBuffer(GL_COLOR_ATTACHMENT0 + grabParams.dwTextureIndex);
            glBindFramebuffer_ptr(GL_DRAW_FRAMEBUFFER, 0);
            glBlitFramebuffer_ptr(0, 0, frameSize.w, frameSize.h,
                                  0, frameSize.h, frameSize.w, 0,
                                  GL_COLOR_BUFFER_BIT, GL_NEAREST);

            glError = glGetError();
            if (glError != GL_NO_ERROR) {
                fprintf(stderr, "GL error: 0x%x\n", glError);
                done = NVFBC_TRUE;
                break;
            }

            glFinish();
        }

        printf("Destroying resources...\n");

        glBindFramebuffer_ptr(GL_READ_FRAMEBUFFER, 0);
        glDeleteFramebuffers_ptr(1, &fbo);

        destroy_window();

        /*
         * Destroy capture session.
         */
        memset(&destroyCaptureParams, 0, sizeof(destroyCaptureParams));

        destroyCaptureParams.dwVersion = NVFBC_DESTROY_CAPTURE_SESSION_PARAMS_VER;

        fbcStatus = pFn.nvFBCDestroyCaptureSession(fbcHandle, &destroyCaptureParams);
        if (fbcStatus != NVFBC_SUCCESS) {
            fprintf(stderr, "%s\n", pFn.nvFBCGetLastErrorStr(fbcHandle));
            break;
        }
    } while (!done);

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
