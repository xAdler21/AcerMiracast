#!/usr/bin/env python3
"""Listen on TCP 7250 and decode Miracast-over-Infrastructure (MS-MICE) messages.

This is only the first test stage: it shows that Windows found the beacon and tried to
connect. It does not open the RTSP session, so Windows gives up after a few seconds.

    python mice_listen.py              listen on 0.0.0.0:7250
    python mice_listen.py --selftest   decode the SOURCE_READY example from the spec
"""
import argparse
import datetime
import socket
import struct
import sys
import threading

COMMANDS = {
    0x01: "SOURCE_READY",
    0x02: "STOP_PROJECTION",
    0x03: "SECURITY_HANDSHAKE",
    0x04: "SESSION_REQUEST",
    0x05: "PIN_CHALLENGE",
    0x06: "PIN_RESPONSE",
}
TLVS = {
    0x00: "FRIENDLY_NAME",
    0x02: "RTSP_PORT",
    0x03: "SOURCE_ID",
    0x04: "SECURITY_TOKEN",
    0x05: "SECURITY_OPTIONS",
    0x06: "PIN_CHALLENGE",
    0x07: "PIN_RESPONSE_REASON",
}


def describe_tlv(tlv_type, value):
    if tlv_type == 0x00:
        return repr(value.decode("utf-16-le", errors="replace"))
    if tlv_type == 0x02 and len(value) == 2:
        return str(struct.unpack(">H", value)[0])
    if tlv_type == 0x05 and value:
        bits = value[0]
        return "dtls_encryption=%d sink_displays_pin=%d" % (bits & 1, (bits >> 1) & 1)
    if tlv_type == 0x07 and value:
        return {0: "PIN accepted", 1: "wrong PIN", 2: "invalid message"}.get(value[0], value.hex())
    if len(value) > 32:
        return "%s... (%d bytes)" % (value[:32].hex(), len(value))
    return value.hex()


def decode_message(msg):
    """msg is one complete message including the 4 byte header. Returns text lines."""
    size, version, command = struct.unpack(">HBB", msg[:4])
    lines = ["%s (command 0x%02x, version %d, %d bytes)"
             % (COMMANDS.get(command, "UNKNOWN"), command, version, size)]
    pos = 4
    while pos + 3 <= len(msg):
        tlv_type, length = struct.unpack(">BH", msg[pos:pos + 3])
        value = msg[pos + 3:pos + 3 + length]
        if len(value) < length:
            lines.append("  truncated TLV 0x%02x (want %d bytes, have %d)" % (tlv_type, length, len(value)))
            break
        lines.append("  %-20s %s" % (TLVS.get(tlv_type, "TLV_0x%02x" % tlv_type), describe_tlv(tlv_type, value)))
        pos += 3 + length
    if pos < len(msg) and pos + 3 > len(msg):
        lines.append("  %d trailing bytes: %s" % (len(msg) - pos, msg[pos:].hex()))
    return lines


def log(text):
    print("%s  %s" % (datetime.datetime.now().strftime("%H:%M:%S"), text), flush=True)


def handle(conn, peer):
    log("connection from %s:%d" % peer)
    buf = b""
    try:
        conn.settimeout(60)
        while True:
            data = conn.recv(4096)
            if not data:
                break
            buf += data
            while len(buf) >= 4:
                size = struct.unpack(">H", buf[:2])[0]
                if size < 4:
                    log("  bad size field, raw: %s" % buf.hex())
                    buf = b""
                    break
                if len(buf) < size:
                    break
                for line in decode_message(buf[:size]):
                    log("  " + line)
                buf = buf[size:]
    except socket.timeout:
        log("  timeout")
    except OSError as exc:
        log("  socket error: %s" % exc)
    finally:
        if buf:
            log("  %d undecoded bytes: %s" % (len(buf), buf.hex()))
        conn.close()
        log("closed %s:%d" % peer)


def selftest():
    example = bytes.fromhex(
        "003d0101"
        "00001e440075006d006d00790031002d004b006100620079006c0061006b006500"
        "0200021c44"
        "03001091f4abe9eff5464aaee269722aed11b5"
    )
    lines = decode_message(example)
    print("\n".join(lines))
    ok = (
        lines[0].startswith("SOURCE_READY")
        and "'Dummy1-Kabylake'" in lines[1]
        and lines[2].split()[-1] == "7236"
        and lines[3].split()[-1] == "91f4abe9eff5464aaee269722aed11b5"
        and len(lines) == 4
    )
    print("selftest:", "OK" if ok else "FAILED")
    return 0 if ok else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bind", default="0.0.0.0")
    parser.add_argument("--port", type=int, default=7250)
    parser.add_argument("--selftest", action="store_true")
    args = parser.parse_args()
    if args.selftest:
        return selftest()

    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind((args.bind, args.port))
    server.listen(8)
    log("listening on %s:%d - press Ctrl+C to stop" % (args.bind, args.port))
    try:
        while True:
            conn, peer = server.accept()
            threading.Thread(target=handle, args=(conn, peer), daemon=True).start()
    except KeyboardInterrupt:
        log("stopped")
    finally:
        server.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
