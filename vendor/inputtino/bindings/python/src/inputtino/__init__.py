from importlib import metadata

from .base import DeviceDefinition, VirtualDevice
from .joypad import (
    ControllerButton,
    Joypad,
    PS5BatteryState,
    PS5Joypad,
    PS5MotionType,
    StickPosition,
    SwitchJoypad,
    XBoxOneJoypad,
)
from .keyboard import Keyboard, KeyCode
from .mouse import Mouse, MouseButton
from .pentablet import PenButtonType, PenTablet, PenToolType
from .touchscreen import TouchScreen
from .trackpad import Trackpad

__version__ = metadata.version("inputtino-python")


__all__ = [
    "DeviceDefinition",
    "Keyboard",
    "KeyCode",
    "Mouse",
    "MouseButton",
    "Joypad",
    "PS5BatteryState",
    "PS5Joypad",
    "PS5MotionType",
    "StickPosition",
    "SwitchJoypad",
    "XBoxOneJoypad",
    "ControllerButton",
    "TouchScreen",
    "Trackpad",
    "PenButtonType",
    "PenTablet",
    "PenToolType",
    "VirtualDevice",
]
