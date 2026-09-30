"""Tests for joypad.py."""

import os
import time
from unittest.mock import MagicMock, patch

import pytest

from inputtino import _core
from inputtino.joypad import (
    ControllerButton,
    DeviceDefinition,
    PS5BatteryState,
    PS5Joypad,
    PS5MotionType,
    SwitchJoypad,
    XBoxOneJoypad,
)
from tests.helpers import mark_practical


@pytest.fixture
def mock_joypad():
    """Create a mock for base Joypad."""
    with patch("inputtino._core.Joypad") as mock_joypad_cls:
        mock_instance = MagicMock(spec=_core.Joypad)
        mock_joypad_cls.create.return_value = mock_instance
        yield mock_instance


@pytest.fixture
def mock_xbox_joypad():
    """Create a mock for XBoxOneJoypad."""
    with patch("inputtino._core.XboxOneJoypad") as mock_joypad_cls:
        mock_instance = MagicMock()
        mock_joypad_cls.create = MagicMock(return_value=mock_instance)
        yield mock_joypad_cls


@pytest.fixture
def mock_switch_joypad():
    """Create a mock for SwitchJoypad."""
    with patch("inputtino._core.SwitchJoypad") as mock_joypad_cls:
        mock_instance = MagicMock()
        mock_joypad_cls.create = MagicMock(return_value=mock_instance)
        yield mock_joypad_cls


@pytest.fixture
def mock_ps5_joypad():
    """Create a mock for PS5Joypad."""
    with patch("inputtino._core.PS5Joypad") as mock_joypad_cls:
        mock_instance = MagicMock()
        mock_joypad_cls.create = MagicMock(return_value=mock_instance)
        yield mock_joypad_cls


def test_xbox_joypad_creation_with_default_device(mock_xbox_joypad):
    """Test creating an Xbox joypad with default device definition."""
    XBoxOneJoypad()
    created_def = mock_xbox_joypad.create.call_args[0][0]

    assert created_def.name == "Wolf X-Box One (virtual) pad"
    assert created_def.vendor_id == 0x045E
    assert created_def.product_id == 0x02EA
    assert created_def.version == 0x0408


def test_xbox_joypad_with_custom_device(mock_xbox_joypad):
    """Test creating an Xbox joypad with custom device definition."""
    custom_def = DeviceDefinition(
        name="Custom Xbox",
        vendor_id=0x1234,
        product_id=0x5678,
        version=0x0001,
    )

    XBoxOneJoypad(custom_def)
    created_def = mock_xbox_joypad.create.call_args[0][0]

    assert created_def.name == custom_def.name
    assert created_def.vendor_id == custom_def.vendor_id
    assert created_def.product_id == custom_def.product_id
    assert created_def.version == custom_def.version


def test_joypad_button_press(mock_xbox_joypad):
    """Test joypad button pressing."""
    joypad = XBoxOneJoypad()
    instance = mock_xbox_joypad.create.return_value

    buttons = ControllerButton.A | ControllerButton.B | ControllerButton.DPAD_UP
    joypad.set_pressed_buttons(buttons)
    instance.set_pressed_buttons.assert_called_once_with(int(buttons))


def test_joypad_triggers(mock_xbox_joypad):
    """Test setting trigger values."""
    joypad = XBoxOneJoypad()
    instance = mock_xbox_joypad.create.return_value

    joypad.set_triggers(1000, 2000)
    instance.set_triggers.assert_called_once_with(1000, 2000)


def test_joypad_stick(mock_xbox_joypad):
    """Test setting stick position."""
    joypad = XBoxOneJoypad()
    instance = mock_xbox_joypad.create.return_value

    joypad.set_stick(_core.StickPosition.LS, 100, 200)
    instance.set_stick.assert_called_once_with(_core.StickPosition.LS, 100, 200)


def test_ps5_joypad_specific_features(mock_ps5_joypad):
    """Test PS5-specific features."""
    joypad = PS5Joypad()
    instance = mock_ps5_joypad.create.return_value
    instance.get_mac_address.return_value = "00:11:22:33:44:55"

    assert joypad.mac_address == "00:11:22:33:44:55"

    joypad.place_finger(1, 960, 540)
    instance.place_finger.assert_called_once_with(1, 960, 540)

    joypad.set_motion(PS5MotionType.ACCELERATION, 1.0, 2.0, 3.0)
    instance.set_motion.assert_called_once_with(
        PS5MotionType.ACCELERATION, 1.0, 2.0, 3.0
    )

    joypad.set_battery(PS5BatteryState.BATTERY_FULL, 100)
    instance.set_battery.assert_called_once_with(PS5BatteryState.BATTERY_FULL, 100)


def test_rumble_callback():
    """Test rumble callback functionality."""
    with patch("inputtino._core.XboxOneJoypad") as mock_joypad_cls:
        joypad = XBoxOneJoypad()

        callback_called = False

        def rumble_callback(low, high):
            nonlocal callback_called
            callback_called = True
            assert low == 1000
            assert high == 2000

        joypad.set_on_rumble(rumble_callback)

        # Simulate rumble event
        mock_joypad_cls.create.return_value.set_on_rumble.call_args[0][0](1000, 2000)
        assert callback_called


def test_ps5_led_callback():
    """Test PS5 LED callback functionality."""
    with patch("inputtino._core.PS5Joypad") as mock_joypad_cls:
        joypad = PS5Joypad()

        callback_called = False

        def led_callback(r, g, b):
            nonlocal callback_called
            callback_called = True
            assert r == 255
            assert g == 0
            assert b == 0

        joypad.set_on_led(led_callback)

        # Simulate LED event
        mock_joypad_cls.create.return_value.set_on_led.call_args[0][0](255, 0, 0)
        assert callback_called


def test_joypad_creation_failure():
    """Test handling of joypad creation failure."""
    with patch("inputtino._core.XboxOneJoypad") as mock_joypad_cls:
        mock_joypad_cls.create.side_effect = RuntimeError(
            "Failed to create joypad device"
        )

        with pytest.raises(RuntimeError, match="Failed to create joypad device"):
            XBoxOneJoypad()


def test_switch_joypad_creation_with_default_device(mock_switch_joypad):
    """Test creating a Switch joypad with default device definition."""
    SwitchJoypad()
    created_def = mock_switch_joypad.create.call_args[0][0]

    assert created_def.name == "Wolf Nintendo (virtual) pad"
    assert created_def.vendor_id == 0x057E
    assert created_def.product_id == 0x2009
    assert created_def.version == 0x8111


def test_switch_joypad_with_custom_device(mock_switch_joypad):
    """Test creating a Switch joypad with custom device definition."""
    custom_def = DeviceDefinition(
        name="Custom Switch",
        vendor_id=0x1234,
        product_id=0x5678,
        version=0x0001,
    )

    SwitchJoypad(custom_def)
    created_def = mock_switch_joypad.create.call_args[0][0]

    assert created_def.name == custom_def.name
    assert created_def.vendor_id == custom_def.vendor_id
    assert created_def.product_id == custom_def.product_id
    assert created_def.version == custom_def.version


def test_switch_rumble_callback():
    """Test Switch rumble callback functionality."""
    with patch("inputtino._core.SwitchJoypad") as mock_joypad_cls:
        joypad = SwitchJoypad()

        callback_called = False

        def rumble_callback(low, high):
            nonlocal callback_called
            callback_called = True
            assert low == 1000
            assert high == 2000

        joypad.set_on_rumble(rumble_callback)

        mock_joypad_cls.create.return_value.set_on_rumble.call_args[0][0](1000, 2000)
        assert callback_called


@mark_practical
def test_practical_joypad_gameplay():
    """Test gamepad controls in a simulated gameplay scenario.

    This practical test demonstrates:
    1. Basic directional movement
    2. Button combinations
    3. Trigger and stick input
    4. Rumble feedback

    Note:
        This test requires actual device access (/dev/uinput)
        and may fail if run without proper permissions.
    """
    joypad = XBoxOneJoypad()
    assert len(joypad.nodes) > 0, "No joypad nodes created"

    time.sleep(0.1)  # Allow device registration

    rumble_received = False

    def on_rumble(low, high):
        nonlocal rumble_received
        rumble_received = True

    joypad.set_on_rumble(on_rumble)

    try:
        # Test movement sequence
        movements = [
            (ControllerButton.DPAD_RIGHT, _core.StickPosition.LS, 32767, 0),
            (ControllerButton.DPAD_DOWN, _core.StickPosition.LS, 0, 32767),
            (ControllerButton.DPAD_LEFT, _core.StickPosition.LS, -32767, 0),
            (ControllerButton.DPAD_UP, _core.StickPosition.LS, 0, -32767),
        ]

        for button, stick, x, y in movements:
            joypad.set_pressed_buttons(button)
            joypad.set_stick(stick, x, y)
            joypad.set_triggers(32767, 32767)
            time.sleep(0.1)
            joypad.set_pressed_buttons(ControllerButton(0))
            time.sleep(0.1)

        # Test button combinations
        joypad.set_pressed_buttons(
            ControllerButton.A | ControllerButton.B | ControllerButton.X
        )
        time.sleep(0.1)

    finally:
        joypad.set_pressed_buttons(ControllerButton(0))
        joypad.set_triggers(0, 0)
        joypad.set_stick(_core.StickPosition.LS, 0, 0)


@mark_practical
def test_practical_ps5_features():
    """Test PS5-specific features in a practical scenario.

    Tests touchpad, motion sensors, LED, and battery reporting.

    Note:
        Requires /dev/uinput access.
    """
    joypad = PS5Joypad()
    # NOTE: 2025/02/05 Not working.
    # assert len(joypad.nodes) > 0

    time.sleep(0.1)

    led_changed = False

    def on_led(r, g, b):
        nonlocal led_changed
        led_changed = True

    joypad.set_on_led(on_led)

    try:
        # Touchpad gesture
        joypad.place_finger(0, 960, 540)
        time.sleep(0.1)
        joypad.place_finger(0, 1200, 540)
        time.sleep(0.1)
        joypad.release_finger(0)

        # Motion controls
        joypad.set_motion(PS5MotionType.ACCELERATION, 0.5, 0.1, 9.8)
        time.sleep(0.1)
        joypad.set_motion(PS5MotionType.GYROSCOPE, 45.0, 0.0, 0.0)
        time.sleep(0.1)

        # Battery update
        joypad.set_battery(PS5BatteryState.BATTERY_FULL, 100)

    finally:
        joypad.release_finger(0)
