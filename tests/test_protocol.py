import unittest

from catprinter import protocol as proto


class Crc8Tests(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(proto.crc8(b""), 0x00)

    def test_single_zero(self):
        self.assertEqual(proto.crc8(b"\x00"), 0x00)

    def test_known_payload(self):
        # CRC-8/MAXIM-DOW, poly 0x07, init 0: table[0x5D] == 0x94
        self.assertEqual(proto.crc8(bytes([0x5D])), 0x94)


class FrameTests(unittest.TestCase):
    def test_make_command_status(self):
        pkt = proto.cmd_get_status()
        self.assertEqual(pkt[0], 0x22)
        self.assertEqual(pkt[1], 0x21)
        self.assertEqual(pkt[2], proto.CommandIDs.GET_STATUS)
        self.assertEqual(pkt[3], 0x00)
        self.assertEqual(int.from_bytes(pkt[4:6], "little"), 1)
        self.assertEqual(pkt[6], 0x00)
        self.assertEqual(pkt[7], proto.crc8(b"\x00"))
        self.assertEqual(pkt[8], 0xFF)

    def test_print_request_line_count(self):
        pkt = proto.cmd_print_request(384, proto.PrintModes.MONOCHROME)
        payload = pkt[6:-2]
        self.assertEqual(int.from_bytes(payload[0:2], "little"), 384)
        self.assertEqual(payload[2], 0x30)
        self.assertEqual(payload[3], 0x00)
        self.assertEqual(pkt[-2], proto.crc8(payload))

    def test_print_request_grayscale_mode(self):
        pkt = proto.cmd_print_request(100, proto.PrintModes.GRAYSCALE)
        payload = pkt[6:-2]
        self.assertEqual(int.from_bytes(payload[0:2], "little"), 100)
        self.assertEqual(payload[3], 0x02)

    def test_set_intensity_clamped(self):
        high = proto.cmd_set_intensity(999)
        self.assertEqual(high[6], 0xFF)
        low = proto.cmd_set_intensity(-3)
        self.assertEqual(low[6], 0x00)

    def test_parse_notification_roundtrip(self):
        pkt = proto.cmd_get_status()
        parsed = proto.parse_notification(pkt)
        self.assertIsNotNone(parsed)
        cmd, payload = parsed
        self.assertEqual(cmd, proto.CommandIDs.GET_STATUS)
        self.assertEqual(payload, b"\x00")

    def test_parse_rejects_garbage(self):
        self.assertIsNone(proto.parse_notification(b"hello"))
        self.assertIsNone(proto.parse_notification(b"\x51\x78\xa1\x00\x01\x00\x00\x00\xff"))


class StatusTests(unittest.TestCase):
    def _payload(self, state=0, battery=80, temp=25, ok=0, error=0, length=8):
        data = bytearray(length)
        data[0] = state
        if length > 3:
            data[3] = battery
        if length > 4:
            data[4] = temp
        if length > 6:
            data[6] = ok
        if length > 7:
            data[7] = error
        return bytes(data)

    def test_ok_standby(self):
        status = proto.parse_status(self._payload())
        self.assertTrue(status.ok)
        self.assertEqual(status.state, "standby")
        self.assertEqual(status.battery, 80)
        self.assertIsNone(status.error)
        self.assertIn("ready", status.kid_message().lower())

    def test_no_paper(self):
        status = proto.parse_status(self._payload(ok=1, error=0x01))
        self.assertFalse(status.ok)
        self.assertEqual(status.error, "no paper")
        self.assertIn("paper", status.kid_message().lower())

    def test_overheat(self):
        status = proto.parse_status(self._payload(ok=1, error=0x04))
        self.assertEqual(status.error, "overheated")

    def test_low_battery(self):
        status = proto.parse_status(self._payload(ok=1, error=0x08))
        self.assertEqual(status.error, "low battery")

    def test_short_payload(self):
        status = proto.parse_status(b"\x00\x00")
        self.assertFalse(status.ok)


class ImagePackingTests(unittest.TestCase):
    def test_row_all_white(self):
        row = [False] * proto.PRINTER_WIDTH_PIXELS
        packed = proto.encode_1bpp_row(row)
        self.assertEqual(len(packed), proto.PRINTER_WIDTH_BYTES)
        self.assertEqual(packed, b"\x00" * proto.PRINTER_WIDTH_BYTES)

    def test_row_all_black(self):
        row = [True] * proto.PRINTER_WIDTH_PIXELS
        packed = proto.encode_1bpp_row(row)
        self.assertEqual(packed, b"\xff" * proto.PRINTER_WIDTH_BYTES)

    def test_lsb_is_leftmost_pixel(self):
        row = [False] * proto.PRINTER_WIDTH_PIXELS
        row[0] = True  # leftmost pixel of the page
        packed = proto.encode_1bpp_row(row)
        self.assertEqual(packed[0], 0x01)
        self.assertEqual(sum(packed[1:]), 0)

    def test_wrong_width(self):
        with self.assertRaises(ValueError):
            proto.encode_1bpp_row([True] * 10)

    def test_padding_to_minimum(self):
        # 10 white rows = 480 bytes, must pad to 4320
        rows = [[False] * proto.PRINTER_WIDTH_PIXELS for _ in range(10)]
        buf = proto.prepare_image_data(rows)
        self.assertEqual(len(buf), proto.MIN_DATA_BYTES)
        self.assertEqual(buf, b"\x00" * proto.MIN_DATA_BYTES)

    def test_no_pad_when_tall_enough(self):
        rows = [[False] * proto.PRINTER_WIDTH_PIXELS for _ in range(100)]
        buf = proto.prepare_image_data(rows)
        self.assertEqual(len(buf), 100 * proto.PRINTER_WIDTH_BYTES)

    def test_vectorized_matches_bit_loop(self):
        row = [False] * proto.PRINTER_WIDTH_PIXELS
        row[0] = True
        row[7] = True
        row[8] = True
        packed = proto.encode_1bpp_row(row)
        self.assertEqual(packed[0], 0x81)  # bits 0 and 7
        self.assertEqual(packed[1], 0x01)

    def test_chunk_size_aligns_to_rows(self):
        self.assertEqual(proto.data_chunk_size(20, slow=False), 48)
        self.assertEqual(proto.data_chunk_size(200, slow=True), 48)
        self.assertEqual(proto.data_chunk_size(509), 480)
        self.assertEqual(proto.data_chunk_size(48), 48)

    def test_chunk_size_4bpp_aligns_to_192(self):
        self.assertEqual(
            proto.data_chunk_size(509, row_bytes=proto.PRINTER_WIDTH_NIBBLES),
            384,
        )
        self.assertEqual(
            proto.data_chunk_size(200, slow=True, row_bytes=proto.PRINTER_WIDTH_NIBBLES),
            192,
        )
        self.assertEqual(proto.bytes_per_row(proto.PrintModes.GRAYSCALE), 192)
        self.assertEqual(proto.bytes_per_row(proto.PrintModes.MONOCHROME), 48)


class FourBitPackingTests(unittest.TestCase):
    def test_row_all_white(self):
        row = [0] * proto.PRINTER_WIDTH_PIXELS
        packed = proto.prepare_image_data_4bpp([row], pad=False)
        self.assertEqual(len(packed), proto.PRINTER_WIDTH_NIBBLES)
        self.assertEqual(packed, b"\x00" * proto.PRINTER_WIDTH_NIBBLES)

    def test_row_all_black(self):
        row = [15] * proto.PRINTER_WIDTH_PIXELS
        packed = proto.prepare_image_data_4bpp([row], pad=False)
        self.assertEqual(packed, b"\xff" * proto.PRINTER_WIDTH_NIBBLES)

    def test_even_x_is_high_nibble(self):
        row = [0] * proto.PRINTER_WIDTH_PIXELS
        row[0] = 0xA  # even x → high nibble
        row[1] = 0x3  # odd x → low nibble
        packed = proto.prepare_image_data_4bpp([row], pad=False)
        self.assertEqual(packed[0], 0xA3)
        self.assertEqual(sum(packed[1:]), 0)

    def test_maikelchan_formula(self):
        # bytes[(y*w+x)>>1] |= level << (((x&1)^1)<<2)
        row = [0] * proto.PRINTER_WIDTH_PIXELS
        row[0] = 15
        row[2] = 1
        packed = proto.prepare_image_data_4bpp([row], pad=False)
        expect = bytearray(proto.PRINTER_WIDTH_NIBBLES)
        for x, level in enumerate(row):
            expect[(x) >> 1] |= level << (((x & 1) ^ 1) << 2)
        self.assertEqual(packed, bytes(expect))

    def test_padding_to_minimum(self):
        rows = [[0] * proto.PRINTER_WIDTH_PIXELS for _ in range(10)]
        buf = proto.prepare_image_data_4bpp(rows)
        self.assertEqual(len(buf), proto.MIN_DATA_BYTES_4BPP)

    def test_wrong_width(self):
        with self.assertRaises(ValueError):
            proto.prepare_image_data_4bpp([[1, 2, 3]])


if __name__ == "__main__":
    unittest.main()
