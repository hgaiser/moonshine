"""Pybind inputtino module!"""

from __future__ import annotations

import typing

__all__ = [
    "ControllerButton",
    "DeviceDefinition",
    "Joypad",
    "Keyboard",
    "Mouse",
    "MouseButton",
    "PS5BatteryState",
    "PS5Joypad",
    "PS5MotionType",
    "PenButtonType",
    "PenTablet",
    "PenToolType",
    "StickPosition",
    "SwitchJoypad",
    "TouchScreen",
    "Trackpad",
    "VirtualDevice",
    "XboxOneJoypad",
]

class ControllerButton:
    """
    Members:

      DPAD_UP

      DPAD_DOWN

      DPAD_LEFT

      DPAD_RIGHT

      START

      BACK

      HOME

      LEFT_STICK

      RIGHT_STICK

      LEFT_BUTTON

      RIGHT_BUTTON

      SPECIAL_FLAG

      PADDLE1_FLAG

      PADDLE2_FLAG

      PADDLE3_FLAG

      PADDLE4_FLAG

      TOUCHPAD_FLAG

      MISC_FLAG

      A

      B

      X

      Y
    """

    A: typing.ClassVar[ControllerButton]  # value = <ControllerButton.A: 4096>
    B: typing.ClassVar[ControllerButton]  # value = <ControllerButton.B: 8192>
    BACK: typing.ClassVar[ControllerButton]  # value = <ControllerButton.BACK: 32>
    DPAD_DOWN: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.DPAD_DOWN: 2>
    DPAD_LEFT: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.DPAD_LEFT: 4>
    DPAD_RIGHT: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.DPAD_RIGHT: 8>
    DPAD_UP: typing.ClassVar[ControllerButton]  # value = <ControllerButton.DPAD_UP: 1>
    HOME: typing.ClassVar[ControllerButton]  # value = <ControllerButton.HOME: 1024>
    LEFT_BUTTON: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.LEFT_BUTTON: 256>
    LEFT_STICK: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.LEFT_STICK: 64>
    MISC_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.MISC_FLAG: 2097152>
    PADDLE1_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.PADDLE1_FLAG: 65536>
    PADDLE2_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.PADDLE2_FLAG: 131072>
    PADDLE3_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.PADDLE3_FLAG: 262144>
    PADDLE4_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.PADDLE4_FLAG: 524288>
    RIGHT_BUTTON: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.RIGHT_BUTTON: 512>
    RIGHT_STICK: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.RIGHT_STICK: 128>
    SPECIAL_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.HOME: 1024>
    START: typing.ClassVar[ControllerButton]  # value = <ControllerButton.START: 16>
    TOUCHPAD_FLAG: typing.ClassVar[
        ControllerButton
    ]  # value = <ControllerButton.TOUCHPAD_FLAG: 1048576>
    X: typing.ClassVar[ControllerButton]  # value = <ControllerButton.X: 16384>
    Y: typing.ClassVar[ControllerButton]  # value = <ControllerButton.Y: 32768>
    __members__: typing.ClassVar[
        dict[str, ControllerButton]
    ]  # value = {'DPAD_UP': <ControllerButton.DPAD_UP: 1>, 'DPAD_DOWN': <ControllerButton.DPAD_DOWN: 2>, 'DPAD_LEFT': <ControllerButton.DPAD_LEFT: 4>, 'DPAD_RIGHT': <ControllerButton.DPAD_RIGHT: 8>, 'START': <ControllerButton.START: 16>, 'BACK': <ControllerButton.BACK: 32>, 'HOME': <ControllerButton.HOME: 1024>, 'LEFT_STICK': <ControllerButton.LEFT_STICK: 64>, 'RIGHT_STICK': <ControllerButton.RIGHT_STICK: 128>, 'LEFT_BUTTON': <ControllerButton.LEFT_BUTTON: 256>, 'RIGHT_BUTTON': <ControllerButton.RIGHT_BUTTON: 512>, 'SPECIAL_FLAG': <ControllerButton.HOME: 1024>, 'PADDLE1_FLAG': <ControllerButton.PADDLE1_FLAG: 65536>, 'PADDLE2_FLAG': <ControllerButton.PADDLE2_FLAG: 131072>, 'PADDLE3_FLAG': <ControllerButton.PADDLE3_FLAG: 262144>, 'PADDLE4_FLAG': <ControllerButton.PADDLE4_FLAG: 524288>, 'TOUCHPAD_FLAG': <ControllerButton.TOUCHPAD_FLAG: 1048576>, 'MISC_FLAG': <ControllerButton.MISC_FLAG: 2097152>, 'A': <ControllerButton.A: 4096>, 'B': <ControllerButton.B: 8192>, 'X': <ControllerButton.X: 16384>, 'Y': <ControllerButton.Y: 32768>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class DeviceDefinition:
    device_phys: str
    device_uniq: str
    name: str
    product_id: int
    vendor_id: int
    version: int
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __init__(self) -> None: ...

class Joypad(VirtualDevice):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def set_pressed_buttons(self, arg0: int) -> None: ...
    def set_stick(self, arg0: StickPosition, arg1: int, arg2: int) -> None: ...
    def set_triggers(self, arg0: int, arg1: int) -> None: ...

class Keyboard(VirtualDevice):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition, arg1: int) -> Keyboard: ...
    def press(self, arg0: int) -> None: ...
    def release(self, arg0: int) -> None: ...

class Mouse(VirtualDevice):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> Mouse: ...
    def horizontal_scroll(self, arg0: int) -> None: ...
    def move(self, arg0: int, arg1: int) -> None: ...
    def move_abs(self, arg0: int, arg1: int, arg2: int, arg3: int) -> None: ...
    def press(self, arg0: MouseButton) -> None: ...
    def release(self, arg0: MouseButton) -> None: ...
    def vertical_scroll(self, arg0: int) -> None: ...

class MouseButton:
    """
    Members:

      LEFT

      MIDDLE

      RIGHT

      SIDE

      EXTRA
    """

    EXTRA: typing.ClassVar[MouseButton]  # value = <MouseButton.EXTRA: 4>
    LEFT: typing.ClassVar[MouseButton]  # value = <MouseButton.LEFT: 0>
    MIDDLE: typing.ClassVar[MouseButton]  # value = <MouseButton.MIDDLE: 1>
    RIGHT: typing.ClassVar[MouseButton]  # value = <MouseButton.RIGHT: 2>
    SIDE: typing.ClassVar[MouseButton]  # value = <MouseButton.SIDE: 3>
    __members__: typing.ClassVar[
        dict[str, MouseButton]
    ]  # value = {'LEFT': <MouseButton.LEFT: 0>, 'MIDDLE': <MouseButton.MIDDLE: 1>, 'RIGHT': <MouseButton.RIGHT: 2>, 'SIDE': <MouseButton.SIDE: 3>, 'EXTRA': <MouseButton.EXTRA: 4>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class PS5BatteryState:
    """
    Members:

      BATTERY_DISCHARGING

      BATTERY_CHARGING

      BATTERY_FULL

      VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE

      TEMPERATURE_ERROR

      CHARGING_ERROR
    """

    BATTERY_CHARGING: typing.ClassVar[
        PS5BatteryState
    ]  # value = <PS5BatteryState.BATTERY_CHARGING: 1>
    BATTERY_DISCHARGING: typing.ClassVar[
        PS5BatteryState
    ]  # value = <PS5BatteryState.BATTERY_DISCHARGING: 0>
    BATTERY_FULL: typing.ClassVar[
        PS5BatteryState
    ]  # value = <PS5BatteryState.BATTERY_FULL: 2>
    CHARGING_ERROR: typing.ClassVar[
        PS5BatteryState
    ]  # value = <PS5BatteryState.CHARGING_ERROR: 15>
    TEMPERATURE_ERROR: typing.ClassVar[
        PS5BatteryState
    ]  # value = <PS5BatteryState.TEMPERATURE_ERROR: 11>
    VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE: typing.ClassVar[
        PS5BatteryState
    ]  # value = <PS5BatteryState.VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE: 10>
    __members__: typing.ClassVar[
        dict[str, PS5BatteryState]
    ]  # value = {'BATTERY_DISCHARGING': <PS5BatteryState.BATTERY_DISCHARGING: 0>, 'BATTERY_CHARGING': <PS5BatteryState.BATTERY_CHARGING: 1>, 'BATTERY_FULL': <PS5BatteryState.BATTERY_FULL: 2>, 'VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE': <PS5BatteryState.VOLTAGE_OR_TEMPERATURE_OUT_OF_RANGE: 10>, 'TEMPERATURE_ERROR': <PS5BatteryState.TEMPERATURE_ERROR: 11>, 'CHARGING_ERROR': <PS5BatteryState.CHARGING_ERROR: 15>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class PS5Joypad(Joypad):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> PS5Joypad: ...
    def get_mac_address(self) -> str: ...
    def get_sys_nodes(self) -> list[str]: ...
    def place_finger(self, arg0: int, arg1: int, arg2: int) -> None: ...
    def release_finger(self, arg0: int) -> None: ...
    def set_battery(self, arg0: PS5BatteryState, arg1: int) -> None: ...
    def set_motion(
        self, arg0: PS5MotionType, arg1: float, arg2: float, arg3: float
    ) -> None: ...
    def set_on_led(self, arg0: typing.Callable[[int, int, int], None]) -> None: ...
    def set_on_rumble(self, arg0: typing.Callable[[int, int], None]) -> None: ...

class PS5MotionType:
    """
    Members:

      ACCELERATION

      GYROSCOPE
    """

    ACCELERATION: typing.ClassVar[
        PS5MotionType
    ]  # value = <PS5MotionType.ACCELERATION: 1>
    GYROSCOPE: typing.ClassVar[PS5MotionType]  # value = <PS5MotionType.GYROSCOPE: 2>
    __members__: typing.ClassVar[
        dict[str, PS5MotionType]
    ]  # value = {'ACCELERATION': <PS5MotionType.ACCELERATION: 1>, 'GYROSCOPE': <PS5MotionType.GYROSCOPE: 2>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class PenButtonType:
    """
    Members:

      PRIMARY

      SECONDARY

      TERTIARY
    """

    PRIMARY: typing.ClassVar[PenButtonType]  # value = <PenButtonType.PRIMARY: 0>
    SECONDARY: typing.ClassVar[PenButtonType]  # value = <PenButtonType.SECONDARY: 1>
    TERTIARY: typing.ClassVar[PenButtonType]  # value = <PenButtonType.TERTIARY: 2>
    __members__: typing.ClassVar[
        dict[str, PenButtonType]
    ]  # value = {'PRIMARY': <PenButtonType.PRIMARY: 0>, 'SECONDARY': <PenButtonType.SECONDARY: 1>, 'TERTIARY': <PenButtonType.TERTIARY: 2>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class PenTablet(VirtualDevice):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> PenTablet: ...
    def place_tool(
        self,
        arg0: PenToolType,
        arg1: float,
        arg2: float,
        arg3: float,
        arg4: float,
        arg5: float,
        arg6: float,
    ) -> None: ...
    def set_btn(self, arg0: PenButtonType, arg1: bool) -> None: ...

class PenToolType:
    """
    Members:

      PEN

      ERASER

      BRUSH

      PENCIL

      AIRBRUSH

      TOUCH

      SAME_AS_BEFORE
    """

    AIRBRUSH: typing.ClassVar[PenToolType]  # value = <PenToolType.AIRBRUSH: 4>
    BRUSH: typing.ClassVar[PenToolType]  # value = <PenToolType.BRUSH: 2>
    ERASER: typing.ClassVar[PenToolType]  # value = <PenToolType.ERASER: 1>
    PEN: typing.ClassVar[PenToolType]  # value = <PenToolType.PEN: 0>
    PENCIL: typing.ClassVar[PenToolType]  # value = <PenToolType.PENCIL: 3>
    SAME_AS_BEFORE: typing.ClassVar[
        PenToolType
    ]  # value = <PenToolType.SAME_AS_BEFORE: 6>
    TOUCH: typing.ClassVar[PenToolType]  # value = <PenToolType.TOUCH: 5>
    __members__: typing.ClassVar[
        dict[str, PenToolType]
    ]  # value = {'PEN': <PenToolType.PEN: 0>, 'ERASER': <PenToolType.ERASER: 1>, 'BRUSH': <PenToolType.BRUSH: 2>, 'PENCIL': <PenToolType.PENCIL: 3>, 'AIRBRUSH': <PenToolType.AIRBRUSH: 4>, 'TOUCH': <PenToolType.TOUCH: 5>, 'SAME_AS_BEFORE': <PenToolType.SAME_AS_BEFORE: 6>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class StickPosition:
    """
    Members:

      RS

      LS
    """

    LS: typing.ClassVar[StickPosition]  # value = <StickPosition.LS: 1>
    RS: typing.ClassVar[StickPosition]  # value = <StickPosition.RS: 0>
    __members__: typing.ClassVar[
        dict[str, StickPosition]
    ]  # value = {'RS': <StickPosition.RS: 0>, 'LS': <StickPosition.LS: 1>}
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def __eq__(self, other: typing.Any) -> bool: ...
    def __getstate__(self) -> int: ...
    def __hash__(self) -> int: ...
    def __index__(self) -> int: ...
    def __init__(self, value: int) -> None: ...
    def __int__(self) -> int: ...
    def __ne__(self, other: typing.Any) -> bool: ...
    def __repr__(self) -> str: ...
    def __setstate__(self, state: int) -> None: ...
    def __str__(self) -> str: ...
    @property
    def name(self) -> str: ...
    @property
    def value(self) -> int: ...

class SwitchJoypad(Joypad):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> SwitchJoypad: ...
    def set_on_rumble(self, arg0: typing.Callable[[int, int], None]) -> None: ...

class TouchScreen(VirtualDevice):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> TouchScreen: ...
    def place_finger(
        self, arg0: int, arg1: float, arg2: float, arg3: float, arg4: int
    ) -> None: ...
    def release_finger(self, arg0: int) -> None: ...

class Trackpad(VirtualDevice):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> Trackpad: ...
    def place_finger(
        self, arg0: int, arg1: float, arg2: float, arg3: float, arg4: int
    ) -> None: ...
    def release_finger(self, arg0: int) -> None: ...
    def set_left_btn(self, arg0: bool) -> None: ...

class VirtualDevice:
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    def get_nodes(self) -> list[str]: ...

class XboxOneJoypad(Joypad):
    @staticmethod
    def _pybind11_conduit_v1_(
        *args: typing.Any, **kwargs: typing.Any
    ) -> typing.Any: ...
    @staticmethod
    def create(arg0: DeviceDefinition) -> XboxOneJoypad: ...
    def set_on_rumble(self, arg0: typing.Callable[[int, int], None]) -> None: ...
