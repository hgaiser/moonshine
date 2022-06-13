################################################################################
# Copyright (c) 2013-2014, NVIDIA CORPORATION. All rights reserved.
#
# Permission is hereby granted, free of charge, to any person obtaining a
# copy of this software and associated documentation files (the "Software"),
# to deal in the Software without restriction, including without limitation
# the rights to use, copy, modify, merge, publish, distribute, sublicense,
# and/or sell copies of the Software, and to permit persons to whom the
# Software is furnished to do so, subject to the following conditions:
#
# The above copyright notice and this permission notice shall be included in
# all copies or substantial portions of the Software.
#
# THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
# IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
# FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
# THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
# LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
# FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
# DEALINGS IN THE SOFTWARE.
################################################################################

# This is used to detects architecture as well as provide overrides
OSUPPER = $(shell uname -s 2>/dev/null | tr "[:lower:]" "[:upper:]")
OSLOWER = $(shell uname -s 2>/dev/null | tr "[:upper:]" "[:lower:]")

OS_ARCH = $(shell uname -m)

# Detecting the location of the CUDA Toolkit if needed
CUDA_PATH ?= /usr/local/cuda
GCC ?= gcc
NVCC := $(CUDA_PATH)/bin/nvcc -ccbin $(GCC)

ifneq ($(filter x86_64,$(OS_ARCH)),)
        OS_SIZE = 64
else ifneq ($(filter i%86,$(OS_ARCH)),)
        OS_SIZE = 32
else
        OS_SIZE = 
endif

# This enables cross compilation from the developer
ifeq ($(i386),1)
        OS_SIZE = 32
        OS_ARCH = i386
endif
ifeq ($(i686),1)
        OS_SIZE = 32
        OS_ARCH = i686
endif
ifeq ($(x86_64),1)
        OS_SIZE = 64
        OS_ARCH = x86_64
endif
ifneq ($(OS_SIZE),)
        MFLAGS = -m$(OS_SIZE)
endif

##############################################################################
# function to generate a list of object files from their corresponding
# source files; example usage:
#
# OBJECTS = $(call BUILD_OBJECT_LIST,$(SOURCES))
##############################################################################

BUILD_OBJECT_LIST = $(notdir $(addsuffix .o,$(basename $(1))))

##############################################################################
# function to define a rule to build an object file; $(1) is the source
# file to compile, and $(2) is any other dependencies.
##############################################################################

define DEFINE_OBJECT_RULE
  $$(call BUILD_OBJECT_LIST,$(1)): $(1) $(2)
	$(CC) $(MFLAGS) $(CFLAGS) -c $$< -o $$@
endef

