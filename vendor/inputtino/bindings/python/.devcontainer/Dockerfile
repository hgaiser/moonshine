FROM ubuntu:24.04

# Install uv
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/
ENV UV_LINK_MODE=copy
# Shell completion
RUN echo 'eval "$(uv generate-shell-completion bash)"' >> ~/.bashrc

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    cmake \
    clang-format \
    # clang-tidy \
    cpplint \
    cppcheck \
    # iwyu \
    libevdev-dev \
    make \
    pkg-config \
    git \
    && rm -rf /var/lib/apt/lists/*
