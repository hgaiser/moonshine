"""Tests for touchscreen.py."""

import time
from unittest.mock import MagicMock, patch

import pytest

from inputtino._core import TouchScreen as CoreTouchScreen
from inputtino.touchscreen import DeviceDefinition, TouchScreen
from tests.helpers import mark_practical


@pytest.fixture
def mock_core_touchscreen():
    """Create a mock for CoreTouchScreen with basic functionality."""
    with patch("inputtino._core.TouchScreen") as mock_touchscreen_cls:
        mock_instance = MagicMock(spec=CoreTouchScreen)
        mock_touchscreen_cls.create.return_value = mock_instance
        yield mock_instance


def test_touchscreen_creation_with_default_device():
    """Test creating a touchscreen with default device definition."""
    with patch("inputtino._core.TouchScreen") as mock_touchscreen_cls:
        TouchScreen()

        mock_touchscreen_cls.create.assert_called_once()
        created_def = mock_touchscreen_cls.create.call_args[0][0]
        assert created_def.name == "Wolf (virtual) touchscreen"
        assert created_def.vendor_id == 0xAB00
        assert created_def.product_id == 0xAB03
        assert created_def.version == 0xAB00


def test_touchscreen_creation_with_custom_device():
    """Test creating a touchscreen with custom device definition."""
    custom_def = DeviceDefinition(
        name="Custom TouchScreen",
        vendor_id=0x1234,
        product_id=0x5678,
        version=0x0001,
        device_phys="/dev/input/test",
        device_uniq="unique_id",
    )

    with patch("inputtino._core.TouchScreen") as mock_touchscreen_cls:
        TouchScreen(custom_def)

        mock_touchscreen_cls.create.assert_called_once()
        created_def = mock_touchscreen_cls.create.call_args[0][0]
        assert created_def.name == custom_def.name
        assert created_def.vendor_id == custom_def.vendor_id
        assert created_def.product_id == custom_def.product_id
        assert created_def.version == custom_def.version
        assert created_def.device_phys == custom_def.device_phys
        assert created_def.device_uniq == custom_def.device_uniq


def test_touchscreen_finger_placement(mock_core_touchscreen):
    """Test finger placement on touchscreen."""
    touchscreen = TouchScreen()

    # Test placing a finger with default pressure and orientation
    touchscreen.place_finger(0, 0.5, 0.5)
    mock_core_touchscreen.place_finger.assert_called_with(0, 0.5, 0.5, 1.0, 0)

    # Test placing a finger with custom pressure and orientation
    touchscreen.place_finger(1, 0.75, 0.25, 0.8, 45)
    mock_core_touchscreen.place_finger.assert_called_with(1, 0.75, 0.25, 0.8, 45)


def test_touchscreen_finger_release(mock_core_touchscreen):
    """Test finger release from touchscreen."""
    touchscreen = TouchScreen()
    touchscreen.release_finger(0)
    mock_core_touchscreen.release_finger.assert_called_with(0)


def test_touchscreen_nodes(mock_core_touchscreen):
    """Test getting touchscreen device nodes."""
    expected_nodes = ["/dev/input/event0"]
    mock_core_touchscreen.get_nodes.return_value = expected_nodes

    touchscreen = TouchScreen()
    assert touchscreen.nodes == expected_nodes
    mock_core_touchscreen.get_nodes.assert_called_once()


def test_touchscreen_creation_failure():
    """Test handling of touchscreen creation failure."""
    with patch("inputtino._core.TouchScreen") as mock_touchscreen_cls:
        mock_touchscreen_cls.create.side_effect = RuntimeError(
            "Failed to create touchscreen device"
        )

        with pytest.raises(RuntimeError, match="Failed to create touchscreen device"):
            TouchScreen()


@mark_practical
def test_practical_touchscreen_draw_circle():
    """Test drawing a circle pattern with touch input.

    This practical test demonstrates drawing a circle by:
    1. Moving a single touch point in a circular pattern
    2. Using normalized coordinates
    3. Creating a smooth touch movement pattern

    Note:
        This test requires actual device access (/dev/uinput)
        and may fail if run without proper permissions.
    """
    touchscreen = TouchScreen()

    # Ensure we have valid device nodes
    assert len(touchscreen.nodes) > 0, "No touchscreen device nodes created"

    # Give system time to register the device
    time.sleep(0.1)

    # Draw a circle using touch inputs
    from math import cos, pi, sin

    center_x, center_y = 0.5, 0.5  # Center of the screen
    radius = 0.2  # Circle radius in normalized coordinates
    steps = 36  # Number of points to create the circle

    try:
        # Draw circle pattern
        for i in range(steps + 1):
            angle = 2 * pi * i / steps
            x = center_x + radius * cos(angle)
            y = center_y + radius * sin(angle)

            # Ensure coordinates stay within bounds
            x = max(0.0, min(1.0, x))
            y = max(0.0, min(1.0, y))

            touchscreen.place_finger(0, x, y)
            time.sleep(0.02)  # Small delay for smooth movement

    finally:
        # Ensure we always release the finger
        touchscreen.release_finger(0)
