"""Mouse input device implementation."""

import time

from . import _core
from ._core import MouseButton
from .base import DeviceDefinition, VirtualDevice

# Default device definition for mouse
DEFAULT_MOUSE = DeviceDefinition(
    name="Wolf mouse virtual device",
    vendor_id=0xAB00,
    product_id=0xAB01,
    version=0xAB00,
)


class Mouse(VirtualDevice):
    """Virtual mouse input device.

    This class provides functionality to simulate mouse movements,
    clicks, and scrolling. The device appears as a real mouse input
    device to the system.
    """

    def __init__(self, device_def: DeviceDefinition = DEFAULT_MOUSE) -> None:
        """Initialize the mouse device with optional custom device definition.

        Args:
            device_def: Optional device definition for customizing the virtual device properties

        Raises:
            RuntimeError: If device creation fails
        """
        self._mouse = _core.Mouse.create(device_def.to_core())
        super().__init__(self._mouse)

    def move(self, delta_x: int, delta_y: int) -> None:
        """Move the mouse cursor relative to its current position.

        Args:
            delta_x: Horizontal movement (positive is right, negative is left)
            delta_y: Vertical movement (positive is down, negative is up)
        """
        self._mouse.move(delta_x, delta_y)

    def move_abs(self, x: int, y: int, screen_width: int, screen_height: int) -> None:
        """Move the mouse cursor to an absolute screen position.

        Args:
            x: Target X coordinate
            y: Target Y coordinate
            screen_width: Width of the screen in pixels
            screen_height: Height of the screen in pixels
        """
        self._mouse.move_abs(x, y, screen_width, screen_height)

    def press(self, button: MouseButton) -> None:
        """Press a mouse button.

        Args:
            button: The button to press
        """
        self._mouse.press(button)

    def release(self, button: MouseButton) -> None:
        """Release a mouse button.

        Args:
            button: The button to release
        """
        self._mouse.release(button)

    def scroll_vertical(self, distance: int) -> None:
        """Scroll vertically.

        Args:
            distance: Scroll distance in high resolution units (±120 per notch).
                     Positive values scroll down, negative values scroll up.
        """
        self._mouse.vertical_scroll(distance)

    def scroll_horizontal(self, distance: int) -> None:
        """Scroll horizontally.

        Args:
            distance: Scroll distance in high resolution units (±120 per notch).
                     Positive values scroll right, negative values scroll left.
        """
        self._mouse.horizontal_scroll(distance)

    def click(
        self, button: MouseButton = MouseButton.LEFT, duration: float = 0.0
    ) -> None:
        """Perform a mouse click by pressing and releasing a button.

        Args:
            button: The button to click
            duration: How long to hold the button down in seconds
        """
        try:
            self.press(button)
            if duration > 0:
                time.sleep(duration)
        finally:
            self.release(button)
