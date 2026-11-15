#!/usr/bin/env python3
"""Test stand-in for a Windows Miracast source, for the Rust receiver.

Does what Windows does: RTSP server, SOURCE_READY to the sink's port 7250, Wi-Fi Display
negotiation (M1..M7) as the source, then RTP/MPEG-TS with a synthetic H.264 stream (CBP
1280x720p30 as negotiated, key frame every second or when the sink asks for one), finally
STOP_PROJECTION. Needs nothing but Python.

    py -I fake_source.py [--sink 127.0.0.1] [--seconds 6] [--format passthrough|other]
"""
import argparse
import os
import socket
import struct
import sys
import threading
import time


def mice(command, tlvs):
    body = b"".join(struct.pack(">BH", t, len(v)) + v for t, v in tlvs)
    return struct.pack(">HBB", 4 + len(body), 1, command) + body


def expect(cond, what):
    print(("ok   " if cond else "FAIL ") + what, flush=True)
    if not cond:
        sys.exit(1)


class Rtsp:
    def __init__(self, sock):
        self.sock, self.buf = sock, b""
        self.lock = threading.Lock()

    def read(self):
        while True:
            head, sep, rest = self.buf.partition(b"\r\n\r\n")
            if sep:
                lines = head.decode().split("\r\n")
                headers = {k.strip().lower(): v.strip() for k, _, v in (l.partition(":") for l in lines[1:])}
                n = int(headers.get("content-length", "0"))
                if len(rest) >= n:
                    self.buf = rest[n:]
                    return lines[0], headers, rest[:n].decode()
            data = self.sock.recv(8192)
            if not data:
                return None
            self.buf += data

    def send(self, first, headers, body=""):
        text = first + "\r\n" + "".join("%s: %s\r\n" % h for h in headers)
        if body:
            text += "Content-Type: text/parameters\r\nContent-Length: %d\r\n" % len(body)
        with self.lock:
            self.sock.sendall((text + "\r\n" + body).encode())


# ---- synthetic H.264 in MPEG-TS -----------------------------------------------------------------

def no_zero(n):
    return bytes(b or 1 for b in os.urandom(n))


SPS = b"\x67\x42\xc0\x1f" + no_zero(8)
PPS = b"\x68\xce\x3c\x80"


def access_unit(key, with_sps=True):
    au = b"\x00\x00\x00\x01\x09\xf0"
    if key:
        if with_sps:
            au += b"\x00\x00\x00\x01" + SPS + b"\x00\x00\x00\x01" + PPS
        au += b"\x00\x00\x00\x01\x65\x88" + no_zero(6000)
    else:
        au += b"\x00\x00\x00\x01\x41\x9a" + no_zero(1500)
    return au


class TsMux:
    VIDEO, PMT = 0x1011, 0x100

    def __init__(self):
        self.cc = {}

    def _packets(self, pid, payload, pusi):
        out = []
        first = True
        while payload or first:
            room = 184
            chunk = payload[:room]
            cc = self.cc.get(pid, 0)
            self.cc[pid] = (cc + 1) & 15
            head = bytes([0x47, ((0x40 if (pusi and first) else 0) | (pid >> 8)), pid & 0xff])
            if len(chunk) < room:                       # stuff with an adaptation field
                stuff = room - len(chunk) - 1
                adapt = bytes([stuff]) + (bytes([0x00]) + b"\xff" * (stuff - 1) if stuff > 0 else b"")
                out.append(head + bytes([0x30 | cc]) + adapt + chunk)
            else:
                out.append(head + bytes([0x10 | cc]) + chunk)
            payload = payload[len(chunk):]
            first = False
        return out

    def tables(self):
        pat = b"\x00\x00\xb0\x0d\x00\x01\xc1\x00\x00\x00\x01" + struct.pack(">H", 0xe000 | self.PMT) + b"\0\0\0\0"
        pmt = (b"\x00\x02\xb0\x12\x00\x01\xc1\x00\x00" + struct.pack(">H", 0xe000 | self.VIDEO) + b"\xf0\x00"
               + b"\x1b" + struct.pack(">H", 0xe000 | self.VIDEO) + b"\xf0\x00" + b"\0\0\0\0")
        return self._packets(0, pat, True) + self._packets(self.PMT, pmt, True)

    def pes(self, au, pts):
        p = pts & ((1 << 33) - 1)
        stamp = bytes([0x21 | ((p >> 29) & 0x0e), (p >> 22) & 0xff, 0x01 | ((p >> 14) & 0xfe),
                       (p >> 7) & 0xff, 0x01 | ((p << 1) & 0xfe)])
        return self._packets(self.VIDEO, b"\x00\x00\x01\xe0\x00\x00\x80\x80\x05" + stamp + au, True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sink", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=7250)
    ap.add_argument("--rtsp-port", type=int, default=7236)
    ap.add_argument("--seconds", type=float, default=6)
    ap.add_argument("--format", default="passthrough")
    args = ap.parse_args()

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", args.rtsp_port))
    listener.listen(1)
    listener.settimeout(10)
    ctrl = socket.create_connection((args.sink, args.port), timeout=5)
    ctrl.sendall(mice(0x01, [(0x00, "Fake-Source".encode("utf-16-le")), (0x02, struct.pack(">H", args.rtsp_port)),
                             (0x03, bytes(range(16)))]))
    sock, peer = listener.accept()
    sock.settimeout(10)
    expect(True, "sink connected back to RTSP from %s" % peer[0])
    c = Rtsp(sock)

    c.send("OPTIONS * RTSP/1.0", [("CSeq", 1), ("Require", "org.wfa.wfd1.0")])
    first, h, _ = c.read()
    expect(first.startswith("RTSP/1.0 200") and "GET_PARAMETER" in h.get("public", ""), "M1 answered")
    first, h, _ = c.read()
    expect(first.startswith("OPTIONS"), "M2 OPTIONS from sink")
    c.send("RTSP/1.0 200 OK", [("CSeq", h["cseq"]), ("Public", "org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER")])
    wanted = ["wfd_video_formats", "wfd_audio_codecs", "wfd_client_rtp_ports", "wfd_content_protection", "microsoft_cursor"]
    c.send("GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 2)], "\r\n".join(wanted) + "\r\n")
    first, h, body = c.read()
    caps = dict(l.split(": ", 1) for l in body.strip().split("\r\n"))
    expect(all(w in caps for w in wanted), "M3 answered for every parameter")
    print("     sink offers video: " + caps["wfd_video_formats"])
    rtp_port = int(caps["wfd_client_rtp_ports"].split(" ")[1])
    url = "rtsp://%s/wfd1.0/streamid=0" % sock.getsockname()[0]
    fmt = ("00 00 01 01 00000020 00000000 00000000 00 0000 0000 00 none none" if args.format == "passthrough"
           else "00 00 02 10 00000080 00000000 00000000 00 0000 0000 00 none none")
    c.send("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 3)],
           "wfd_video_formats: %s\r\nwfd_audio_codecs: LPCM 00000002 00\r\nwfd_presentation_URL: %s none\r\n"
           "wfd_client_rtp_ports: RTP/AVP/UDP;unicast %d 0 mode=play\r\n" % (fmt, url, rtp_port))
    expect(c.read()[0].startswith("RTSP/1.0 200"), "M4 accepted")
    c.send("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 4)], "wfd_trigger_method: SETUP\r\n")
    expect(c.read()[0].startswith("RTSP/1.0 200"), "M5 accepted")
    first, h, _ = c.read()
    expect(first.startswith("SETUP " + url) and "client_port=%d" % rtp_port in h.get("transport", ""), "M6 SETUP")
    c.send("RTSP/1.0 200 OK", [("CSeq", h["cseq"]), ("Session", "12345678;timeout=30"),
                               ("Transport", "RTP/AVP/UDP;unicast;client_port=%d;server_port=19000" % rtp_port)])
    first, h, _ = c.read()
    expect(first.startswith("PLAY") and h.get("session") == "12345678", "M7 PLAY")
    c.send("RTSP/1.0 200 OK", [("CSeq", h["cseq"]), ("Session", "12345678")])

    # From here on the sink may send requests (IDR) at any time: a reader thread answers them.
    state = {"idr": 0, "idr_pending": False, "teardown": False, "keepalive": False}

    def reader():
        while True:
            msg = c.read()
            if msg is None:
                return
            first, h, body = msg
            if first.startswith("SET_PARAMETER") and "wfd_idr_request" in body:
                state["idr"] += 1
                state["idr_pending"] = True
                c.send("RTSP/1.0 200 OK", [("CSeq", h["cseq"])])
            elif first.startswith("TEARDOWN"):
                state["teardown"] = True
                c.send("RTSP/1.0 200 OK", [("CSeq", h["cseq"])])
            elif first.startswith("RTSP/1.0 200"):
                state["keepalive"] = True

    threading.Thread(target=reader, daemon=True).start()

    mux = TsMux()
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    seq, frames, sent = 65530, int(args.seconds * 30), 0
    pending = []
    start = time.monotonic()
    for n in range(frames):
        key = n % 30 == 0 or state["idr_pending"]
        state["idr_pending"] = False
        pkts = (mux.tables() if key else []) + mux.pes(access_unit(key, with_sps=(n != 30)), n * 3000)
        pending += pkts
        while len(pending) >= 7 or (pending and n == frames - 1):
            chunk, pending = pending[:7], pending[7:]
            udp.sendto(struct.pack(">BBHII", 0x80, 33, seq & 0xffff, n * 3000, 0x1234) + b"".join(chunk),
                       (args.sink, rtp_port))
            seq += 1
            sent += 1
        time.sleep(max(0, start + (n + 1) / 30 - time.monotonic()))
    expect(True, "sent %d frames in %d RTP packets" % (frames, sent))
    expect(state["idr"] > 0, "sink asked for key frames %d times" % state["idr"])
    c.send("GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 5)])
    time.sleep(0.5)
    expect(state["keepalive"], "keep-alive answered")
    ctrl.sendall(mice(0x02, [(0x03, bytes(range(16)))]))
    time.sleep(1.0)
    expect(state["teardown"], "TEARDOWN after STOP_PROJECTION")
    print("fake source: all steps OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
