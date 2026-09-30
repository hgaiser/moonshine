"""Provides joypad device implementations."""

from collections.abc import Callable
from enum import IntFlag

from . import _core
from ._core import (
    ControllerButton as CoreButton,
    PS5BatteryState,
    PS5MotionType,
    StickPosition,
)
from .base import DeviceDefinition, VirtualDevice


class ControllerButton(IntFlag):
    """Button flags for controller input."""

    DPAD_UP = CoreButton.DPAD_UP.value
    DPAD_DOWN = CoreButton.DPAD_DOWN.value
    DPAD_LEFT = CoreButton.DPAD_LEFT.value
    DPAD_RIGHT = CoreButton.DPAD_RIGHT.value

    START = CoreButton.START.value
    BACK = CoreButton.BACK.value
    HOME = CoreButton.HOME.value

    LEFT_STICK = CoreButton.LEFT_STICK.value
    RIGHT_STICK = CoreButton.RIGHT_STICK.value
    LEFT_BUTTON = CoreButton.LEFT_BUTTON.value
    RIGHT_BUTTON = CoreButton.RIGHT_BUTTON.value

    SPECIAL = CoreButton.SPECIAL_FLAG.value
    PADDLE1 = CoreButton.PADDLE1_FLAG.value
    PADDLE2 = CoreButton.PADDLE2_FLAG.value
    PADDLE3 = CoreButton.PADDLE3_FLAG.value
    PADDLE4 = CoreButton.PADDLE4_FLAG.value
    TOUCHPAD = CoreButton.TOUCHPAD_FLAG.value
    MISC = CoreButton.MISC_FLAG.value

    A = CoreButton.A.value
    B = CoreButton.B.value
    X = CoreButton.X.value
    Y = CoreButton.Y.value


class Joypad(VirtualDevice):
    """Base class for all joypad implementations."""

    def __init__(self, device_def: DeviceDefinition, joypad: _core.Joypad) -> None:
        """Initialize joypad.

        Args:
            device_def: Device definition for the joypad
            joypad: Core joypad instance
        """
        super().__init__(joypad)
        self._device_def = device_def
        self._joypad = joypad

    def set_pressed_buttons(self, buttons: ControllerButton) -> None:
        """Set currently pressed buttons.

        Args:
            buttons: Button flags indicating which buttons are pressed
        """
        self._joypad.set_pressed_buttons(int(buttons))

    def set_triggers(self, left: int, right: int) -> None:
        """Set trigger values.

        Args:
            left: Left trigger value (0-65535)
            right: Right trigger value (0-65535)
        """
        self._joypad.set_triggers(left, right)

    def set_stick(self, stick: StickPosition, x: int, y: int) -> None:
        """Set stick position.

        Args:
            stick: Which stick to move (LS or RS)
            x: X position (-32768 to 32767)
            y: Y position (-32768 to 32767)
        """
        self._joypad.set_stick(stick, x, y)


DEFAULT_XBOX_ONE_JOYPAD_DEF = DeviceDefinition(
    name="Wolf X-Box One (virtual) pad",
    vendor_id=0x045E,
    product_id=0x02EA,
    version=0x0408,
)


class XBoxOneJoypad(Joypad):
    """Xbox One controller implementation."""

    def __init__(
        self, device_def: DeviceDefinition = DEFAULT_XBOX_ONE_JOYPAD_DEF
    ) -> None:
        """Initialize Xbox One controller.

        Args:
            device_def: Optional device definition.
        """
        joypad = _core.XboxOneJoypad.create(device_def.to_core())
        super().__init__(device_def, joypad)
        self._xbox_joypad = joypad

    def set_on_rumble(self, callback: Callable[[int, int], None]) -> None:
        """Set rumble feedback callback.

        Args:
            callback: Function called when rumble event occurs.
                     Takes (low_freq, high_freq) as arguments
        """
        self._xbox_joypad.set_on_rumble(callback)


DEFAULT_SWITCH_JOYPAD_DEF = DeviceDefinition(
    name="Wolf Nintendo (virtual) pad",
    vendor_id=0x057E,
    product_id=0x2009,
    version=0x8111,
)


class SwitchJoypad(Joypad):
    """Nintendo Switch controller implementation."""

    def __init__(
        self, device_def: DeviceDefinition = DEFAULT_SWITCH_JOYPAD_DEF
    ) -> None:
        """Initialize Switch controller.

        Args:
            device_def: Optional device definition.
        """
        joypad = _core.SwitchJoypad.create(device_def.to_core())
        super().__init__(device_def, joypad)
        self._switch_joypad = joypad

    def set_on_rumble(self, callback: Callable[[int, int], None]) -> None:
        """Set rumble feedback callback.

        Args:
            callback: Function called when rumble event occurs.
                     Takes (low_freq, high_freq) as arguments
        """
        self._switch_joypad.set_on_rumble(callback)


DEFAULT_PS5_JOYPAD_DEF = DeviceDefinition(
    name="Wolf DualSense (virtual) pad",
    vendor_id=0x054C,
    product_id=0x0CE6,
    version=0x8111,
)


class PS5Joypad(Joypad):
    """PlayStation 5 DualSense controller implementation."""

    def __init__(self, device_def: DeviceDefinition = DEFAULT_PS5_JOYPAD_DEF) -> None:
        """Initialize PS5 controller.

        Args:
            device_def: Optional device definition.
        """
        joypad = _core.PS5Joypad.create(device_def.to_core())
        super().__init__(device_def, joypad)
        self._ps5_joypad = joypad

    @property
    def mac_address(self) -> str:
        """Get MAC address of the controller.

        Returns:
            MAC address string
        """
        return self._ps5_joypad.get_mac_address()

    @property
    def sys_nodes(self) -> list[str]:
        """Get system device nodes.

        Returns:
            List of system node paths
        """
        return self._ps5_joypad.get_sys_nodes()

    def place_finger(self, finger_nr: int, x: int, y: int) -> None:
        """Place finger on touchpad.

        Args:
            finger_nr: Finger number (multi-touch index)
            x: X position (0-1920)
            y: Y position (0-1080)
        """
        self._ps5_joypad.place_finger(finger_nr, x, y)

    def release_finger(self, finger_nr: int) -> None:
        """Release finger from touchpad.

        Args:
            finger_nr: Finger number to release
        """
        self._ps5_joypad.release_finger(finger_nr)

    def set_motion(self, type_: PS5MotionType, x: float, y: float, z: float) -> None:
        """Set motion sensor values.

        Args:
            type_: Sensor type (acceleration or gyroscope)
            x: X-axis value (m/s² for accel, deg/s for gyro)
            y: Y-axis value
            z: Z-axis value
        """
        self._ps5_joypad.set_motion(type_, x, y, z)

    def set_battery(self, state: PS5BatteryState, percentage: int) -> None:
        """Set battery state.

        Args:
            state: Battery state
            percentage: Battery level percentage (0-100)
        """
        self._ps5_joypad.set_battery(state, percentage)

    def set_on_rumble(self, callback: Callable[[int, int], None]) -> None:
        """Set rumble feedback callback.

        Args:
            callback: Function called when rumble event occurs.
                     Takes (low_freq, high_freq) as arguments
        """
        self._ps5_joypad.set_on_rumble(callback)

    def set_on_led(self, callback: Callable[[int, int, int], None]) -> None:
        """Set LED color callback.

        Args:
            callback: Function called when LED color changes.
                     Takes (r, g, b) as arguments
        """
        self._ps5_joypad.set_on_led(callback)
