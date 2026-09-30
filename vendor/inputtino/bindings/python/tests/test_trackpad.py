"""Tests for trackpad.py."""

import time
from unittest.mock import MagicMock, patch

import pytest

from inputtino._core import Trackpad as CoreTrackpad
from inputtino.trackpad import DeviceDefinition, Trackpad
from tests.helpers import mark_practical


@pytest.fixture
def mock_core_trackpad():
    """Create a mock for CoreTrackpad with basic functionality."""
    with patch("inputtino._core.Trackpad") as mock_trackpad_cls:
        mock_instance = MagicMock(spec=CoreTrackpad)
        mock_trackpad_cls.create.return_value = mock_instance
        yield mock_instance


def test_trackpad_creation_with_default_device():
    """Test creating a trackpad with default device definition."""
    with patch("inputtino._core.Trackpad") as mock_trackpad_cls:
        Trackpad()

        mock_trackpad_cls.create.assert_called_once()
        created_def = mock_trackpad_cls.create.call_args[0][0]
        assert created_def.name == "Wolf (virtual) touchpad"
        assert created_def.vendor_id == 0xAB00
        assert created_def.product_id == 0xAB02
        assert created_def.version == 0xAB00


def test_trackpad_creation_with_custom_device():
    """Test creating a trackpad with custom device definition."""
    custom_def = DeviceDefinition(
        name="Custom Trackpad",
        vendor_id=0x1234,
        product_id=0x5678,
        version=0x0001,
        device_phys="/dev/input/test",
        device_uniq="unique_id",
    )

    with patch("inputtino._core.Trackpad") as mock_trackpad_cls:
        Trackpad(custom_def)

        mock_trackpad_cls.create.assert_called_once()
        created_def = mock_trackpad_cls.create.call_args[0][0]
        assert created_def.name == custom_def.name
        assert created_def.vendor_id == custom_def.vendor_id
        assert created_def.product_id == custom_def.product_id
        assert created_def.version == custom_def.version
        assert created_def.device_phys == custom_def.device_phys
        assert created_def.device_uniq == custom_def.device_uniq


def test_trackpad_finger_operations(mock_core_trackpad):
    """Test trackpad finger placement and release operations."""
    trackpad = Trackpad()

    # Test finger placement
    trackpad.place_finger(0, 0.5, 0.5, 0.8, 45)
    mock_core_trackpad.place_finger.assert_called_once_with(0, 0.5, 0.5, 0.8, 45)

    # Test finger release
    trackpad.release_finger(0)
    mock_core_trackpad.release_finger.assert_called_once_with(0)


def test_trackpad_button_operations(mock_core_trackpad):
    """Test trackpad button operations."""
    trackpad = Trackpad()

    # Test button press
    trackpad.set_left_button(True)
    mock_core_trackpad.set_left_btn.assert_called_once_with(True)

    # Test button release
    mock_core_trackpad.set_left_btn.reset_mock()
    trackpad.set_left_button(False)
    mock_core_trackpad.set_left_btn.assert_called_once_with(False)


def test_trackpad_nodes(mock_core_trackpad):
    """Test getting trackpad device nodes."""
    expected_nodes = ["/dev/input/event1"]
    mock_core_trackpad.get_nodes.return_value = expected_nodes

    trackpad = Trackpad()
    assert trackpad.nodes == expected_nodes
    mock_core_trackpad.get_nodes.assert_called_once()


def test_trackpad_creation_failure():
    """Test handling of trackpad creation failure."""
    with patch("inputtino._core.Trackpad") as mock_trackpad_cls:
        mock_trackpad_cls.create.side_effect = RuntimeError(
            "Failed to create trackpad device"
        )

        with pytest.raises(RuntimeError, match="Failed to create trackpad device"):
            Trackpad()


@mark_practical
def test_practical_trackpad_gestures():
    """Test practical trackpad gestures.

    This practical test demonstrates basic trackpad functionality by:
    1. Simulating single finger tap
    2. Simulating two-finger scroll
    3. Testing button click

    Note:
        This test requires actual device access (/dev/uinput)
        and may fail if run without proper permissions.
    """
    trackpad = Trackpad()

    # Ensure we have valid device nodes
    assert len(trackpad.nodes) > 0, "No trackpad device nodes created"

    # Give system time to register the device
    time.sleep(0.1)

    # Single finger tap
    trackpad.place_finger(0, 0.5, 0.5, 1.0, 0)
    time.sleep(0.1)
    trackpad.release_finger(0)

    # Two-finger scroll (vertical)
    trackpad.place_finger(0, 0.5, 0.3, 1.0, 0)
    trackpad.place_finger(1, 0.5, 0.4, 1.0, 0)
    for y in range(3, 8):
        trackpad.place_finger(0, 0.5, y / 10, 1.0, 0)
        trackpad.place_finger(1, 0.5, (y + 1) / 10, 1.0, 0)
        time.sleep(0.05)
    trackpad.release_finger(0)
    trackpad.release_finger(1)

    # Button click
    trackpad.set_left_button(True)
    time.sleep(0.1)
    trackpad.set_left_button(False)
