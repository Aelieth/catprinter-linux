from __future__ import annotations

import logging
import os
import shutil

__version__ = "0.1.0"

logger = logging.getLogger("catprinter")

# Host prefixes as well as $PATH: inside a toolbox or distrobox the host's
# binaries live under /run/host. This lives HERE rather than in render.py so a
# health check can call it without first importing numpy and Pillow — which are
# two of the things the health check exists to report on.
_HOST_BIN_PREFIXES = (
    "/usr/bin",
    "/usr/sbin",
    "/run/host/usr/bin",
    "/run/host/usr/sbin",
)


def which(name: str) -> str | None:
    found = shutil.which(name)
    if found:
        return found
    for prefix in _HOST_BIN_PREFIXES:
        path = os.path.join(prefix, name)
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    return None


# Kid-facing messages, not BLE transport details — and _printer_state needs
# NOT_FOUND on every Get-Printer-Attributes. Defining them in ble.py meant
# reading a string cost you an import of bleak.
NOT_FOUND = "Turn the cat printer on and make sure Bluetooth is on."
NO_BLUEZ = (
    "Cannot talk to Bluetooth on this computer. "
    "Is Bluetooth enabled? On Fedora Silverblue/toolbox, BlueZ lives on the host."
)
