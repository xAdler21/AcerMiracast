"""Picture output to an Acer / EZCast Pro projector ("EZ Display" protocol, port 2425).

Taken from a capture of the Acer software projecting to an Acer P5 (all numbers 32 bit
little endian):

  UDP to port 2425 (from any port)
    "2:<time>:<user>:<host>:1:<user>\\0"          -> answer "...:3:vendor=acer:model=ezcast:..."
    "2:<time>:<user>:<host>:34:\\0" + MIRROR_REQUEST
                                                  -> answer "...:34\\0" {1, 0x1001, width, height, 0, 0}
  TCP to port 2425
    hello            {0, n+24, 9, 0x1000, 1, 0, 0, n} + host name
    request stream   {0, 24,   6, 0x1000, 1, 0, 0, 7}       7 = H.264
    heartbeat        {0, 24,   4, 0, 0, 0, 0, 0}            once a second
    answer           {seq, 24, 6, 1, 1, split, position, 1} 1 = allowed
    video            {seq, size+24} {2, 0x1000, 7, width, height, size} + one H.264 access unit
                     (Annex B, 4 byte start codes, SPS/PPS in front of every key frame)

The Acer software additionally keeps an encrypted control connection (port from the
projector's EZREMOTE broadcast) open; this module does not.
"""
import datetime
import socket
import struct
import threading
import time

PORT = 2425
CMD_VIDEO = 2
CMD_HEARTBEAT = 4
CMD_REQUEST_STREAM = 6
CMD_NOTIFICATION = 7
CMD_HELLO = 9
FORMAT_H264 = 7
USER = "User"
# chunk header {total=1, index=0, size=48, 0} + {1,0,0,0,0,0} twice, as sent by the Acer software
MIRROR_REQUEST = (struct.pack("<4I", 1, 0, 48, 0)
                  + struct.pack("<6I", 1, 0, 0, 0, 0, 0) + struct.pack("<6I", 1, 0, 0, 0, 0, 0))
START_CODE = b"\x00\x00\x00\x01"


def log(text):
    print("%s  projector: %s" % (datetime.datetime.now().strftime("%H:%M:%S"), text), flush=True)


def message(seq, command, a, b, c, d, payload=b"", version=0x1000):
    body = struct.pack("<6I", command, version, a, b, c, d) + payload
    return struct.pack("<2I", seq, len(body)) + body


def hello(name):
    raw = name.encode("ascii", "replace")
    return message(0, CMD_HELLO, 1, 0, 0, len(raw), raw)


def request_stream():
    return message(0, CMD_REQUEST_STREAM, 1, 0, 0, FORMAT_H264)


def heartbeat():
    return message(0, CMD_HEARTBEAT, 0, 0, 0, 0, version=0)


def video(seq, unit, width, height):
    return message(seq, CMD_VIDEO, FORMAT_H264, width, height, len(unit), unit)


def udp_text(command, host):
    return ("2:%u:%s:%s:%d:" % (int(time.time()), USER, host, command)).encode("ascii", "replace")


def parse_size(datagram):
    """Picture size from the answer to the mirror request, or None."""
    pos = datagram.find(b":34\x00")
    if pos < 0 or len(datagram) < pos + 4 + 16:
        return None
    tag, _flags, width, height = struct.unpack_from("<4I", datagram, pos + 4)
    if tag == 1 and 160 <= width <= 4096 and 120 <= height <= 2400:
        return width, height
    return None


def is_key_frame(unit):
    """True if the access unit contains an IDR slice (NAL type 5)."""
    pos = unit.find(b"\x00\x00\x01")
    while pos >= 0 and pos + 3 < len(unit):
        if unit[pos + 3] & 0x1F == 5:
            return True
        pos = unit.find(b"\x00\x00\x01", pos + 3)
    return False


class EZDisplayOutput:
    """Video output for FrameHub: send_video(access_unit) / session_ended()."""

    def __init__(self, host, width, height, name="Plintus", prepare=None):
        self.host = host
        self.width = width
        self.height = height
        self.name = name
        self.prepare = prepare          # called before connecting, e.g. to select the input
        self.sock = None
        self.seq = 0
        self.sent = 0
        self.allowed = False
        self.need_key = True
        self.retry_at = 0.0
        self.failures = 0
        self.lock = threading.Lock()

    # -- connection ------------------------------------------------------------------------
    def _ask(self, udp, datagram, wanted):
        """Send one UDP datagram, return the first answer containing `wanted` (or None)."""
        for _attempt in range(2):
            udp.sendto(datagram, (self.host, PORT))
            deadline = time.monotonic() + 1.5
            while time.monotonic() < deadline:
                try:
                    data, peer = udp.recvfrom(65536)
                except socket.timeout:
                    continue
                if peer[0] == self.host and wanted in data:
                    return data
        return None

    def _handshake(self):
        udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            udp.bind(("", 0))
            udp.settimeout(0.3)
            answer = self._ask(udp, udp_text(1, self.name) + USER.encode() + b"\0", b":3:")
            if answer is None:
                log("no answer to the UDP greeting")
            else:
                log("found: %s" % answer.decode("ascii", "replace").rstrip("\0"))
            answer = self._ask(udp, udp_text(34, self.name) + b"\0" + MIRROR_REQUEST, b":34\x00")
            if answer is None:
                log("no answer to the mirror request")
                return None
            size = parse_size(answer)
            log("mirror request answered, picture size %s" % ("%dx%d" % size if size else "unknown"))
            return size
        except OSError as exc:
            log("UDP handshake failed: %s" % exc)
            return None
        finally:
            udp.close()

    def _connect(self):
        if self.prepare:
            try:
                self.prepare()
            except Exception as exc:
                log("could not prepare the projector: %s" % exc)
        size = self._handshake()
        if size and size != (self.width, self.height):
            log("projector asks for %dx%d but the video is %dx%d" % (size + (self.width, self.height)))
        sock = socket.create_connection((self.host, PORT), timeout=4)
        sock.settimeout(3)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.sendall(hello(self.name))
        sock.sendall(request_stream())
        self.sock = sock
        self.seq = 0
        self.sent = 0
        self.allowed = False
        self.need_key = True
        threading.Thread(target=self._read, args=(sock,), daemon=True).start()
        threading.Thread(target=self._heartbeat, args=(sock,), daemon=True).start()
        log("connected to %s:%d, stream requested" % (self.host, PORT))

    def _read(self, sock):
        buf = b""
        try:
            while self.sock is sock:
                try:
                    data = sock.recv(4096)
                except socket.timeout:
                    continue
                if not data:
                    log("projector closed the connection")
                    break
                buf += data
                while len(buf) >= 8:
                    _seq, length = struct.unpack_from("<2I", buf)
                    if length > 1 << 20:
                        log("unexpected data: %s" % buf[:64].hex())
                        buf = b""
                        break
                    if len(buf) < 8 + length:
                        break
                    body, buf = buf[8:8 + length], buf[8 + length:]
                    self._on_message(body)
        except OSError as exc:
            if self.sock is sock:
                log("connection lost: %s" % exc)
        if self.sock is sock:
            refused = self.sent == 0
            self._drop()
            # A projector that refuses us must not be hammered: every attempt makes it show
            # "processing signal". Three tries per session, 15 s apart.
            if refused:
                self.failures += 1
            self.retry_at = time.monotonic() + (15 if self.failures < 3 else 1e9)
            if self.failures >= 3:
                log("giving up for this session after 3 refused connections")

    def _heartbeat(self, sock):
        beat = heartbeat()
        while self.sock is sock:
            time.sleep(1.0)
            with self.lock:
                if self.sock is not sock:
                    return
                try:
                    sock.sendall(beat)
                except OSError:
                    return

    def _on_message(self, body):
        words = struct.unpack_from("<%dI" % (len(body) // 4), body)
        log("message %s" % " ".join("%x" % w for w in words[:8]))
        if not words:
            return
        if words[0] == CMD_REQUEST_STREAM and len(words) >= 6:
            self.allowed = words[5] == 1
            self.need_key = True
            log("stream request %s (split %d, position %d)"
                % ("accepted" if self.allowed else "DENIED", words[3], words[4]))
        elif words[0] == CMD_NOTIFICATION and len(words) >= 3 and words[2] in (2, 4):
            self.allowed = False

    def _drop(self):
        sock, self.sock = self.sock, None
        self.allowed = False
        if sock:
            try:
                sock.close()
            except OSError:
                pass

    # -- FrameHub interface ----------------------------------------------------------------
    def send_frame(self, jpeg):
        pass                                # this projector takes H.264, not JPEG

    def send_video(self, unit):
        with self.lock:
            if self.sock is None:
                if time.monotonic() < self.retry_at:
                    return
                try:
                    self._connect()
                except OSError as exc:
                    log("connect failed: %s" % exc)
                    self.retry_at = time.monotonic() + 5
                    return
            if not self.allowed:
                return
            if self.need_key:
                if not is_key_frame(unit):
                    return                  # the projector can only start at a key frame
                self.need_key = False
            try:
                self.sock.sendall(video(self.seq, unit, self.width, self.height))
            except OSError as exc:
                log("send failed: %s" % exc)
                self._drop()
                self.retry_at = time.monotonic() + 2
                return
            self.seq += 1
            self.sent += 1
            if self.sent in (1, 30) or self.sent % 1800 == 0:
                log("%d video frames sent" % self.sent)

    def session_ended(self):
        with self.lock:
            if self.sock:
                log("session ended after %d video frames" % self.sent)
            self._drop()
            self.retry_at = 0.0
            self.failures = 0
