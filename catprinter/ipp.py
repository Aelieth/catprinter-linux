"""Minimal IPP 1.1 for a localhost CUPS queue.

Enough for cupsd's `ipp` backend: Get-Printer-Attributes, Validate-Job,
Print-Job, Create-Job, Send-Document, Cancel-Job, Get-Job-Attributes,
Get-Jobs. Not a general IPP server.
"""

from __future__ import annotations

import struct
import time
from dataclasses import dataclass, field

from catprinter import __version__

# Operation IDs
OP_PRINT_JOB = 0x0002
OP_VALIDATE_JOB = 0x0004
OP_CREATE_JOB = 0x0005
OP_SEND_DOCUMENT = 0x0006
OP_CANCEL_JOB = 0x0008
OP_GET_JOB_ATTRIBUTES = 0x0009
OP_GET_JOBS = 0x000A
OP_GET_PRINTER_ATTRIBUTES = 0x000B

# Status
STATUS_OK = 0x0000
STATUS_CLIENT_BAD_REQUEST = 0x0400
STATUS_CLIENT_NOT_FOUND = 0x0406
STATUS_CLIENT_NOT_POSSIBLE = 0x0404
STATUS_SERVER_INTERNAL = 0x0500
STATUS_SERVER_BUSY = 0x0507

# Delimiter tags
TAG_OPERATION = 0x01
TAG_JOB = 0x02
TAG_END = 0x03
TAG_PRINTER = 0x04
TAG_UNSUPPORTED = 0x05

# Value tags
TAG_INTEGER = 0x21
TAG_BOOLEAN = 0x22
TAG_ENUM = 0x23
TAG_RESOLUTION = 0x32
TAG_KEYWORD = 0x44
TAG_URI = 0x45
TAG_CHARSET = 0x47
TAG_LANGUAGE = 0x48
TAG_MIME = 0x49
TAG_TEXT = 0x41
TAG_NAME = 0x42

PRINTER_IDLE = 3
PRINTER_PROCESSING = 4
PRINTER_STOPPED = 5

JOB_PENDING = 3
JOB_PROCESSING = 5
JOB_CANCELED = 7
JOB_ABORTED = 8
JOB_COMPLETED = 9


@dataclass
class IppAttribute:
    tag: int
    name: str
    values: list[bytes]


@dataclass
class IppRequest:
    version: tuple[int, int]
    operation: int
    request_id: int
    attributes: dict[str, IppAttribute] = field(default_factory=dict)
    document: bytes = b""

    def get(self, name: str, default: str | None = None) -> str | None:
        attr = self.attributes.get(name)
        if not attr or not attr.values:
            return default
        return attr.values[0].decode("utf-8", "replace")

    def get_int(self, name: str, default: int | None = None) -> int | None:
        attr = self.attributes.get(name)
        if not attr or not attr.values:
            return default
        raw = attr.values[0]
        if len(raw) == 4:
            return int.from_bytes(raw, "big", signed=True)
        return default


def parse_ipp(data: bytes) -> IppRequest:
    if len(data) < 8:
        raise ValueError("IPP message too short")
    version = (data[0], data[1])
    operation = int.from_bytes(data[2:4], "big")
    request_id = int.from_bytes(data[4:8], "big")
    attrs: dict[str, IppAttribute] = {}
    i = 8
    last_name = ""
    while i < len(data):
        tag = data[i]
        i += 1
        if tag == TAG_END:
            break
        if tag in (TAG_OPERATION, TAG_JOB, TAG_PRINTER, TAG_UNSUPPORTED):
            last_name = ""
            continue
        if i + 2 > len(data):
            break
        name_len = int.from_bytes(data[i : i + 2], "big")
        i += 2
        name = data[i : i + name_len].decode("utf-8", "replace")
        i += name_len
        if i + 2 > len(data):
            break
        value_len = int.from_bytes(data[i : i + 2], "big")
        i += 2
        value = data[i : i + value_len]
        i += value_len
        if not name:
            name = last_name
        else:
            last_name = name
        if not name:
            continue
        if name in attrs:
            attrs[name].values.append(value)
        else:
            attrs[name] = IppAttribute(tag=tag, name=name, values=[value])
    return IppRequest(
        version=version,
        operation=operation,
        request_id=request_id,
        attributes=attrs,
        document=data[i:],
    )


def _enc_attr(tag: int, name: str, value: bytes) -> bytes:
    name_b = name.encode("utf-8")
    return bytes([tag]) + struct.pack(">H", len(name_b)) + name_b + struct.pack(">H", len(value)) + value


def _enc_more(tag: int, value: bytes) -> bytes:
    return bytes([tag]) + struct.pack(">H", 0) + struct.pack(">H", len(value)) + value


def enc_text(name: str, value: str, tag: int = TAG_KEYWORD) -> bytes:
    return _enc_attr(tag, name, value.encode("utf-8"))


def enc_texts(name: str, values: list[str], tag: int = TAG_KEYWORD) -> bytes:
    if not values:
        return b""
    out = _enc_attr(tag, name, values[0].encode("utf-8"))
    for extra in values[1:]:
        out += _enc_more(tag, extra.encode("utf-8"))
    return out


def enc_int(name: str, value: int, tag: int = TAG_INTEGER) -> bytes:
    return _enc_attr(tag, name, struct.pack(">i", value))


def enc_bool(name: str, value: bool) -> bytes:
    return _enc_attr(TAG_BOOLEAN, name, bytes([1 if value else 0]))


def enc_res(name: str, dpi: int) -> bytes:
    return _enc_attr(TAG_RESOLUTION, name, struct.pack(">iiB", dpi, dpi, 3))


def build_response(
    request: IppRequest,
    status: int,
    groups: list[tuple[int, bytes]],
) -> bytes:
    out = bytes([request.version[0], request.version[1]])
    out += struct.pack(">H", status)
    out += struct.pack(">I", request.request_id)
    out += bytes([TAG_OPERATION])
    out += enc_text("attributes-charset", "utf-8", TAG_CHARSET)
    out += enc_text("attributes-natural-language", "en", TAG_LANGUAGE)
    for tag, payload in groups:
        out += bytes([tag])
        out += payload
    out += bytes([TAG_END])
    return out


def quality_from_request(request: IppRequest) -> str:
    """Map IPP / PPD options onto default | picture | text."""
    cat = request.get("CatQuality") or request.get("catquality")
    if cat:
        return cat.strip().lower()
    optimize = (request.get("print-content-optimize") or "").lower()
    if optimize in {"photo", "graphics", "image"}:
        return "picture"
    if optimize == "text":
        return "text"
    if optimize in {"auto", "text-and-graphics"}:
        return "default"
    pq = request.get_int("print-quality")
    if pq == 5:
        return "picture"
    if pq == 3:
        return "text"
    return "default"


def tone_from_request(request: IppRequest) -> str:
    """Map IPP / PPD CatTone onto blackwhite | grayscale."""
    cat = request.get("CatTone") or request.get("cattone")
    if cat:
        return cat.strip().lower()
    return "blackwhite"


def media_type_from_request(request: IppRequest) -> str:
    """Map PPD / IPP media type onto paper | sticker. Label only."""
    cat = request.get("CatMediaType") or request.get("catmediatype")
    if not cat:
        cat = request.get("MediaType") or request.get("mediatype")
    if cat:
        key = cat.strip().lower()
        if key in {"sticker", "label", "labels"}:
            return "sticker"
        if key in {"paper", "stationery", "plain"}:
            return "paper"
    itype = (request.get("media-type") or "").lower()
    if itype in {"labels", "label", "stationery-coated"}:
        return "sticker"
    return "paper"


def printer_attributes(
    *,
    printer_uri: str,
    state: int,
    reasons: list[str],
    accepting: bool,
    queued: int,
    uptime: int,
    name: str = "CatPrinter",
) -> bytes:
    body = b""
    body += enc_texts("printer-uri-supported", [printer_uri], TAG_URI)
    body += enc_text("uri-security-supported", "none")
    body += enc_text("uri-authentication-supported", "none")
    body += enc_text("printer-name", name, TAG_NAME)
    body += enc_text("printer-info", "Cat Printer MXW01", TAG_TEXT)
    body += enc_text("printer-make-and-model", f"MXW01 (catprinter {__version__})", TAG_TEXT)
    body += enc_text("printer-location", "Bluetooth", TAG_TEXT)
    body += enc_int("printer-state", state, TAG_ENUM)
    body += enc_texts("printer-state-reasons", reasons or ["none"])
    body += enc_bool("printer-is-accepting-jobs", accepting)
    body += enc_int("queued-job-count", queued)
    body += enc_texts("ipp-versions-supported", ["1.1", "2.0"])
    body += enc_int("operations-supported", OP_PRINT_JOB, TAG_ENUM)
    for op in (
        OP_VALIDATE_JOB,
        OP_CREATE_JOB,
        OP_SEND_DOCUMENT,
        OP_CANCEL_JOB,
        OP_GET_JOB_ATTRIBUTES,
        OP_GET_JOBS,
        OP_GET_PRINTER_ATTRIBUTES,
    ):
        body += _enc_more(TAG_ENUM, struct.pack(">i", op))
    body += enc_text("charset-configured", "utf-8", TAG_CHARSET)
    body += enc_texts("charset-supported", ["utf-8"], TAG_CHARSET)
    body += enc_text("natural-language-configured", "en", TAG_LANGUAGE)
    body += enc_texts("generated-natural-language-supported", ["en"], TAG_LANGUAGE)
    body += enc_text("document-format-default", "application/pdf", TAG_MIME)
    body += enc_texts(
        "document-format-supported",
        ["application/pdf", "image/jpeg", "image/png", "image/pwg-raster"],
        TAG_MIME,
    )
    body += enc_text("pdl-override-supported", "attempted")
    body += enc_int("printer-up-time", max(1, uptime))
    body += enc_texts("compression-supported", ["none"])
    body += enc_text("media-default", "om_cat-tape_48x297mm")
    body += enc_texts(
        "media-supported",
        [
            "om_cat-tape_48x297mm",
            "om_cat-tape_48x500mm",
            "iso_a4_210x297mm",
            "na_letter_8.5x11in",
        ],
    )
    body += enc_text("media-type-default", "stationery")
    body += enc_texts("media-type-supported", ["stationery", "labels"])
    body += enc_text("media-source-default", "roll")
    body += enc_texts("media-source-supported", ["roll"])
    body += enc_int("media-left-margin-supported", 0)
    body += enc_int("media-right-margin-supported", 0)
    body += enc_int("media-top-margin-supported", 0)
    body += enc_int("media-bottom-margin-supported", 0)
    body += enc_res("printer-resolution-default", 203)
    body += enc_res("printer-resolution-supported", 203)
    body += enc_int("copies-default", 1)
    body += enc_int("copies-supported", 1)  # range would be better; 1 is fine
    body += enc_text("print-color-mode-default", "monochrome")
    body += enc_texts("print-color-mode-supported", ["monochrome"])
    body += enc_text("print-content-optimize-default", "auto")
    body += enc_texts(
        "print-content-optimize-supported",
        ["auto", "photo", "text", "graphics", "text-and-graphics"],
    )
    body += enc_int("print-quality-default", 4, TAG_ENUM)
    body += enc_int("print-quality-supported", 3, TAG_ENUM)
    body += _enc_more(TAG_ENUM, struct.pack(">i", 4))
    body += _enc_more(TAG_ENUM, struct.pack(">i", 5))
    body += enc_texts("job-creation-attributes-supported", [
        "copies",
        "document-format",
        "media",
        "print-content-optimize",
        "print-quality",
        "CatQuality",
        "CatTone",
        "CatMediaType",
        "media-type",
        "job-name",
    ])
    body += enc_text("which-jobs-supported", "completed")
    body += _enc_more(TAG_KEYWORD, b"not-completed")
    return body


def job_attributes(
    *,
    job_id: int,
    job_uri: str,
    printer_uri: str,
    state: int,
    name: str,
    impressions: int = 0,
) -> bytes:
    body = b""
    body += enc_int("job-id", job_id)
    body += enc_text("job-uri", job_uri, TAG_URI)
    body += enc_text("job-printer-uri", printer_uri, TAG_URI)
    body += enc_int("job-state", state, TAG_ENUM)
    reasons = {
        JOB_PENDING: "none",
        JOB_PROCESSING: "job-printing",
        JOB_COMPLETED: "job-completed-successfully",
        JOB_CANCELED: "job-canceled-by-user",
        JOB_ABORTED: "aborted-by-system",
    }
    body += enc_text("job-state-reasons", reasons.get(state, "none"))
    body += enc_text("job-name", name or "print", TAG_NAME)
    body += enc_int("job-impressions-completed", impressions)
    return body


def now_uptime(started: float) -> int:
    return max(1, int(time.time() - started))
