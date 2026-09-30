"""Keyboard input device implementation."""

from __future__ import annotations

import time
from enum import IntEnum

from typing_extensions import Self

from . import _core
from .base import DeviceDefinition, VirtualDevice

# Default device definition for keyboard
DEFAULT_KEYBOARD = DeviceDefinition(
    name="Wolf (virtual) keyboard",
    vendor_id=0xAB00,
    product_id=0xAB05,
    version=0xAB00,
)


class Keyboard(VirtualDevice):
    """Virtual keyboard input device.

    This class provides functionality to simulate keyboard input.
    The device appears as a real keyboard input device to the system.
    Key codes used are Win32 Virtual Key (VK) codes.

    Example:
        >>> from inputtino.keyboard import Keyboard, KeyCode
        >>> kb = Keyboard()
        >>> kb.press(KeyCode.CTRL)
        >>> kb.press(KeyCode.C)
        >>> kb.release(KeyCode.C)
        >>> kb.release(KeyCode.CTRL)
    """

    def __init__(
        self,
        device_def: DeviceDefinition = DEFAULT_KEYBOARD,
        millis_repress_key: int = 50,
    ) -> None:
        """Initialize the keyboard device with optional custom device
        definition.

        Args:
            device_def: Optional device definition for customizing the virtual device properties
            millis_repress_key: Time in milliseconds between key press repeats (default: 50)

        Raises:
            RuntimeError: If device creation fails
        """
        self._keyboard = _core.Keyboard.create(device_def.to_core(), millis_repress_key)
        super().__init__(self._keyboard)

    def press(self, key_code: int) -> None:
        """Press a keyboard key.

        Args:
            key_code: Win32 Virtual Key code of the key to press
        """
        self._keyboard.press(key_code)

    def release(self, key_code: int) -> None:
        """Release a keyboard key.

        Args:
            key_code: Win32 Virtual Key code of the key to release
        """
        self._keyboard.release(key_code)

    def type(self, key_code: int, duration: float = 0.1) -> None:
        """Press and release a key with specified duration.

        Args:
            key_code: Win32 Virtual Key code of the key
            duration: Time in seconds to hold the key (default: 0.1)
        """
        try:
            self.press(key_code)
            if duration > 0:
                time.sleep(duration)
        finally:
            self.release(key_code)


class KeyCode(IntEnum):
    """KeyCode mapping for virtual key codes.

    This class provides a mapping between human-readable key names and
    their corresponding virtual key codes used by the input system.

    Key codes are referred from
    <https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes>
    """

    # Standard Keys
    BACKSPACE = 0x08
    TAB = 0x09
    CLEAR = 0x0C
    ENTER = 0x0D
    SHIFT = 0x10
    CTRL = 0x11
    ALT = 0x12
    PAUSE = 0x13
    CAPS_LOCK = 0x14
    ESC = 0x1B
    SPACE = 0x20
    PAGE_UP = 0x21
    PAGE_DOWN = 0x22
    END = 0x23
    HOME = 0x24
    LEFT = 0x25
    UP = 0x26
    RIGHT = 0x27
    DOWN = 0x28
    PRINTSCREEN = 0x2C
    INSERT = 0x2D
    DELETE = 0x2E

    # Numbers
    KEY_0 = 0x30
    KEY_1 = 0x31
    KEY_2 = 0x32
    KEY_3 = 0x33
    KEY_4 = 0x34
    KEY_5 = 0x35
    KEY_6 = 0x36
    KEY_7 = 0x37
    KEY_8 = 0x38
    KEY_9 = 0x39

    # Letters
    A = 0x41
    B = 0x42
    C = 0x43
    D = 0x44
    E = 0x45
    F = 0x46
    G = 0x47
    H = 0x48
    I = 0x49
    J = 0x4A
    K = 0x4B
    L = 0x4C
    M = 0x4D
    N = 0x4E
    O = 0x4F
    P = 0x50
    Q = 0x51
    R = 0x52
    S = 0x53
    T = 0x54
    U = 0x55
    V = 0x56
    W = 0x57
    X = 0x58
    Y = 0x59
    Z = 0x5A

    # Windows Keys
    LEFT_WIN = 0x5B
    RIGHT_WIN = 0x5C
    APP = 0x5D

    # Numpad
    NUMPAD_0 = 0x60
    NUMPAD_1 = 0x61
    NUMPAD_2 = 0x62
    NUMPAD_3 = 0x63
    NUMPAD_4 = 0x64
    NUMPAD_5 = 0x65
    NUMPAD_6 = 0x66
    NUMPAD_7 = 0x67
    NUMPAD_8 = 0x68
    NUMPAD_9 = 0x69
    MULTIPLY = 0x6A
    ADD = 0x6B
    SUBTRACT = 0x6D
    DECIMAL = 0x6E
    DIVIDE = 0x6F

    # Function Keys
    F1 = 0x70
    F2 = 0x71
    F3 = 0x72
    F4 = 0x73
    F5 = 0x74
    F6 = 0x75
    F7 = 0x76
    F8 = 0x77
    F9 = 0x78
    F10 = 0x79
    F11 = 0x7A
    F12 = 0x7B
    F13 = 0x7C
    F14 = 0x7D
    F15 = 0x7E
    F16 = 0x7F
    F17 = 0x80
    F18 = 0x81
    F19 = 0x82
    F20 = 0x83
    F21 = 0x84
    F22 = 0x85
    F23 = 0x86
    F24 = 0x87

    # Lock Keys
    NUM_LOCK = 0x90
    SCROLL_LOCK = 0x91

    # Left/Right Keys
    LEFT_SHIFT = 0xA0
    RIGHT_SHIFT = 0xA1
    LEFT_CONTROL = 0xA2
    RIGHT_CONTROL = 0xA3
    LEFT_ALT = 0xA4
    RIGHT_ALT = 0xA5

    # Media Keys
    VOLUME_MUTE = 0xAD
    VOLUME_DOWN = 0xAE
    VOLUME_UP = 0xAF
    MEDIA_NEXT = 0xB0
    MEDIA_PREV = 0xB1
    MEDIA_STOP = 0xB2
    MEDIA_PLAY_PAUSE = 0xB3

    # OEM Keys
    SEMICOLON = 0xBA  # ;:
    PLUS = 0xBB  # =+
    COMMA = 0xBC  # ,<
    MINUS = 0xBD  # -_
    PERIOD = 0xBE  # .>
    SLASH = 0xBF  # /?
    TILDE = 0xC0  # `~
    OPEN_BRACKET = 0xDB  # [{
    BACKSLASH = 0xDC  # \|
    CLOSE_BRACKET = 0xDD  # ]}
    QUOTE = 0xDE  # '"

    @classmethod
    def from_str(cls, string: str) -> Self:
        """Retrieves key code from string.

        Args:
            string: Key name.

        Returns:
            KeyCode member.

        Raises:
            KeyError: If provided name is not in KeyCode enum members.
        """
        return cls[string.upper()]
