"""Pen tablet input device implementation."""

from . import _core
from ._core import PenButtonType, PenToolType
from .base import DeviceDefinition, VirtualDevice

# Default device definition for pen tablet
DEFAULT_PEN_TABLET = DeviceDefinition(
    name="Wolf (virtual) pen tablet",
    vendor_id=0xAB00,
    product_id=0xAB04,
    version=0xAB00,
)


class PenTablet(VirtualDevice):
    """Virtual pen tablet input device.

    This class provides functionality to simulate pen tablet input with
    various tools and buttons. The device appears as a real pen tablet
    input device to the system.

    implements a pen tablet as defined in libinput
    https://wayland.freedesktop.org/libinput/doc/latest/tablet-support.html
    """

    def __init__(self, device_def: DeviceDefinition = DEFAULT_PEN_TABLET) -> None:
        """Initialize the pentablet device with optional custom device
        definition.

        Args:
            device_def: Optional device definition for customizing the virtual device properties

        Raises:
            RuntimeError: If device creation fails
        """
        self._tablet = _core.PenTablet.create(device_def.to_core())
        super().__init__(self._tablet)

    def place_tool(
        self,
        tool_type: PenToolType,
        x: float,
        y: float,
        pressure: float,
        distance: float,
        tilt_x: float,
        tilt_y: float,
    ) -> None:
        """Place a tool on the tablet surface.

        Args:
            tool_type: Type of tool being used
            x: X coordinate in range [0.0, 1.0]
            y: Y coordinate in range [0.0, 1.0]
            pressure: Tool pressure in range [0.0, 1.0]
            distance: Tool distance from surface in range [0.0, 1.0]
            tilt_x: Tool tilt on X axis in range [-90.0, 90.0] degrees
            tilt_y: Tool tilt on Y axis in range [-90.0, 90.0] degrees

        Refer to the libinput docs to better understand what each param means:
        https://wayland.freedesktop.org/libinput/doc/latest/tablet-support.html#special-axes-on-tablet-tools
        """
        self._tablet.place_tool(tool_type, x, y, pressure, distance, tilt_x, tilt_y)

    def set_button(self, button: PenButtonType, pressed: bool) -> None:
        """Set the state of a pen button.

        Args:
            button: The button to modify
            pressed: True if button is pressed, False if released
        """
        self._tablet.set_btn(button, pressed)
