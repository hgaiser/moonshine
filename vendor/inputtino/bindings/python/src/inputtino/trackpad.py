"""Trackpad input device implementation."""

from . import _core
from .base import DeviceDefinition, VirtualDevice

DEFAULT_TRACKPAD = DeviceDefinition(
    name="Wolf (virtual) touchpad",
    vendor_id=0xAB00,
    product_id=0xAB02,
    version=0xAB00,
)


class Trackpad(VirtualDevice):
    """Virtual trackpad input device.

    This class provides functionality to simulate multi-touch trackpad
    interactions. The device appears as a real touchpad input device to
    the system.

    implements a pure multi-touch touchpad as defined in libinput
    https://wayland.freedesktop.org/libinput/doc/latest/touchpads.html
    """

    def __init__(self, device_def: DeviceDefinition = DEFAULT_TRACKPAD) -> None:
        """Initialize the trackpad device with optional custom device
        definition.

        Args:
            device_def: Optional device definition for customizing the virtual device properties

        Raises:
            RuntimeError: If device creation fails
        """
        self._trackpad = _core.Trackpad.create(device_def.to_core())
        super().__init__(self._trackpad)

    def place_finger(
        self,
        finger_nr: int,
        x: float,
        y: float,
        pressure: float = 1.0,
        orientation: int = 0,
    ) -> None:
        """Place a finger on the trackpad surface.

        The x and y coordinates are normalized device coordinates from the top-left
        corner (0.0, 0.0) to bottom-right corner (1.0, 1.0).

        Args:
            finger_nr: Finger index (supports multi-touch)
            x: Normalized X coordinate [0.0, 1.0]
            y: Normalized Y coordinate [0.0, 1.0]
            pressure: Touch pressure [0.0, 1.0]
            orientation: Finger orientation in degrees [-90, 90]
        """
        self._trackpad.place_finger(finger_nr, x, y, pressure, orientation)

    def release_finger(self, finger_nr: int) -> None:
        """Release a finger from the trackpad surface.

        Args:
            finger_nr: Finger index to release
        """
        self._trackpad.release_finger(finger_nr)

    def set_left_button(self, pressed: bool) -> None:
        """Set the state of the left trackpad button.

        Args:
            pressed: True to press the button, False to release
        """
        self._trackpad.set_left_btn(pressed)
