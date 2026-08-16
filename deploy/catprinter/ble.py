"""BLE transport for the MXW01.

Connect only for the job, then drop the GATT link so the printer can walk
to the other room. BlueZ lies about MTU; we force a real negotiation.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
import uuid
from typing import Optional

from bleak import BleakClient, BleakScanner
from bleak.backends.device import BLEDevice
from bleak.backends.scanner import AdvertisementData
from bleak.exc import BleakError

from catprinter import NO_BLUEZ, NOT_FOUND, logger
from catprinter import protocol as proto

SCAN_TIMEOUT_S = 8.0
PACING_DELAY_S = 0.008
NOTIFICATION_TIMEOUT_S = 7.0
PRINT_COMPLETE_BASE_TIMEOUT_S = 15.0
PRINT_COMPLETE_LINES_PER_SEC = 15.0
# 8s cancelled in-flight BlueZ connects on a weak link and wedged the adapter.
# 12s is enough to fail fast vs the old 20s, without aborting a live page.
CONNECT_TIMEOUT_S = 12.0
CONNECT_ATTEMPTS = 3

# Toolbox / distrobox: the system bus is on the host, not in the container.
_HOST_SYSTEM_BUS = "unix:path=/run/host/run/dbus/system_bus_socket"


def _ensure_system_bus() -> None:
    if os.environ.get("DBUS_SYSTEM_BUS_ADDRESS"):
        return
    if os.path.exists("/run/dbus/system_bus_socket") or os.path.exists(
        "/var/run/dbus/system_bus_socket"
    ):
        return
    if os.path.exists("/run/host/run/dbus/system_bus_socket"):
        os.environ["DBUS_SYSTEM_BUS_ADDRESS"] = _HOST_SYSTEM_BUS
        logger.debug("Using host system bus at %s", _HOST_SYSTEM_BUS)


async def _force_bluez_mtu(client: BleakClient) -> None:
    """BlueZ reports a fake MTU of 23 unless we poke it.

    Works across Bleak versions: older ones expose _acquire_mtu on the
    client, newer ones hide it on the BlueZ backend.
    """
    candidates = [
        getattr(client, "_acquire_mtu", None),
        getattr(getattr(client, "_backend", None), "_acquire_mtu", None),
    ]
    for fn in candidates:
        if not callable(fn):
            continue
        try:
            await fn()
            logger.debug("Forced BlueZ MTU negotiation")
            return
        except Exception as exc:  # noqa: BLE001 — private API, best-effort
            logger.debug("BlueZ MTU acquire failed: %s", exc)


def _looks_like_address(value: str) -> bool:
    with contextlib.suppress(ValueError):
        uuid.UUID(value)
        return True
    return value.count(":") == 5 and value.replace(":", "").isalnum()


def _is_mxw01(device: BLEDevice, adv: AdvertisementData) -> bool:
    name = (device.name or adv.local_name or "").strip()
    if name.upper() == "MXW01":
        return True
    advertised = {u.lower() for u in (adv.service_uuids or [])}
    return any(svc.lower() in advertised for svc in proto.SERVICE_UUIDS)


def _props_look_like_mxw01(
    props: dict,
    address_hint: Optional[str] = None,
    wanted_name: Optional[str] = None,
) -> bool:
    """Match a BlueZ Device1 property dict. No advertisement required.

    Settings "Connect" keeps the MXW01 in BlueZ's object tree and it stops
    advertising. Scan-only discovery then lies and says the printer is off.
    """
    address = (props.get("Address") or "").lower()
    name = (props.get("Name") or props.get("Alias") or "").strip()
    uuids = {str(u).lower() for u in (props.get("UUIDs") or [])}
    if address_hint:
        return address == address_hint.lower()
    if wanted_name:
        return name == wanted_name
    if name.upper() == "MXW01":
        return True
    return any(svc.lower() in uuids for svc in proto.SERVICE_UUIDS)


def _pick_known_props(candidates: list[tuple[str, dict]]) -> tuple[str, dict] | None:
    """Prefer an already-connected MXW01, then the strongest RSSI."""
    if not candidates:
        return None

    def key(item: tuple[str, dict]) -> tuple[int, int]:
        props = item[1]
        connected = 1 if props.get("Connected") else 0
        rssi = props.get("RSSI")
        if not isinstance(rssi, int):
            rssi = -999
        return (connected, rssi)

    return max(candidates, key=key)


def _ble_device_from_bluez(path: str, props: dict) -> BLEDevice:
    name = props.get("Name") or props.get("Alias") or "MXW01"
    return BLEDevice(props["Address"], name, {"path": path, "props": props})


async def _release_bluez_device(path: str) -> None:
    """Drop BlueZ's Device connection if Settings (or we) left it held.

    BleakClient.disconnect() can release only our GATT session. This printer
    has one radio slot: if BlueZ still shows Connected, it will not advertise
    and the next job (or the other room) cannot find it.
    """
    if not path:
        return
    try:
        from bleak.backends.bluezdbus import defs
        from bleak.backends.bluezdbus.manager import get_global_bluez_manager
        from dbus_fast.message import Message
    except ImportError:
        return
    try:
        manager = await get_global_bluez_manager()
        if not manager.is_connected(path):
            return
        bus = getattr(manager, "_bus", None)
        if bus is None:
            return
        logger.info("🔌 BlueZ still holding the printer — releasing %s", path)
        await bus.call(
            Message(
                destination=defs.BLUEZ_SERVICE,
                interface=defs.DEVICE_INTERFACE,
                path=path,
                member="Disconnect",
            )
        )
    except Exception as exc:  # noqa: BLE001 — best-effort idle radio
        logger.debug("BlueZ Device.Disconnect failed: %s", exc)


async def _known_mxw01(
    address_hint: Optional[str] = None,
    wanted_name: Optional[str] = None,
) -> tuple[BLEDevice, int, bool] | None:
    """Return a cached BlueZ MXW01 if the adapter already knows it."""
    try:
        from bleak.backends.bluezdbus import defs
        from bleak.backends.bluezdbus.manager import get_global_bluez_manager
    except ImportError:
        return None
    try:
        manager = await get_global_bluez_manager()
    except Exception as exc:  # noqa: BLE001 — cache is a fast path, not required
        logger.debug("BlueZ manager unavailable: %s", exc)
        return None
    properties = getattr(manager, "_properties", None) or {}
    found: list[tuple[str, dict]] = []
    for path, ifaces in properties.items():
        props = (ifaces or {}).get(defs.DEVICE_INTERFACE)
        if not props:
            continue
        if _props_look_like_mxw01(props, address_hint, wanted_name):
            found.append((path, props))
    picked = _pick_known_props(found)
    if picked is None:
        return None
    path, props = picked
    rssi = props.get("RSSI")
    if not isinstance(rssi, int):
        rssi = -999
    return _ble_device_from_bluez(path, props), rssi, bool(props.get("Connected"))


def _fmt_ble_error(exc: BaseException) -> str:
    text = str(exc).strip()
    if not text:
        text = type(exc).__name__
        if isinstance(exc, TimeoutError):
            text = "connection timed out"
    return text


def _pick_candidate(
    discovered,
    wanted_name: Optional[str],
) -> tuple[int, BLEDevice] | None:
    candidates: list[tuple[int, BLEDevice]] = []
    for dev, adv in discovered.values():
        if wanted_name:
            name = (dev.name or adv.local_name or "")
            if name != wanted_name:
                continue
        elif not _is_mxw01(dev, adv):
            continue
        candidates.append((adv.rssi if adv.rssi is not None else -999, dev))
    if not candidates:
        return None
    candidates.sort(key=lambda item: item[0], reverse=True)
    return candidates[0]


class NotificationHub:
    def __init__(self) -> None:
        self.received: dict[int, bytes] = {}
        self.events: dict[int, asyncio.Event] = {}

    def handler(self, _sender, data: bytearray) -> None:
        parsed = proto.parse_notification(bytes(data))
        if parsed is None:
            logger.debug("Ignoring non-MXW01 notify: %s", bytes(data).hex())
            return
        cmd_id, payload = parsed
        if cmd_id in (proto.CommandIDs.PRINT, proto.CommandIDs.PRINT_COMPLETE):
            logger.info("📡 0x%02X  %s", cmd_id, payload.hex())
        else:
            logger.debug("📡 0x%02X  %s", cmd_id, payload.hex())
        self.received[cmd_id] = payload
        event = self.events.get(cmd_id)
        if event is not None:
            event.set()

    async def wait(self, cmd_id: int, timeout: float) -> bytes:
        # Drop a stale reply from a previous command, then arm the waiter
        # so a notify that arrives after the write cannot be missed.
        self.received.pop(cmd_id, None)
        event = asyncio.Event()
        self.events[cmd_id] = event
        try:
            await asyncio.wait_for(event.wait(), timeout=timeout)
        except asyncio.TimeoutError as exc:
            raise TimeoutError(
                f"Printer did not answer command 0x{cmd_id:02X}"
            ) from exc
        finally:
            self.events.pop(cmd_id, None)
        return self.received.pop(cmd_id)


class MXW01:
    """One print-session: connect, do the work, disconnect in __aexit__."""

    def __init__(self, device: Optional[str] = None) -> None:
        self.device_hint = device
        self.client: Optional[BleakClient] = None
        self.hub = NotificationHub()
        self._control = proto.CONTROL_WRITE_UUID
        self._data = proto.DATA_WRITE_UUID
        self._notify = proto.NOTIFY_UUID
        self._data_char = None
        self._device_path: Optional[str] = None
        self.slow = False

    async def __aenter__(self) -> "MXW01":
        await self.connect()
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.disconnect()

    async def connect(self) -> None:
        _ensure_system_bus()
        wanted_name = None
        address_hint = None
        if self.device_hint:
            if _looks_like_address(self.device_hint):
                address_hint = self.device_hint
            else:
                wanted_name = self.device_hint.strip()

        logger.info("⏳ Looking for the cat printer...")
        try:
            cached = await _known_mxw01(address_hint, wanted_name)
            if cached is not None:
                target, rssi, already = cached
                where = "already connected" if already else "known to Bluetooth"
                logger.info("✅ Found %s (%s)  rssi=%s", target, where, rssi)
                try:
                    await self._gatt_connect(target, rssi, already_connected=already)
                    await self._bind_characteristics()
                    return
                except RuntimeError:
                    if already:
                        raise
                    logger.info("Cached printer did not answer. Scanning...")

            await self._scan_and_connect(address_hint, wanted_name)
            await self._bind_characteristics()
        except FileNotFoundError as exc:
            raise RuntimeError(NO_BLUEZ) from exc

    async def _scan_and_connect(
        self,
        address_hint: Optional[str],
        wanted_name: Optional[str],
    ) -> None:
        rssi = -999
        target: Optional[BLEDevice] = None
        async with BleakScanner() as scanner:
            deadline = asyncio.get_event_loop().time() + SCAN_TIMEOUT_S
            while asyncio.get_event_loop().time() < deadline:
                if address_hint:
                    for dev in scanner.discovered_devices:
                        if dev.address.lower() == address_hint.lower():
                            target = dev
                            break
                else:
                    picked = _pick_candidate(
                        scanner.discovered_devices_and_advertisement_data,
                        wanted_name,
                    )
                    if picked:
                        rssi, target = picked
                if target is not None:
                    break
                await asyncio.sleep(0.25)

            if target is None:
                raise RuntimeError(NOT_FOUND)

            logger.info("✅ Found %s  rssi=%s", target, rssi)
            # Connect while the scanner is still running. Stopping the scan
            # first is how BlueZ "loses" these toys and Connect page-timeouts.
            await self._gatt_connect(target, rssi, already_connected=False)

    async def _gatt_connect(
        self,
        target: BLEDevice,
        rssi: int,
        already_connected: bool,
    ) -> None:
        last_error: Optional[BaseException] = None
        weak = rssi != -999 and rssi < -80
        extra = "  (weak signal — put the printer next to the computer)" if weak else ""
        logger.info("⏳ Connecting to %s...%s", target, extra)

        for attempt in range(1, CONNECT_ATTEMPTS + 1):
            # pair=False: this printer does not need (and should not get) a
            # BlueZ bond. Settings "Pair" is unnecessary; "Connect" is fine
            # because we attach to that existing GATT link.
            client = BleakClient(target, timeout=CONNECT_TIMEOUT_S, pair=False)
            try:
                await client.connect()
                self.client = client
                details = getattr(target, "details", None) or {}
                path = details.get("path") if isinstance(details, dict) else None
                self._device_path = path if isinstance(path, str) else None
                return
            except (BleakError, TimeoutError, FileNotFoundError) as exc:
                last_error = exc
                logger.warning(
                    "connect attempt %s/%s failed: %s",
                    attempt,
                    CONNECT_ATTEMPTS,
                    _fmt_ble_error(exc),
                )
                with contextlib.suppress(Exception):
                    await client.disconnect()
                await asyncio.sleep(1.2)

        hint = ""
        if weak:
            hint = " It is far away (weak Bluetooth). Move it next to the computer."
        if already_connected:
            hint += (
                " Bluetooth Settings is holding the printer."
                " Turn the printer off and on, and do not leave it Connected in Settings."
            )
        else:
            hint += (
                " Close the phone app if it is open — the printer only"
                " allows one connection."
            )
        raise RuntimeError(
            f"Could not connect to the cat printer "
            f"({_fmt_ble_error(last_error)}).{hint}"
        ) from last_error

    async def _bind_characteristics(self) -> None:
        assert self.client is not None
        await _force_bluez_mtu(self.client)
        logger.info("✅ Connected  MTU=%s", self.client.mtu_size)

        service = None
        wanted = {u.lower() for u in proto.SERVICE_UUIDS}
        for svc in self.client.services:
            if svc.uuid.lower() in wanted:
                service = svc
                break
        if service is None:
            await self.disconnect()
            raise RuntimeError(
                "Connected, but this is not an MXW01 (missing AE30 service)."
            )

        control = service.get_characteristic(proto.CONTROL_WRITE_UUID)
        notify = service.get_characteristic(proto.NOTIFY_UUID)
        data = service.get_characteristic(proto.DATA_WRITE_UUID)
        if not all([control, notify, data]):
            await self.disconnect()
            raise RuntimeError(
                "Connected, but the MXW01 is missing AE01/AE02/AE03. "
                "This firmware is not one we know."
            )
        self._control = control.uuid
        self._notify = notify.uuid
        self._data = data.uuid
        self._data_char = data
        await self.client.start_notify(self._notify, self.hub.handler)
        logger.info("✅ Notifications on")

    async def disconnect(self) -> None:
        client = self.client
        path = self._device_path
        self.client = None
        self._device_path = None
        if client is None and not path:
            return
        if client is not None:
            with contextlib.suppress(Exception):
                if client.is_connected:
                    await client.stop_notify(self._notify)
            with contextlib.suppress(Exception):
                if client.is_connected:
                    await client.disconnect()
        if path:
            await _release_bluez_device(path)
        logger.info("🔌 Disconnected")
        # Cheap LE toys stop advertising until the link is fully gone.
        await asyncio.sleep(0.8)

    async def _write_ctrl(self, packet: bytes) -> None:
        assert self.client is not None
        await self.client.write_gatt_char(self._control, packet, response=False)

    async def _write_data(self, chunk: bytes) -> None:
        assert self.client is not None
        await self.client.write_gatt_char(self._data, chunk, response=False)

    async def set_intensity(self, intensity: int) -> None:
        await self._write_ctrl(proto.cmd_set_intensity(intensity))
        await asyncio.sleep(0.05)

    async def get_status(self) -> proto.PrinterStatus:
        await self._write_ctrl(proto.cmd_get_status())
        payload = await self.hub.wait(proto.CommandIDs.GET_STATUS, NOTIFICATION_TIMEOUT_S)
        status = proto.parse_status(payload)
        logger.info("📋 %s  raw=%s", status.kid_message(), payload.hex())
        return status

    async def eject(self, line_count: int) -> None:
        await self._write_ctrl(proto.cmd_eject(line_count))
        await asyncio.sleep(0.2)

    async def retract(self, line_count: int) -> None:
        await self._write_ctrl(proto.cmd_retract(line_count))
        await asyncio.sleep(0.2)

    def _att_payload_limit(self) -> int:
        """BlueZ often leaves Characteristic.max_write_* stuck at 20 after we
        force a real MTU. Trust the larger of the two."""
        assert self.client is not None
        sizes = [20]
        char = self._data_char
        if char is not None:
            size = getattr(char, "max_write_without_response_size", None)
            if isinstance(size, int) and size > 0:
                sizes.append(size)
        mtu = getattr(self.client, "mtu_size", 23) or 23
        sizes.append(max(20, int(mtu) - 3))
        return max(sizes)

    async def print_image(
        self,
        image_data: bytes,
        intensity: int = proto.DEFAULT_INTENSITY,
        slow: bool = False,
        mode: int = proto.PrintModes.MONOCHROME,
    ) -> None:
        row_bytes = proto.bytes_per_row(mode)
        if len(image_data) % row_bytes != 0:
            raise ValueError(
                f"image buffer length {len(image_data)} is not a multiple of "
                f"{row_bytes}"
            )
        line_count = len(image_data) // row_bytes

        await self.set_intensity(intensity)
        status = await self.get_status()
        if not status.ok:
            raise RuntimeError(status.kid_message())

        tone = "4bpp" if mode == proto.PrintModes.GRAYSCALE else "1bpp"
        logger.info("🖨️  Print request  %s lines  %s", line_count, tone)
        await self._write_ctrl(proto.cmd_print_request(line_count, mode))
        ack = await self.hub.wait(proto.CommandIDs.PRINT, NOTIFICATION_TIMEOUT_S)
        if not ack or ack[0] != 0:
            raise RuntimeError(
                f"Printer rejected the print job ({ack.hex() if ack else 'no ack'})."
            )

        chunk = proto.data_chunk_size(
            self._att_payload_limit(),
            slow=slow or self.slow,
            row_bytes=row_bytes,
        )
        writes = (len(image_data) + chunk - 1) // chunk
        logger.info(
            "⏳ Sending %s bytes in %s writes (%s bytes/write)...",
            len(image_data),
            writes,
            chunk,
        )
        for offset in range(0, len(image_data), chunk):
            await self._write_data(image_data[offset : offset + chunk])
            await asyncio.sleep(PACING_DELAY_S)

        await self._write_ctrl(proto.cmd_flush())
        timeout = PRINT_COMPLETE_BASE_TIMEOUT_S + (line_count / PRINT_COMPLETE_LINES_PER_SEC)
        logger.info("⏳ Waiting for print to finish (%.0fs)...", timeout)
        try:
            await self.hub.wait(proto.CommandIDs.PRINT_COMPLETE, timeout)
            logger.info("✅ Printed")
        except TimeoutError:
            logger.warning("⚠️  No print-complete ping. It may still have printed.")


async def run_ble(
    image_data: bytes,
    device: Optional[str] = None,
    intensity: int = proto.DEFAULT_INTENSITY,
    slow: bool = False,
    mode: int = proto.PrintModes.MONOCHROME,
) -> None:
    try:
        async with MXW01(device) as printer:
            await printer.print_image(
                image_data, intensity=intensity, slow=slow, mode=mode
            )
    except (RuntimeError, BleakError, TimeoutError) as exc:
        logger.error("🛑 %s", exc)
        raise


async def run_status(device: Optional[str] = None) -> proto.PrinterStatus:
    async with MXW01(device) as printer:
        return await printer.get_status()


async def run_eject(line_count: int, device: Optional[str] = None, retract: bool = False) -> None:
    async with MXW01(device) as printer:
        if retract:
            await printer.retract(line_count)
        else:
            await printer.eject(line_count)
