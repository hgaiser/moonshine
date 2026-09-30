"""Touchscreen input device implementation."""

from . import _core
from .base import DeviceDefinition, VirtualDevice

# Default device definition for touchscreen
DEFAULT_TOUCHSCREEN = DeviceDefinition(
    name="Wolf (virtual) touchscreen",
    vendor_id=0xAB00,
    product_id=0xAB03,
    version=0xAB00,
)


class TouchScreen(VirtualDevice):
    """Virtual touchscreen input device.

    This class provides functionality to simulate touchscreen
    interactions. The device appears as a real touchscreen input device
    to the system.
    """

    def __init__(self, device_def: DeviceDefinition = DEFAULT_TOUCHSCREEN) -> None:
        """Initialize the touchscreen device with optional custom device
        definition.

        Args:
            device_def: Optional device definition for customizing the virtual device properties

        Raises:
            RuntimeError: If device creation fails
        """
        self._touchscreen = _core.TouchScreen.create(device_def.to_core())
        super().__init__(self._touchscreen)

    def place_finger(
        self,
        finger_nr: int,
        x: float,
        y: float,
        pressure: float = 1.0,
        orientation: int = 0,
    ) -> None:
        """Place a finger on the touchscreen.

        Args:
            finger_nr: Finger number for multi-touch support
            x: X coordinate in normalized device coordinates [0.0, 1.0]
            y: Y coordinate in normalized device coordinates [0.0, 1.0]
            pressure: Pressure value between 0.0 and 1.0
            orientation: Finger orientation in degrees [-90, 90]
        """
        self._touchscreen.place_finger(finger_nr, x, y, pressure, orientation)

    def release_finger(self, finger_nr: int) -> None:
        """Release a finger from the touchscreen.

        Args:
            finger_nr: Finger number to release
        """
        self._touchscreen.release_finger(finger_nr)
