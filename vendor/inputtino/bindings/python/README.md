[![Python 3.10+](https://img.shields.io/badge/python-3.10+-blue.svg)](https://www.python.org/downloads/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Document Style](https://img.shields.io/badge/%20docstyle-google-3666d6.svg)](https://google.github.io/styleguide/pyguide.html#s3.8-comments-and-docstrings)

# inputtino-python

Python bindings for the [inputtino](../../README.md) library, providing virtual input device control for Linux.

## Features

- Mouse, keyboard, touchscreen, trackpad and pen tablet input simulation
- Full gamepad support (Xbox One, PS5, Nintendo Switch)
- Complete type hints and documentation
- Pythonic interface with easy-to-use API

## Requirements

- Python 3.10 or higher
- Linux system with uinput support
- Root privileges or appropriate permissions for `/dev/uinput` and `/dev/uhid` (for PS5Joypad)

## Installation

1. Install system dependencies:

**Ubuntu / Debian**:

```sh
sudo apt install git cmake build-essential pkg-config libevdev-dev
```

**Arch Linux**:

```sh
sudo pacman -Sy git cmake base-devel pkgconf libevdev
```

**Fedora**:

```sh
sudo dnf install git cmake gcc gcc-c++ make pkgconf-pkg-config libevdev-devel
# Optional python3-devel
```

2. Install inputtino-python:

```bash
pip install "git+https://github.com/games-on-whales/inputtino.git#subdirectory=bindings/python&branch=stable"
```

This package requires root privileges or appropriate permissions for /dev/uinput to work properly.

## Quick Start

### Mouse Control

```python
from inputtino import Mouse, MouseButton

# Initialize mouse device
mouse = Mouse()

# Move mouse
mouse.move(100, 50)  # Move right 100, down 50
mouse.move_abs(500, 300, 1920, 1080)  # Move to absolute position

# Click operations
mouse.click(MouseButton.LEFT)
mouse.click(MouseButton.RIGHT, duration=0.5)  # Hold for 0.5 seconds

# Scrolling
mouse.scroll_vertical(120)  # Scroll up
mouse.scroll_horizontal(-120)  # Scroll left
```

### Keyboard Control

```python
from inputtino import Keyboard, KeyCode

# Initialize keyboard device
keyboard = Keyboard()

# Type individual keys
keyboard.press(KeyCode.A)
keyboard.release(KeyCode.A)

# Convenient typing with auto-release
keyboard.type(KeyCode.ENTER)
keyboard.type(KeyCode.from_str("space"))  # From string representation
```

### PS5 Controller

```python
from inputtino import PS5Joypad, ControllerButton, StickPosition

# Initialize PS5 controller
joypad = PS5Joypad()

# Set button states
joypad.set_pressed_buttons(ControllerButton.X | ControllerButton.DPAD_UP)

# Move analog sticks
joypad.set_stick(StickPosition.LS, x=100, y=200)

# Set trigger pressure
joypad.set_triggers(left=32000, right=16000)

# Handle rumble feedback
joypad.set_on_rumble(lambda low, high: print(f"Rumble: {low}, {high}"))
```

## Docker Container Integration

When using inputtino-python in Docker environments, add these required configurations:

### Required Docker Settings

```yaml
# docker-compose.yml example
services:
  your-service:
    volumes:
      - /dev/input:/dev/input:rw  # For input device access
      - /dev/uhid:/dev/uhid       # For PS5 controller support
    devices:
      - /dev/uinput              # For virtual device creation
    privileged: true             # Required for device access
```

Or when running Docker:

```bash
docker run -it --rm \
  -v /dev/input:/dev/input:rw \
  -v /dev/uhid:/dev/uhid \
  --device /dev/uinput \
  --privileged \
  your-image-name
```

## Development

Simply open this directory in your favorite IDE and use [devcontainers](https://code.visualstudio.com/docs/devcontainers/containers)!

Or for a more manual approach, follow these steps:

### Prerequisites

- Python 3.10 or higher
- Git

### Setting Up

1. If using `uv` (recommended):
```bash
git clone https://github.com/your-username/inputtino.git
cd inputtino/bindings/python
uv sync
```

2. If using `pip`:
```bash
git clone https://github.com/your-username/inputtino.git
cd inputtino/bindings/python
python -m venv ./venv
source ./venv/bin/activate
pip install .
```

### Running Tests

```bash
uv run pytest -v --log-level INFO --cov -m "not practical"
```

## Coding Standards

### Python Code

- Use type hints for all function arguments and return values
- Prefer built-in `list` and `tuple` over `typing.List` and `typing.Tuple`
- All public classes and methods must have docstrings (Google Style)
- Follow PEP 8 guidelines
- Keep functions focused and single-purpose
- Prioritize code readability

Example docstring:

```python
def function_name(param1: str, param2: list[int]) -> bool:
    """Short description of function.

    Args:
        param1: Description of first parameter
        param2: Description of second parameter

    Returns:
        Description of return value

    Raises:
        ValueError: Description of when this is raised
    """
    pass
```

### C++ Code

- Use pybind11 for Python bindings
- Run `make includes` to see the include directories path for your IDE setup
- After modifying C++ binding code, always run `make stubs` to regenerate Python stubs
- Follow modern C++ practices
- Keep binding code simple and focused on Python integration
- Document any platform-specific considerations
