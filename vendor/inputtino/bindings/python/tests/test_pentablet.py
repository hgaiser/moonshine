"""Tests for pentablet.py."""

import math
import time
from unittest.mock import MagicMock, patch

import pytest

from inputtino._core import PenTablet as CorePenTablet
from inputtino.pentablet import (
    DEFAULT_PEN_TABLET,
    DeviceDefinition,
    PenButtonType,
    PenTablet,
    PenToolType,
)
from tests.helpers import mark_practical


@pytest.fixture
def mock_core_tablet():
    """Create a mock for CorePenTablet with basic functionality."""
    with patch("inputtino._core.PenTablet") as mock_tablet_cls:
        mock_instance = MagicMock(spec=CorePenTablet)
        mock_tablet_cls.create.return_value = mock_instance
        yield mock_instance


def test_pen_tablet_creation_with_default_device():
    """Test creating a pen tablet with default device definition."""
    with patch("inputtino._core.PenTablet") as mock_tablet_cls:
        PenTablet()

        mock_tablet_cls.create.assert_called_once()
        created_def = mock_tablet_cls.create.call_args[0][0]
        assert created_def.name == DEFAULT_PEN_TABLET.name
        assert created_def.vendor_id == DEFAULT_PEN_TABLET.vendor_id
        assert created_def.product_id == DEFAULT_PEN_TABLET.product_id
        assert created_def.version == DEFAULT_PEN_TABLET.version


def test_pen_tablet_creation_with_custom_device():
    """Test creating a pen tablet with custom device definition."""
    custom_def = DeviceDefinition(
        name="Custom Pen Tablet",
        vendor_id=0x1234,
        product_id=0x5678,
        version=0x0001,
        device_phys="/dev/input/test",
        device_uniq="unique_id",
    )

    with patch("inputtino._core.PenTablet") as mock_tablet_cls:
        PenTablet(custom_def)

        mock_tablet_cls.create.assert_called_once()
        created_def = mock_tablet_cls.create.call_args[0][0]
        assert created_def.name == custom_def.name
        assert created_def.vendor_id == custom_def.vendor_id
        assert created_def.product_id == custom_def.product_id
        assert created_def.version == custom_def.version
        assert created_def.device_phys == custom_def.device_phys
        assert created_def.device_uniq == custom_def.device_uniq


def test_pen_tablet_place_tool(mock_core_tablet):
    """Test placing a tool on the tablet surface."""
    tablet = PenTablet()
    tablet.place_tool(
        PenToolType.PEN,
        x=0.5,
        y=0.5,
        pressure=1.0,
        distance=0.0,
        tilt_x=45.0,
        tilt_y=-45.0,
    )

    mock_core_tablet.place_tool.assert_called_once_with(
        PenToolType.PEN, 0.5, 0.5, 1.0, 0.0, 45.0, -45.0
    )


def test_pen_tablet_set_button(mock_core_tablet):
    """Test setting pen button states."""
    tablet = PenTablet()

    for button in [
        PenButtonType.PRIMARY,
        PenButtonType.SECONDARY,
        PenButtonType.TERTIARY,
    ]:
        for pressed in (True, False):
            tablet.set_button(button, pressed)
            mock_core_tablet.set_btn.assert_called_with(button, pressed)


def test_pen_tablet_nodes(mock_core_tablet):
    """Test getting pen tablet device nodes."""
    expected_nodes = ["/dev/input/event0", "/dev/input/pen0"]
    mock_core_tablet.get_nodes.return_value = expected_nodes

    tablet = PenTablet()
    assert tablet.nodes == expected_nodes
    mock_core_tablet.get_nodes.assert_called_once()


def test_pen_tablet_creation_failure():
    """Test handling of pen tablet creation failure."""
    with patch("inputtino._core.PenTablet") as mock_tablet_cls:
        mock_tablet_cls.create.side_effect = RuntimeError(
            "Failed to create pen tablet device"
        )

        with pytest.raises(RuntimeError, match="Failed to create pen tablet device"):
            PenTablet()


@mark_practical
def test_practical_pen_draw_circle():
    """Test drawing a circle pattern with the pen tablet.

    This practical test demonstrates:
    1. Drawing a circle using the pen tool
    2. Varying pressure based on position
    3. Adding tilt effects

    Note:
        This test requires actual device access (/dev/uinput)
        and may fail if run without proper permissions.
    """
    tablet = PenTablet()

    # Ensure we have valid device nodes
    assert len(tablet.nodes) > 0, "No pen tablet device nodes created"

    # Give system time to register the device
    time.sleep(0.1)

    # Draw circle pattern
    radius = 0.3  # 30% of tablet width
    center_x = 0.5
    center_y = 0.5
    steps = 50
    try:
        for i in range(steps + 1):
            angle = 2 * math.pi * i / steps
            x = center_x + radius * math.cos(angle)
            y = center_y + radius * math.sin(angle)

            # Vary pressure based on position (higher on right side)
            pressure = 0.5 + 0.5 * math.cos(angle)

            # Add tilt effect (tilt towards center)
            tilt_x = -30 * math.cos(angle)  # -30 to 30 degrees
            tilt_y = -30 * math.sin(angle)  # -30 to 30 degrees

            tablet.place_tool(
                PenToolType.PEN,
                x=x,
                y=y,
                pressure=pressure,
                distance=0.0,  # Pen is touching surface
                tilt_x=tilt_x,
                tilt_y=tilt_y,
            )
            time.sleep(0.02)  # Small delay for smooth movement

        # Test button presses at cardinal points
        for angle in [0, math.pi / 2, math.pi, 3 * math.pi / 2]:
            x = center_x + radius * math.cos(angle)
            y = center_y + radius * math.sin(angle)

            tablet.place_tool(
                PenToolType.PEN,
                x=x,
                y=y,
                pressure=1.0,
                distance=0.0,
                tilt_x=0,
                tilt_y=0,
            )
            tablet.set_button(PenButtonType.PRIMARY, True)
            time.sleep(0.1)
            tablet.set_button(PenButtonType.PRIMARY, False)
            time.sleep(0.1)

    finally:
        # Place tool at distance to indicate release
        tablet.place_tool(
            PenToolType.SAME_AS_BEFORE,
            x=center_x,
            y=center_y,
            pressure=0.0,
            distance=1.0,  # Pen is far from surface
            tilt_x=0,
            tilt_y=0,
        )
