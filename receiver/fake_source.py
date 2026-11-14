#!/usr/bin/env python3
"""Test stand-in for a Windows Miracast source. Exercises miracast_sink.py end to end.

It does what Windows does: opens an RTSP server, sends SOURCE_READY to the sink's port 7250,
plays the Wi-Fi Display negotiation (M1..M7) as the source, then sends RTP/MPEG-TS packets
(with one swapped pair and one duplicate) and finally STOP_PROJECTION.

    python fake_source.py --sink 127.0.0.1 [--ts file.ts] [--packets 200]

Without --ts it sends numbered dummy TS packets; with --ts it streams a real MPEG-TS file.
Exit code 0 means every protocol step happened in the expected order.
"""
import argparse
import socket
import struct
import sys
import time

from miracast_sink import RtspConnection


def mice(command, tlvs):
    body = b"".join(struct.pack(">BH", t, len(v)) + v for t, v in tlvs)
    return struct.pack(">HBB", 4 + len(body), 1, command) + body


def dummy_ts(index):
    return b"\x47\x1f\xff\x10" + struct.pack(">I", index) + b"\xff" * 180


def expect(cond, what):
    print(("ok   " if cond else "FAIL ") + what, flush=True)
    if not cond:
        sys.exit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--sink", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=7250)
    parser.add_argument("--rtsp-port", type=int, default=7236)
    parser.add_argument("--ts", help="MPEG-TS file to stream instead of dummy packets")
    parser.add_argument("--packets", type=int, default=200, help="RTP packets when using dummy data")
    args = parser.parse_args()

    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", args.rtsp_port))
    listener.listen(1)
    listener.settimeout(10)

    ctrl = socket.create_connection((args.sink, args.port), timeout=5)
    ctrl.sendall(mice(0x01, [(0x00, "Fake-Source".encode("utf-16-le")),
                             (0x02, struct.pack(">H", args.rtsp_port)),
                             (0x03, bytes(range(16)))]))
    sock, peer = listener.accept()
    sock.settimeout(10)
    expect(True, "sink connected back to RTSP port from %s" % peer[0])
    conn = RtspConnection(sock)

    conn.send("OPTIONS * RTSP/1.0", [("CSeq", 1), ("Require", "org.wfa.wfd1.0")])          # M1
    msg = conn.read()
    expect(msg.is_response and msg.status == 200 and "GET_PARAMETER" in msg.headers.get("public", ""),
           "M1 answered with Public header")
    msg = conn.read()                                                                      # M2
    expect(msg.method == "OPTIONS", "M2 OPTIONS from sink")
    conn.send("RTSP/1.0 200 OK", [("CSeq", msg.headers["cseq"]),
              ("Public", "org.wfa.wfd1.0, SETUP, TEARDOWN, PLAY, PAUSE, GET_PARAMETER, SET_PARAMETER")])

    wanted = ["wfd_video_formats", "wfd_audio_codecs", "wfd_client_rtp_ports",
              "wfd_content_protection", "wfd_display_edid", "microsoft_cursor"]
    conn.send("GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 2)],
              ("\r\n".join(wanted) + "\r\n").encode())                                     # M3
    msg = conn.read()
    caps = msg.params()
    expect(msg.status == 200 and all(name in caps for name in wanted), "M3 answered for every parameter")
    expect(caps["microsoft_cursor"] == "none", "unknown parameter answered with none")
    rtp_port = int(caps["wfd_client_rtp_ports"].split(" ")[1])
    expect(rtp_port > 0, "sink RTP port is %d" % rtp_port)

    url = "rtsp://%s/wfd1.0/streamid=0" % sock.getsockname()[0]
    body = ("wfd_video_formats: 00 00 02 10 00000080 00000000 00000000 00 0000 0000 00 none none\r\n"
            "wfd_audio_codecs: LPCM 00000002 00\r\n"
            "wfd_presentation_URL: %s none\r\n"
            "wfd_client_rtp_ports: RTP/AVP/UDP;unicast %d 0 mode=play\r\n" % (url, rtp_port))
    conn.send("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 3)], body.encode())   # M4
    expect(conn.read().status == 200, "M4 accepted")
    conn.send("SET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 4)],
              b"wfd_trigger_method: SETUP\r\n")                                            # M5
    expect(conn.read().status == 200, "M5 accepted")

    msg = conn.read()                                                                      # M6
    expect(msg.method == "SETUP" and msg.first.split(" ")[1] == url
           and "client_port=%d" % rtp_port in msg.headers.get("transport", ""), "M6 SETUP with our URL")
    conn.send("RTSP/1.0 200 OK", [("CSeq", msg.headers["cseq"]), ("Session", "12345678;timeout=30"),
              ("Transport", "RTP/AVP/UDP;unicast;client_port=%d;server_port=19000" % rtp_port)])
    msg = conn.read()                                                                      # M7
    expect(msg.method == "PLAY" and msg.headers.get("session") == "12345678", "M7 PLAY with session id")
    conn.send("RTSP/1.0 200 OK", [("CSeq", msg.headers["cseq"]), ("Session", "12345678")])

    # ---- stream ----
    if args.ts:
        with open(args.ts, "rb") as handle:
            data = handle.read()
        payloads = [data[i:i + 7 * 188] for i in range(0, len(data), 7 * 188)]
        pace = 0.002
    else:
        payloads = [b"".join(dummy_ts(n * 7 + k) for k in range(7)) for n in range(args.packets)]
        pace = 0.0005
    order = list(range(len(payloads)))
    if len(order) > 20:
        order[10], order[11] = order[11], order[10]      # out of order
        order.insert(15, order[14])                      # duplicate
    udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    for n in order:
        header = struct.pack(">BBHII", 0x80, 33, (65530 + n) & 0xFFFF, n * 3000, 0x1234)   # seq wraps
        udp.sendto(header + payloads[n], (args.sink, rtp_port))
        time.sleep(pace)
    expect(True, "sent %d RTP packets (%d bytes of MPEG-TS)" % (len(order), sum(map(len, payloads))))

    conn.send("GET_PARAMETER rtsp://localhost/wfd1.0 RTSP/1.0", [("CSeq", 5)])             # M16
    expect(conn.read().status == 200, "keep-alive answered")

    time.sleep(0.5)
    ctrl.sendall(mice(0x02, [(0x03, bytes(range(16)))]))                                   # stop
    msg = conn.read()
    expect(msg is not None and msg.method == "TEARDOWN", "TEARDOWN after STOP_PROJECTION")
    conn.send("RTSP/1.0 200 OK", [("CSeq", msg.headers["cseq"])])
    time.sleep(0.5)
    print("fake source: all steps OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
