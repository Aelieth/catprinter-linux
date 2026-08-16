import unittest

from catprinter.ipp import (
    OP_GET_PRINTER_ATTRIBUTES,
    OP_PRINT_JOB,
    STATUS_OK,
    TAG_OPERATION,
    build_response,
    enc_text,
    parse_ipp,
    media_type_from_request,
    printer_attributes,
    quality_from_request,
    tone_from_request,
)


def _minimal_request(operation=OP_GET_PRINTER_ATTRIBUTES, extra=b"", document=b""):
    body = bytes([0x01, 0x01])
    body += operation.to_bytes(2, "big")
    body += (1).to_bytes(4, "big")
    body += bytes([TAG_OPERATION])
    body += enc_text("attributes-charset", "utf-8", 0x47)
    body += enc_text("attributes-natural-language", "en", 0x48)
    body += extra
    body += bytes([0x03])
    body += document
    return body


class IppCodecTests(unittest.TestCase):
    def test_roundtrip_get_printer(self):
        raw = _minimal_request()
        req = parse_ipp(raw)
        self.assertEqual(req.operation, OP_GET_PRINTER_ATTRIBUTES)
        self.assertEqual(req.get("attributes-charset"), "utf-8")
        self.assertEqual(req.document, b"")

    def test_print_job_keeps_document(self):
        raw = _minimal_request(OP_PRINT_JOB, document=b"%PDF-1.4 fake")
        req = parse_ipp(raw)
        self.assertEqual(req.operation, OP_PRINT_JOB)
        self.assertTrue(req.document.startswith(b"%PDF"))

    def test_quality_mapping(self):
        extra = enc_text("CatQuality", "Picture", 0x42)
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(quality_from_request(req), "picture")

        extra = enc_text("print-content-optimize", "text")
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(quality_from_request(req), "text")

        extra = enc_text("CatQuality", "Document", 0x42)
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(quality_from_request(req), "document")

    def test_tone_mapping(self):
        extra = enc_text("CatTone", "Grayscale", 0x42)
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(tone_from_request(req), "grayscale")

        extra = enc_text("CatTone", "BlackWhite", 0x42)
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(tone_from_request(req), "blackwhite")

        req = parse_ipp(_minimal_request())
        self.assertEqual(tone_from_request(req), "blackwhite")

    def test_media_type_mapping(self):
        extra = enc_text("CatMediaType", "Sticker", 0x42)
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(media_type_from_request(req), "sticker")

        extra = enc_text("media-type", "labels")
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(media_type_from_request(req), "sticker")

        extra = enc_text("MediaType", "Paper", 0x42)
        req = parse_ipp(_minimal_request(extra=extra))
        self.assertEqual(media_type_from_request(req), "paper")

        req = parse_ipp(_minimal_request())
        self.assertEqual(media_type_from_request(req), "paper")

    def test_printer_attributes_are_tape_not_office_paper(self):
        body = printer_attributes(
            printer_uri="ipp://127.0.0.1:8095/ipp/print",
            state=3,
            reasons=["none"],
            accepting=True,
            queued=0,
            uptime=1,
        )
        self.assertIn(b"om_cat-tape_48x297mm", body)
        self.assertIn(b"om_cat-tape_48x500mm", body)
        self.assertIn(b"media-default", body)
        self.assertIn(b"om_cat-tape_48x297mm", body)
        self.assertIn(b"iso_a4_210x297mm", body)
        self.assertIn(b"na_letter_8.5x11in", body)
        self.assertIn(b"stationery", body)
        self.assertIn(b"labels", body)
        self.assertIn(b"roll", body)
        self.assertIn(b"catprinter 0.1.0", body)

    def test_build_response_is_parseable(self):
        req = parse_ipp(_minimal_request())
        resp = build_response(req, STATUS_OK, [])
        parsed = parse_ipp(resp)
        self.assertEqual(parsed.request_id, 1)
        self.assertEqual(parsed.operation, STATUS_OK)
