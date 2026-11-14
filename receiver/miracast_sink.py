#!/usr/bin/env python3
"""Miracast-over-Infrastructure (MS-MICE) sink.

Flow for one session:
  1. Windows connects to TCP 7250 and sends SOURCE_READY (friendly name, RTSP port).
  2. We connect back to the source's RTSP port and negotiate Wi-Fi Display (M1..M7).
  3. The source sends RTP/MPEG-TS (H.264) to our UDP port.
  4. ffmpeg decodes the stream to JPEG frames, which go to the outputs
     (web panel, later the projector).

    python miracast_sink.py                      run with defaults
    python miracast_sink.py --dump-ts out.ts     also write the raw MPEG-TS stream to a file
    python miracast_sink.py --no-decode          protocol only, no ffmpeg needed

Needs only the Python standard library plus an `ffmpeg` binary on PATH (unless --no-decode).
"""
import argparse
import datetime
import json
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time

import ezdisplay
import panel
from mice_listen import decode_message

MICE_SOURCE_READY = 0x01
MICE_STOP_PROJECTION = 0x02
TLV_FRIENDLY_NAME = 0x00
TLV_RTSP_PORT = 0x02
TLV_SOURCE_ID = 0x03


def log(text):
    print("%s  %s" % (datetime.datetime.now().strftime("%H:%M:%S"), text), flush=True)


def parse_mice(msg):
    """Return (command, {tlv_type: value}) for one complete MICE message."""
    _size, _version, command = struct.unpack(">HBB", msg[:4])
    tlvs = {}
    pos = 4
    while pos + 3 <= len(msg):
        tlv_type, length = struct.unpack(">BH", msg[pos:pos + 3])
        tlvs[tlv_type] = msg[pos + 3:pos + 3 + length]
        pos += 3 + length
    return command, tlvs


# ---- frame distribution ------------------------------------------------------------------------

class FrameHub:
    """Holds the newest JPEG frame and wakes up everybody who waits for the next one."""

    def __init__(self):
        self._cond = threading.Condition()
        self._frame = None
        self._serial = 0
        self.outputs = []

    def publish(self, jpeg):
        with self._cond:
            self._frame = jpeg
            self._serial += 1
            self._cond.notify_all()
        for out in self.outputs:
            try:
                out.send_frame(jpeg)
            except Exception as exc:  # an output must never kill the session
                log("output %s failed: %s" % (type(out).__name__, exc))

    def clear(self):
        with self._cond:
            self._frame = None
            self._serial += 1
            self._cond.notify_all()
        for out in self.outputs:
            try:
                out.session_ended()
            except Exception as exc:
                log("output %s failed: %s" % (type(out).__name__, exc))

    def wait(self, last_serial, timeout=5.0):
        with self._cond:
            if self._serial == last_serial:
                self._cond.wait(timeout)
            return self._serial, self._frame


# ---- decoder: MPEG-TS in, JPEG frames out ------------------------------------------------------

class Decoder:
    def __init__(self, hub, fps, width, quality, height=0):
        vf = "fps=%d" % fps
        if width and height:      # exact size, letterboxed
            vf += (",scale=%d:%d:force_original_aspect_ratio=decrease,pad=%d:%d:(ow-iw)/2:(oh-ih)/2"
                   % (width, height, width, height))
        elif width:
            vf += ",scale=%d:-2" % width
        vf += ",format=yuvj420p"  # plain baseline 4:2:0 JPEG
        cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error",
               "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "200000",
               "-analyzeduration", "500000", "-f", "mpegts", "-i", "pipe:0",
               "-an", "-vf", vf, "-q:v", str(quality), "-f", "mjpeg", "pipe:1"]
        self.hub = hub
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.frames = 0
        threading.Thread(target=self._read, daemon=True).start()

    def feed(self, data):
        try:
            self.proc.stdin.write(data)
        except (OSError, ValueError):
            pass

    def _read(self):
        buf = b""
        while True:
            chunk = self.proc.stdout.read1(65536)
            if not chunk:
                break
            buf += chunk
            while True:
                start = buf.find(b"\xff\xd8")
                if start < 0:
                    buf = b""
                    break
                end = buf.find(b"\xff\xd9", start + 2)
                if end < 0:
                    buf = buf[start:]
                    break
                self.frames += 1
                self.hub.publish(buf[start:end + 2])
                buf = buf[end + 2:]

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(3)
        except subprocess.TimeoutExpired:
            self.proc.kill()


# ---- RTP receiver ------------------------------------------------------------------------------

class RtpReceiver(threading.Thread):
    """Receives RTP (payload type 33, MPEG-TS) and hands the TS bytes on in sequence order."""

    def __init__(self, bind, port, sinks):
        super().__init__(daemon=True)
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4 * 1024 * 1024)
        self.sock.bind((bind, port))
        self.sock.settimeout(1.0)
        self.sinks = sinks
        self.packets = 0
        self.lost = 0
        self.bytes = 0
        self._halt = False
        self._expected = None
        self._held = {}

    def stop(self):
        self._halt = True

    def _deliver(self, payload):
        self.bytes += len(payload)
        for sink in self.sinks:
            sink(payload)

    def _accept(self, seq, payload):
        if self._expected is None:
            self._expected = seq
        ahead = (seq - self._expected) & 0xFFFF
        if ahead >= 0x8000:          # older than what we already passed on
            return
        self._held[seq] = payload
        # hold back at most 16 packets while waiting for a missing one
        while self._held:
            if self._expected in self._held:
                self._deliver(self._held.pop(self._expected))
                self._expected = (self._expected + 1) & 0xFFFF
            elif len(self._held) > 16:
                self.lost += 1
                self._expected = (self._expected + 1) & 0xFFFF
            else:
                break

    def run(self):
        while not self._halt:
            try:
                data, _peer = self.sock.recvfrom(65536)
            except socket.timeout:
                continue
            except OSError:
                break
            if len(data) < 12 or data[0] >> 6 != 2:
                continue
            offset = 12 + 4 * (data[0] & 0x0F)
            if data[0] & 0x10:       # header extension
                if len(data) < offset + 4:
                    continue
                offset += 4 + 4 * struct.unpack(">H", data[offset + 2:offset + 4])[0]
            end = len(data)
            if data[0] & 0x20 and end > offset:   # padding
                end -= data[-1]
            if end <= offset:
                continue
            self.packets += 1
            self._accept(struct.unpack(">H", data[2:4])[0], data[offset:end])
        self.sock.close()


# ---- RTSP / Wi-Fi Display ----------------------------------------------------------------------

class RtspMessage:
    def __init__(self, first, headers, body):
        self.first = first
        self.headers = headers
        self.body = body
        self.is_response = first.startswith("RTSP/")

    @property
    def method(self):
        return self.first.split(" ", 1)[0]

    @property
    def status(self):
        return int(self.first.split(" ")[1]) if self.is_response else 0

    def params(self):
        """Body lines 'name: value' as a dict; lines without ':' map to ''."""
        result = {}
        for line in self.body.decode("utf-8", "replace").splitlines():
            line = line.strip()
            if not line:
                continue
            name, _, value = line.partition(":")
            result[name.strip()] = value.strip()
        return result


class RtspConnection:
    def __init__(self, sock):
        self.sock = sock
        self.buf = b""

    def read(self):
        """Return the next RtspMessage, or None when the peer closed the connection."""
        # self.buf is only consumed once a message is complete, so a socket timeout in the
        # middle of a message loses nothing.
        while True:
            head, sep, rest = self.buf.partition(b"\r\n\r\n")
            if sep:
                lines = head.decode("utf-8", "replace").split("\r\n")
                headers = {}
                for line in lines[1:]:
                    name, _, value = line.partition(":")
                    headers[name.strip().lower()] = value.strip()
                length = int(headers.get("content-length", "0") or 0)
                if len(rest) >= length:
                    self.buf = rest[length:]
                    return RtspMessage(lines[0], headers, rest[:length])
            data = self.sock.recv(8192)
            if not data:
                return None
            self.buf += data

    def send(self, first, headers, body=b""):
        lines = [first] + ["%s: %s" % item for item in headers]
        if body:
            lines.append("Content-Type: text/parameters")
            lines.append("Content-Length: %d" % len(body))
        self.sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode() + body)


# Answers to the source's capability query (M3). Only the parameters the source asks for are sent.
def wfd_capabilities(rtp_port):
    return {
        # native, preferred-mode, then one H.264 codec entry: profile CHP|CBP, level 4.2,
        # all CEA / VESA / handheld resolutions, no latency / slice / frame-skip extras.
        "wfd_video_formats": "00 00 03 10 0001ffff 1fffffff 00001fff 00 0000 0000 10 none none",
        "wfd_audio_codecs": "LPCM 00000002 00",          # 48 kHz stereo, mandatory format
        "wfd_client_rtp_ports": "RTP/AVP/UDP;unicast %d 0 mode=play" % rtp_port,
        "wfd_content_protection": "none",
        "wfd_display_edid": "none",
        "wfd_coupled_sink": "none",
        "wfd_uibc_capability": "none",
        "wfd_standby_resume_capability": "none",
        "wfd_connector_type": "05",                      # HDMI
        "wfd_idr_request_capability": "1",
    }


class Session:
    def __init__(self, args, hub, source_ip, rtsp_port, name):
        self.args = args
        self.hub = hub
        self.source_ip = source_ip
        self.rtsp_port = rtsp_port
        self.name = name
        self.stop_requested = threading.Event()
        self.playing = False
        self.started = time.time()
        self.decoder = None
        self._cseq = 0
        self._pending = {}

    def request(self, conn, method, url, headers=(), body=b""):
        self._cseq += 1
        self._pending[self._cseq] = method
        conn.send("%s %s RTSP/1.0" % (method, url), [("CSeq", self._cseq)] + list(headers), body)

    def run(self):
        sinks = []
        decoder = None
        dump = None
        if not self.args.no_decode:
            decoder = self.decoder = Decoder(self.hub, self.args.fps, self.args.width, self.args.quality,
                                             getattr(self.args, "height", 0))
            sinks.append(decoder.feed)
        if self.args.dump_ts:
            dump = open(self.args.dump_ts, "wb")
            sinks.append(dump.write)
        rtp = RtpReceiver(self.args.bind, self.args.rtp_port, sinks)
        rtp.start()
        sock = None
        try:
            sock = socket.create_connection((self.source_ip, self.rtsp_port), timeout=10)
            sock.settimeout(1.0)
            log("RTSP connected to %s:%d" % (self.source_ip, self.rtsp_port))
            self._rtsp_loop(RtspConnection(sock))
        except OSError as exc:
            log("RTSP error: %s" % exc)
        finally:
            if sock:
                sock.close()
            rtp.stop()
            rtp.join(3)
            if decoder:
                decoder.close()
            if dump:
                dump.close()
            self.hub.clear()
            log("session with %r ended: %d RTP packets, %d lost, %d bytes, %d frames"
                % (self.name, rtp.packets, rtp.lost, rtp.bytes, decoder.frames if decoder else 0))

    def _rtsp_loop(self, conn):
        url = "rtsp://%s/wfd1.0/streamid=0" % self.source_ip
        session_id = None
        last_seen = time.monotonic()
        while True:
            if self.stop_requested.is_set():
                if session_id:
                    self.request(conn, "TEARDOWN", url, [("Session", session_id)])
                return
            try:
                msg = conn.read()
            except socket.timeout:
                if time.monotonic() - last_seen > self.args.idle_timeout:
                    log("RTSP: nothing from the source for %d s, giving up" % self.args.idle_timeout)
                    return
                continue
            if msg is None:
                log("RTSP: source closed the connection")
                return
            last_seen = time.monotonic()
            cseq = msg.headers.get("cseq", "0")

            if msg.is_response:
                method = self._pending.pop(int(cseq), "?")
                if msg.status != 200:
                    log("RTSP: %s was answered with %s" % (method, msg.first))
                    return
                if method == "SETUP":
                    session_id = msg.headers.get("session", "").split(";")[0]
                    self.request(conn, "PLAY", url, [("Session", session_id)])
                elif method == "PLAY":
                    self.playing = True
                    log("RTSP: PLAY accepted, stream is running")
                elif method == "TEARDOWN":
                    return
                continue

            if msg.method == "OPTIONS":                                  # M1, then our M2
                conn.send("RTSP/1.0 200 OK", [("CSeq", cseq),
                          ("Public", "org.wfa.wfd1.0, GET_PARAMETER, SET_PARAMETER")])
                self.request(conn, "OPTIONS", "*", [("Require", "org.wfa.wfd1.0")])
            elif msg.method == "GET_PARAMETER":
                wanted = msg.params()
                if not wanted:                                           # M16 keep-alive
                    conn.send("RTSP/1.0 200 OK", [("CSeq", cseq)])
                    continue
                caps = wfd_capabilities(self.args.rtp_port)              # M3
                body = "".join("%s: %s\r\n" % (name, caps.get(name, "none")) for name in wanted)
                conn.send("RTSP/1.0 200 OK", [("CSeq", cseq)], body.encode())
            elif msg.method == "SET_PARAMETER":
                params = msg.params()
                conn.send("RTSP/1.0 200 OK", [("CSeq", cseq)])
                if "wfd_presentation_URL" in params:                     # M4
                    first = params["wfd_presentation_URL"].split(" ")[0]
                    if first.startswith("rtsp://"):
                        url = first
                    log("RTSP: source chose video %r" % params.get("wfd_video_formats", "?"))
                trigger = params.get("wfd_trigger_method", "")
                if trigger == "SETUP":                                   # M5 -> M6
                    self.request(conn, "SETUP", url, [("Transport",
                                 "RTP/AVP/UDP;unicast;client_port=%d" % self.args.rtp_port)])
                elif trigger == "TEARDOWN" and session_id:
                    self.request(conn, "TEARDOWN", url, [("Session", session_id)])
                elif trigger in ("PAUSE", "PLAY") and session_id:
                    self.request(conn, trigger, url, [("Session", session_id)])
            else:
                conn.send("RTSP/1.0 405 Method Not Allowed", [("CSeq", cseq)])


# ---- MICE control channel (TCP 7250) ------------------------------------------------------------

class Sink:
    def __init__(self, args, hub):
        self.args = args
        self.hub = hub
        self.lock = threading.Lock()
        self.session = None

    def handle(self, conn, peer):
        buf = b""
        conn.settimeout(self.args.idle_timeout)
        try:
            while True:
                data = conn.recv(4096)
                if not data:
                    return
                buf += data
                while len(buf) >= 4:
                    size = struct.unpack(">H", buf[:2])[0]
                    if size < 4:
                        log("MICE: bad size field from %s: %s" % (peer[0], buf.hex()))
                        return
                    if len(buf) < size:
                        break
                    msg, buf = buf[:size], buf[size:]
                    for line in decode_message(msg):
                        log("MICE %s: %s" % (peer[0], line))
                    self.on_message(peer[0], *parse_mice(msg))
        except OSError:
            return
        finally:
            conn.close()

    def on_message(self, source_ip, command, tlvs):
        if command == MICE_SOURCE_READY:
            name = tlvs.get(TLV_FRIENDLY_NAME, b"").decode("utf-16-le", "replace")
            port = 7236
            if len(tlvs.get(TLV_RTSP_PORT, b"")) == 2:
                port = struct.unpack(">H", tlvs[TLV_RTSP_PORT])[0]
            with self.lock:
                if self.session is not None:
                    log("MICE: busy with %r, ignoring %r" % (self.session.name, name))
                    return
                session = Session(self.args, self.hub, source_ip, port, name)
                self.session = session
            threading.Thread(target=self._run, args=(session,), daemon=True).start()
        elif command == MICE_STOP_PROJECTION:
            with self.lock:
                session = self.session
            if session and session.source_ip == source_ip:
                session.stop_requested.set()

    def _run(self, session):
        log("session start: %r from %s" % (session.name, session.source_ip))
        try:
            session.run()
        finally:
            with self.lock:
                self.session = None


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bind", default="0.0.0.0", help="address to listen on")
    parser.add_argument("--port", type=int, default=7250, help="MICE control port (TCP)")
    parser.add_argument("--rtp-port", type=int, default=50000, help="UDP port for the video stream")
    parser.add_argument("--http-port", type=int, default=8445, help="web panel, 0 = off")
    parser.add_argument("--config", default="/etc/acermiracast/config.json", help="panel settings")
    parser.add_argument("--fps", type=int, default=15, help="JPEG frames per second")
    parser.add_argument("--width", type=int, default=0, help="scale to this width, 0 = keep")
    parser.add_argument("--quality", type=int, default=5, help="ffmpeg JPEG quality 2 (best) .. 31")
    parser.add_argument("--idle-timeout", type=int, default=70, help="seconds without RTSP traffic")
    parser.add_argument("--dump-ts", help="write the received MPEG-TS stream to this file")
    parser.add_argument("--no-decode", action="store_true", help="do not start ffmpeg")
    args = parser.parse_args()

    if not args.no_decode and not shutil.which("ffmpeg"):
        sys.exit("ffmpeg was not found on PATH (install it, or use --no-decode)")

    config = {}
    try:
        with open(args.config, encoding="utf-8") as handle:
            config = json.load(handle)
    except FileNotFoundError:
        log("no config file %s, running without projector and beacon" % args.config)

    hub = FrameHub()
    if config.get("picture_output") == "ezdisplay" and config.get("projector_host"):
        # the projector wants pictures in exactly its own size
        args.width = int(config.get("picture_width", 1920))
        args.height = int(config.get("picture_height", 1080))
        host = config["projector_host"]
        prepare = None
        if config.get("projector_control", "acer_web") == "acer_web":
            prepare = lambda: panel.acer_post(host, "/form/control_cgi", "src=33")   # LAN/WiFi input
        hub.outputs.append(ezdisplay.EZDisplayOutput(
            host, args.width, args.height, config.get("room_name", "Plintus"), prepare))
        log("pictures go to the projector %s as %dx%d" % (host, args.width, args.height))

    sink = Sink(args, hub)
    if args.http_port:
        panel.start(args.bind, args.http_port, hub, sink, config)
        log("web panel on http://%s:%d/" % (args.bind, args.http_port))
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind((args.bind, args.port))
    server.listen(8)
    log("MICE sink listening on %s:%d, RTP on UDP %d" % (args.bind, args.port, args.rtp_port))
    try:
        while True:
            conn, peer = server.accept()
            threading.Thread(target=sink.handle, args=(conn, peer), daemon=True).start()
    except KeyboardInterrupt:
        log("stopped")
    finally:
        server.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
