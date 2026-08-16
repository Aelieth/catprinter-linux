#!/usr/bin/env python
"""Print an image (or check status / feed paper) on an MXW01 cat printer."""

import argparse
import asyncio
import logging
import os
import sys

_ROOT = os.path.dirname(os.path.realpath(__file__))
if _ROOT not in sys.path:
    sys.path.insert(0, _ROOT)

from catprinter import logger
from catprinter import protocol as proto
from catprinter.ble import MXW01, NOT_FOUND
from catprinter.render import render_to_buffer, write_preview


def parse_args():
    parser = argparse.ArgumentParser(
        description="Print to an MXW01 cat printer over Bluetooth"
    )
    parser.add_argument(
        "filename",
        nargs="?",
        help="File to print (PNG/JPEG/PDF). Not needed with --status/--eject.",
    )
    parser.add_argument(
        "-l",
        "--log-level",
        choices=["debug", "info", "warn", "error"],
        default="info",
    )
    parser.add_argument(
        "-b",
        "--dithering-algo",
        dest="dithering_algo",
        choices=["mean-threshold", "floyd-steinberg", "atkinson", "halftone", "none"],
        default=None,
        help=(
            "Override the quality preset's dither. "
            f"'none' requires a width of {proto.PRINTER_WIDTH_PIXELS} px."
        ),
    )
    parser.add_argument(
        "-s",
        "--show-preview",
        action="store_true",
        help="Write a tape preview (PNG + 48 mm PDF) and ask before printing.",
    )
    parser.add_argument(
        "--preview-only",
        action="store_true",
        help="Write the tape preview and exit without printing.",
    )
    parser.add_argument(
        "-d",
        "--device",
        default="",
        help=(
            "BLE address or advertisement name (default: auto-discover MXW01). "
            "Do not bake a MAC into scripts — walk the printer to the other room."
        ),
    )
    parser.add_argument(
        "-i",
        "--intensity",
        type=lambda x: int(x, 0),
        default=proto.DEFAULT_INTENSITY,
        help="Print darkness 0-255 (default 0x5D). Hex (0x5D) or decimal.",
    )
    parser.add_argument(
        "--top-first",
        action="store_true",
        help="Do not rotate 180°. Default is bottom-first so text comes out right-side up.",
    )
    parser.add_argument(
        "--status",
        action="store_true",
        help="Query battery / paper / temperature and exit.",
    )
    parser.add_argument(
        "--eject",
        type=int,
        metavar="LINES",
        help="Feed this many lines of paper and exit.",
    )
    parser.add_argument(
        "--retract",
        type=int,
        metavar="LINES",
        help="Retract this many lines of paper and exit.",
    )
    parser.add_argument(
        "--slow",
        action="store_true",
        help="Send one row per BLE write. Use only if a print comes out striped.",
    )
    parser.add_argument(
        "-q",
        "--quality",
        choices=["default", "picture", "text", "document"],
        default="default",
        help=(
            "default = mixed, picture = photos (darker), text = sharp threshold, "
            "document = shrink a whole A4/Letter page (no trim)."
        ),
    )
    parser.add_argument(
        "--tone",
        choices=["blackwhite", "grayscale"],
        default="blackwhite",
        help=(
            "blackwhite = 1 bit dither (default). "
            "grayscale = 16-level thermal print (best photos)."
        ),
    )
    parser.add_argument(
        "--no-trim",
        action="store_true",
        help="Do not crop white margins before scaling.",
    )
    return parser.parse_args()


def configure_logger(log_level):
    logger.setLevel(log_level)
    handler = logging.StreamHandler(sys.stdout)
    handler.setLevel(log_level)
    logger.addHandler(handler)


def _prepare_image(args):
    if not args.filename:
        raise RuntimeError("Give an image file, or use --status / --eject / --retract.")
    if not os.path.exists(args.filename):
        raise RuntimeError(f"File not found: {args.filename}")

    intensity = None if args.intensity == proto.DEFAULT_INTENSITY else args.intensity
    buffer, job = render_to_buffer(
        path=args.filename,
        quality=args.quality,
        tone=args.tone,
        dither=args.dithering_algo,
        trim=False if args.no_trim else None,
        intensity=intensity,
        rotate_180=not args.top_first,
    )
    logger.info(
        "✅ Ready to print: %s  quality=%s tone=%s dither=%s  %s bytes",
        job.bitmap.shape,
        job.quality,
        job.tone,
        job.dither,
        len(buffer),
    )
    if args.show_preview or args.preview_only:
        write_preview(job, rotate_180=not args.top_first)
        if args.preview_only:
            return buffer, job
        if input("🤔 Go ahead with print? [Y/n]? ").lower() == "n":
            raise RuntimeError("Aborted print.")
    return buffer, job


async def _run(args) -> int:
    device = args.device or None
    try:
        if args.status:
            async with MXW01(device) as printer:
                status = await printer.get_status()
                print(status.kid_message())
                return 0 if status.ok else 2

        if args.eject is not None or args.retract is not None:
            async with MXW01(device) as printer:
                if args.eject is not None:
                    await printer.eject(args.eject)
                if args.retract is not None:
                    await printer.retract(args.retract)
            return 0

        if args.preview_only:
            _prepare_image(args)
            return 0

        if args.show_preview:
            image_data, job = _prepare_image(args)
            async with MXW01(device) as printer:
                await printer.print_image(
                    image_data,
                    intensity=job.intensity,
                    slow=args.slow,
                    mode=job.mode,
                )
            return 0

        # Dither while BlueZ is scanning / connecting — connect is the long pole.
        prep = asyncio.create_task(asyncio.to_thread(_prepare_image, args))
        try:
            async with MXW01(device) as printer:
                image_data, job = await prep
                await printer.print_image(
                    image_data,
                    intensity=job.intensity,
                    slow=args.slow,
                    mode=job.mode,
                )
        except BaseException:
            prep.cancel()
            raise
        return 0
    except RuntimeError as exc:
        logger.error("🛑 %s", exc)
        if str(exc) == NOT_FOUND:
            return 3
        return 1


def main():
    args = parse_args()
    configure_logger(getattr(logging, args.log_level.upper()))
    try:
        raise SystemExit(asyncio.run(_run(args)))
    except KeyboardInterrupt:
        logger.error("🛑 Interrupted")
        raise SystemExit(130)


if __name__ == "__main__":
    main()
