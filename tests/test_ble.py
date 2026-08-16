import unittest
from unittest.mock import AsyncMock, patch

from catprinter import protocol as proto
from catprinter.ble import (
    MXW01,
    _pick_known_props,
    _props_look_like_mxw01,
)


class KnownDeviceMatchTests(unittest.TestCase):
    def test_matches_name(self):
        self.assertTrue(_props_look_like_mxw01({"Name": "MXW01", "Address": "AA:BB"}))
        self.assertTrue(_props_look_like_mxw01({"Alias": "MXW01", "Address": "AA:BB"}))
        self.assertFalse(_props_look_like_mxw01({"Name": "Headphones", "Address": "AA:BB"}))

    def test_matches_service_uuid_without_name(self):
        self.assertTrue(
            _props_look_like_mxw01(
                {"Address": "AA:BB", "UUIDs": [proto.MAIN_SERVICE_UUID]}
            )
        )
        self.assertTrue(
            _props_look_like_mxw01(
                {"Address": "AA:BB", "UUIDs": [proto.MAIN_SERVICE_UUID_ALT]}
            )
        )

    def test_address_hint_wins(self):
        props = {"Name": "Headphones", "Address": "48:0F:57:17:06:9D"}
        self.assertTrue(_props_look_like_mxw01(props, address_hint="48:0F:57:17:06:9D"))
        self.assertFalse(_props_look_like_mxw01(props, address_hint="00:11:22:33:44:55"))

    def test_wanted_name(self):
        props = {"Name": "KitchenCat", "Address": "AA:BB"}
        self.assertTrue(_props_look_like_mxw01(props, wanted_name="KitchenCat"))
        self.assertFalse(_props_look_like_mxw01(props, wanted_name="MXW01"))

    def test_prefer_already_connected(self):
        idle = ("/dev/idle", {"Address": "AA", "Connected": False, "RSSI": -40})
        held = ("/dev/held", {"Address": "BB", "Connected": True, "RSSI": -90})
        path, props = _pick_known_props([idle, held])
        self.assertEqual(path, "/dev/held")
        self.assertEqual(props["Address"], "BB")

    def test_prefer_stronger_rssi_when_neither_connected(self):
        weak = ("/dev/weak", {"Address": "AA", "Connected": False, "RSSI": -90})
        strong = ("/dev/strong", {"Address": "BB", "Connected": False, "RSSI": -50})
        path, _props = _pick_known_props([weak, strong])
        self.assertEqual(path, "/dev/strong")

    def test_empty(self):
        self.assertIsNone(_pick_known_props([]))


class FakeClient:
    def __init__(self) -> None:
        self.is_connected = True
        self.disconnected = False
        self.stopped = False

    async def stop_notify(self, _uuid) -> None:
        self.stopped = True

    async def disconnect(self) -> None:
        self.disconnected = True
        self.is_connected = False


class SessionDisconnectTests(unittest.IsolatedAsyncioTestCase):
    async def test_aexit_disconnects_after_print_failure(self):
        printer = MXW01()
        fake = FakeClient()
        printer.client = fake
        with patch("catprinter.ble.asyncio.sleep", new=AsyncMock()):
            await printer.__aexit__(RuntimeError, RuntimeError("print failed"), None)
        self.assertTrue(fake.disconnected)
        self.assertTrue(fake.stopped)
        self.assertIsNone(printer.client)

    async def test_aexit_releases_bluez_even_if_settings_held_the_link(self):
        printer = MXW01()
        fake = FakeClient()
        printer.client = fake
        printer._device_path = "/org/bluez/hci0/dev_48_0F_57_17_06_9D"
        with (
            patch("catprinter.ble.asyncio.sleep", new=AsyncMock()),
            patch(
                "catprinter.ble._release_bluez_device", new=AsyncMock()
            ) as release,
        ):
            await printer.__aexit__(None, None, None)
        release.assert_awaited_once_with("/org/bluez/hci0/dev_48_0F_57_17_06_9D")
        self.assertTrue(fake.disconnected)
        self.assertIsNone(printer._device_path)


if __name__ == "__main__":
    unittest.main()

