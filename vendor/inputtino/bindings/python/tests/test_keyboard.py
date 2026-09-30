"""Tests for keyboard.py."""

import time
from unittest.mock import MagicMock, patch

import pytest

from inputtino._core import Keyboard as CoreKeyboard
from inputtino.base import DeviceDefinition
from inputtino.keyboard import DEFAULT_KEYBOARD, Keyboard, KeyCode
from tests.helpers import mark_practical


@pytest.fixture
def mock_core_keyboard():
    """Create a mock for CoreKeyboard with basic functionality."""
    with patch("inputtino._core.Keyboard") as mock_keyboard_cls:
        mock_instance = MagicMock(spec=CoreKeyboard)
        mock_keyboard_cls.create.return_value = mock_instance
        yield mock_instance


def test_keyboard_creation_with_default_device():
    """Test creating a keyboard with default device definition."""
    with patch("inputtino._core.Keyboard") as mock_keyboard_cls:
        Keyboard()

        mock_keyboard_cls.create.assert_called_once()
        created_def = mock_keyboard_cls.create.call_args[0][0]
        assert created_def.name == DEFAULT_KEYBOARD.name
        assert created_def.vendor_id == DEFAULT_KEYBOARD.vendor_id
        assert created_def.product_id == DEFAULT_KEYBOARD.product_id
        assert created_def.version == DEFAULT_KEYBOARD.version


def test_keyboard_creation_with_custom_device():
    """Test creating a keyboard with custom device definition."""
    custom_def = DeviceDefinition(
        name="Custom Keyboard",
        vendor_id=0x1234,
        product_id=0x5678,
        version=0x0001,
        device_phys="/dev/input/test",
        device_uniq="unique_id",
    )

    with patch("inputtino._core.Keyboard") as mock_keyboard_cls:
        Keyboard(custom_def)

        mock_keyboard_cls.create.assert_called_once()
        created_def = mock_keyboard_cls.create.call_args[0][0]
        assert created_def.name == custom_def.name
        assert created_def.vendor_id == custom_def.vendor_id
        assert created_def.product_id == custom_def.product_id
        assert created_def.version == custom_def.version
        assert created_def.device_phys == custom_def.device_phys
        assert created_def.device_uniq == custom_def.device_uniq


def test_keyboard_custom_repress_time():
    """Test creating a keyboard with custom key repress time."""
    with patch("inputtino._core.Keyboard") as mock_keyboard_cls:
        Keyboard(millis_repress_key=100)
        mock_keyboard_cls.create.assert_called_once()
        assert mock_keyboard_cls.create.call_args[0][1] == 100


def test_keyboard_key_press_and_release(mock_core_keyboard):
    """Test keyboard key press and release."""
    keyboard = Keyboard()

    # Test single key press/release
    keyboard.press(KeyCode.A)
    mock_core_keyboard.press.assert_called_with(KeyCode.A)

    keyboard.release(KeyCode.A)
    mock_core_keyboard.release.assert_called_with(KeyCode.A)

    # Test modifier key press/release
    keyboard.press(KeyCode.SHIFT)
    mock_core_keyboard.press.assert_called_with(KeyCode.SHIFT)

    keyboard.release(KeyCode.SHIFT)
    mock_core_keyboard.release.assert_called_with(KeyCode.SHIFT)


def test_keyboard_nodes(mock_core_keyboard):
    """Test getting keyboard device nodes."""
    expected_nodes = ["/dev/input/event1"]
    mock_core_keyboard.get_nodes.return_value = expected_nodes

    keyboard = Keyboard()
    assert keyboard.nodes == expected_nodes
    mock_core_keyboard.get_nodes.assert_called_once()


def test_keyboard_type(mock_core_keyboard):
    """Test keyboard type method."""
    keyboard = Keyboard()

    with patch("time.sleep") as mock_sleep:
        # Test with default duration
        keyboard.type(KeyCode.A)
        mock_core_keyboard.press.assert_called_with(KeyCode.A)
        mock_sleep.assert_called_with(0.1)
        mock_core_keyboard.release.assert_called_with(KeyCode.A)

        # Test with custom duration
        keyboard.type(KeyCode.B, 0.5)
        mock_core_keyboard.press.assert_called_with(KeyCode.B)
        mock_sleep.assert_called_with(0.5)
        mock_core_keyboard.release.assert_called_with(KeyCode.B)


def test_keyboard_creation_failure():
    """Test handling of keyboard creation failure."""
    with patch("inputtino._core.Keyboard") as mock_keyboard_cls:
        mock_keyboard_cls.create.side_effect = RuntimeError(
            "Failed to create keyboard device"
        )

        with pytest.raises(RuntimeError, match="Failed to create keyboard device"):
            Keyboard()


def test_key_code_from_string():
    assert KeyCode.from_str("a") is KeyCode.A
    assert KeyCode.from_str("SHIFT") is KeyCode.SHIFT

    with pytest.raises(KeyError):
        KeyCode.from_str("")


@mark_practical
def test_practical_keyboard_input():
    """Test simulating keyboard input with a practical example.

    This practical test demonstrates:
    1. Creating a keyboard device
    2. Simulating Ctrl+A (select all) keyboard shortcut
    3. Verifying device creation and basic functionality

    Note:
        This test requires actual device access (/dev/uinput)
        and may fail if run without proper permissions.
    """
    keyboard = Keyboard()

    # Ensure we have valid device nodes
    assert len(keyboard.nodes) > 0, "No keyboard device nodes created"

    # Give system time to register the device
    time.sleep(0.1)

    try:
        # Simulate Ctrl+A (select all)
        keyboard.press(KeyCode.CTRL)
        keyboard.press(KeyCode.A)
        time.sleep(0.1)
        keyboard.release(KeyCode.A)
        keyboard.release(KeyCode.CTRL)

    except Exception as e:
        raise RuntimeError(f"Failed to simulate keyboard input: {e}") from e
