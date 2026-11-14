"""Picture output to an Acer / EZCast Pro projector ("EZ Display" protocol, port 2425).

One connection per projection session:
  1. UDP knock:  "2:<packet no>:<user>:<host>:1:<user>\\0" to the projector's port 2425.
     The projector answers over UDP; the answer carries its picture size.
  2. TCP connect to port 2425, then two messages:
       hello            {0, n+24, 9, 0x1000, 1, 0, 0, n} + host name
       request stream   {0, 24,   6, 0x1000, 1, 0, 0, 4}
  3. For every picture:
       {seq, size+24} {2, 0x1000, 4, width, height, size} + JPEG
All numbers are 32 bit little endian. The projector answers on the TCP connection
(6 = result of the request, 7 = notification); everything it sends is logged.

Not yet confirmed against a real projector.
"""
import datetime
import socket
import struct
import threading
import time

PORT = 2425
CMD_PICTURE = 2
CMD_REQUEST_STREAM = 6
CMD_NOTIFICATION = 7
CMD_HELLO = 9
FORMAT_JPEG_STREAM = 4
NOTIFICATIONS = {1: "change", 2: "stop stream", 3: "request stream again", 4: "disconnect"}


def log(text):
    print("%s  projector: %s" % (datetime.datetime.now().strftime("%H:%M:%S"), text), flush=True)


def message(seq, command, a, b, c, d, payload=b""):
    body = struct.pack("<6I", command, 0x1000, a, b, c, d) + payload
    return struct.pack("<2I", seq, len(body)) + body


def hello(name):
    raw = name.encode("ascii", "replace")
    return message(0, CMD_HELLO, 1, 0, 0, len(raw), raw)


def request_stream(split_count=0, position=0):
    return message(0, CMD_REQUEST_STREAM, 1, split_count, position, FORMAT_JPEG_STREAM)


def picture(seq, jpeg, width, height):
    return message(seq, CMD_PICTURE, FORMAT_JPEG_STREAM, width, height, len(jpeg), jpeg)


def knock(packet_no, user, host):
    return ("2:%u:%s:%s:1:%s" % (packet_no, user, host, user)).encode("ascii", "replace") + b"\0"


def find_size(datagram):
    """Look for {1, 1, width, height} in a UDP answer; returns (width, height) or None."""
    for pos in range(0, len(datagram) - 15):
        tag, flag, width, height = struct.unpack_from("<4I", datagram, pos)
        if tag == 1 and flag & 0xFF == 1 and 320 <= width <= 4096 and 240 <= height <= 2400:
            return width, height
    return None


class EZDisplayOutput:
    """Frame output for FrameHub: send_frame(jpeg) / session_ended()."""

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
        self.retry_at = 0.0
        self.lock = threading.Lock()

    # -- connection ------------------------------------------------------------------------
    def _knock(self):
        udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            try:
                udp.bind(("", PORT))            # the original software uses its port 2425
            except OSError:
                udp.bind(("", 0))
            udp.settimeout(0.5)
            udp.sendto(knock(int(time.time()), self.name, self.name), (self.host, PORT))
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                try:
                    data, peer = udp.recvfrom(65536)
                except socket.timeout:
                    continue
                if peer[0] != self.host:
                    continue
                log("UDP answer (%d bytes): %s" % (len(data), data[:160].hex()))
                size = find_size(data)
                if size:
                    return size
        except OSError as exc:
            log("UDP knock failed: %s" % exc)
        finally:
            udp.close()
        return None

    def _connect(self):
        if self.prepare:
            try:
                self.prepare()
            except Exception as exc:
                log("could not prepare the projector: %s" % exc)
        size = self._knock()
        if size and size != (self.width, self.height):
            log("projector asks for %dx%d, pictures are %dx%d" % (size + (self.width, self.height)))
        elif not size:
            log("no picture size in the UDP answer, using %dx%d" % (self.width, self.height))
        sock = socket.create_connection((self.host, PORT), timeout=4)
        sock.settimeout(3)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.sendall(hello(self.name))
        sock.sendall(request_stream())
        self.sock = sock
        self.seq = 0
        self.sent = 0
        self.allowed = False
        threading.Thread(target=self._read, args=(sock,), daemon=True).start()
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
        except OSError:
            pass
        if self.sock is sock:
            self._drop()

    def _on_message(self, body):
        words = struct.unpack_from("<%dI" % (len(body) // 4), body)
        log("message %s" % " ".join("%x" % w for w in words[:8]))
        if not words:
            return
        if words[0] == CMD_REQUEST_STREAM:
            self.allowed = len(words) >= 6 and words[5] == 1
            log("stream request %s" % ("accepted" if self.allowed else "DENIED"))
        elif words[0] == CMD_NOTIFICATION and len(words) >= 3:
            log("notification: %s" % NOTIFICATIONS.get(words[2], words[2]))
            if words[2] == 3 and self.sock:
                try:
                    self.sock.sendall(request_stream())
                except OSError:
                    pass
            elif words[2] in (2, 4):
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
            try:
                self.sock.sendall(picture(self.seq, jpeg, self.width, self.height))
            except OSError as exc:
                log("send failed: %s" % exc)
                self._drop()
                self.retry_at = time.monotonic() + 2
                return
            self.seq += 1
            self.sent += 1
            if self.sent in (1, 30) or self.sent % 900 == 0:
                log("%d pictures sent" % self.sent)

    def session_ended(self):
        with self.lock:
            if self.sock:
                log("session ended after %d pictures" % self.sent)
            self._drop()
            self.retry_at = 0.0
