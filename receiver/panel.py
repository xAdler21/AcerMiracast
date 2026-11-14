"""Web panel: status of projector, beacon box and receiver, and projector control (PJLink).

Served by miracast_sink.py. Meant to be opened as a top-level page (e.g. the Beamer tab of
the Plintus app).

    GET  /                 the panel
    GET  /api/status       everything the panel shows, as JSON
    POST /api/projector    {"action": "power", "value": "on"|"off"}
                           {"action": "input", "value": "31"}
                           {"action": "mute",  "value": "on"|"off"}
    GET  /stream           live picture of the running session (MJPEG)
    GET  /frame.jpg        newest frame
"""
import hashlib
import http.server
import json
import os
import re
import socket
import threading
import time
import urllib.request

PJLINK_PORT = 4352
POWER = {"0": "off", "1": "on", "2": "cooling", "3": "warming"}
INPUT_KINDS = {"1": "RGB", "2": "Video", "3": "Digital", "4": "Speicher", "5": "Netzwerk"}
ERROR_PARTS = ["Lüfter", "Lampe", "Temperatur", "Abdeckung", "Filter", "Sonstiges"]


# Acer projectors with the ATOP network module (web page "Acer Projector Control"): PJLink on
# these reports a wrong state, the web page's own interface is the one that works.
ACER_INPUTS = [("3", "HDMI 1"), ("6", "HDMI 2"), ("33", "Drahtlos"), ("20", "VGA 1"),
               ("21", "VGA 2"), ("22", "Video"), ("34", "Medien"), ("35", "USB")]
ACER_ERRORS = {"1": "Lüfter blockiert", "2": "Übertemperatur", "3": "Thermoschalter",
               "4": "Lampe zündet nicht", "5": "Lampe ging aus", "6": "Farbrad"}


def acer_post(host, path, body):
    """POST to the projector's web interface; returns its answer {key: value}.

    The answer looks like '<html>\\n{pwr:"1",hid:"0",src:"6"}' (keys unquoted).
    """
    request = urllib.request.Request("http://%s%s" % (host, path), data=body.encode(),
                                     headers={"Content-Type": "application/x-www-form-urlencoded"})
    with urllib.request.urlopen(request, timeout=4) as reply:
        text = reply.read(4096).decode("utf-8", "replace")
    return dict(re.findall(r'(\w+):"([^"]*)"', text))


class PJLinkError(Exception):
    pass


class PJLink:
    """PJLink class 1 client. One short TCP connection per batch of commands."""

    def __init__(self, host, password="", timeout=3.0):
        self.host = host
        self.password = password
        self.timeout = timeout

    def run(self, commands):
        """commands: list of (name, parameter). Returns {name: answer}."""
        result = {}
        with socket.create_connection((self.host, PJLINK_PORT), timeout=self.timeout) as sock:
            sock.settimeout(self.timeout)
            reader = sock.makefile("rb")
            greeting = self._line(reader)
            prefix = ""
            if greeting.upper().startswith("PJLINK 1 "):
                if not self.password:
                    raise PJLinkError("Beamer verlangt ein PJLink-Passwort")
                prefix = hashlib.md5((greeting[9:] + self.password).encode()).hexdigest()
            elif not greeting.upper().startswith("PJLINK 0"):
                raise PJLinkError("unerwartete Begrüßung: %r" % greeting)
            for name, parameter in commands:
                sock.sendall(("%s%%1%s %s\r" % (prefix, name, parameter)).encode())
                prefix = ""                       # the digest is only sent once
                result[name] = self._answer(reader, name)
        return result

    def _answer(self, reader, name):
        """Read lines until the one that answers `name`.

        Some projectors (seen on an Acer network module) answer in lower case and put stray
        NUL / empty lines between answers, so answers are matched by command name, not by order.
        """
        wanted = "%1" + name.lower() + "="
        for _ in range(6):
            line = self._line(reader)
            if line.upper().startswith("PJLINK ERRA"):
                raise PJLinkError("PJLink-Passwort ist falsch")
            if line.lower().startswith(wanted):
                value = line[len(wanted):]
                return value.upper() if value.upper() in ("OK", "ERR1", "ERR2", "ERR3", "ERR4") else value
        raise PJLinkError("keine Antwort auf %s" % name)

    @staticmethod
    def _line(reader):
        data = b""
        while not data.endswith(b"\r") and len(data) < 512:   # PJLink lines end with CR only
            byte = reader.read(1)
            if not byte:
                raise PJLinkError("Verbindung vom Beamer beendet")
            data += byte
        return data.decode("utf-8", "replace").replace("\x00", "").strip()


def describe_input(code):
    return "%s %s" % (INPUT_KINDS.get(code[:1], "Eingang"), code[1:]) if code else ""


class Monitor(threading.Thread):
    """Polls projector and beacon box in the background and keeps the latest state."""

    def __init__(self, config, sink):
        super().__init__(daemon=True)
        self.config = config
        self.sink = sink
        self.lock = threading.Lock()
        self.wake = threading.Event()
        self.projector = {"configured": bool(config.get("projector_host")), "reachable": False}
        self.beacon = {"configured": bool(config.get("beacon_host")), "reachable": False}
        self._beacon_checked = 0.0
        self._waking_until = 0.0
        self._beacon_uptime = None
        self._beacon_changed = time.monotonic()

    def pjlink(self):
        return PJLink(self.config["projector_host"], self.config.get("projector_password", ""))

    def poll_projector(self):
        if not self.config.get("projector_host"):
            return
        state = {"configured": True, "reachable": False, "host": self.config["projector_host"]}
        if self.config.get("projector_control", "acer_web") == "acer_web":
            self._poll_acer(state)
        else:
            self._poll_pjlink(state)
        with self.lock:
            self.projector = state

    def _poll_acer(self, state):
        state["model"] = self.config.get("projector_model", "")
        state["inputs"] = [{"code": code, "label": label} for code, label in ACER_INPUTS]
        state["errors"] = []
        state["lamp_hours"] = None
        try:
            home = acer_post(state["host"], "/form/home_cgi", "QueryControl")
            ctrl = acer_post(state["host"], "/form/control_cgi", "QueryControl")
        except OSError:
            # Switched off, the projector leaves the network completely. That is its normal
            # "off"; it comes back with a Wake-on-LAN packet.
            state["power"] = "warming" if time.monotonic() < self._waking_until else "off"
            state["offline"] = True
            return
        state["reachable"] = True
        self._waking_until = 0.0
        status = home.get("sysstatus", "")
        state["power"] = {"0": "off", "3": "cooling"}.get(status, "on" if status else "unknown")
        if home.get("lamphur", "").isdigit():
            state["lamp_hours"] = int(home["lamphur"])
        error = ACER_ERRORS.get(home.get("errorstatus", "0"))
        if error:
            state["errors"] = [{"part": error, "level": "error"}]
        if state["power"] == "on":
            state["input"] = home.get("displaysrc", "")
            state["input_label"] = dict(ACER_INPUTS).get(state["input"], "")
            # No "mute": the projector's Hide flag stayed at 1 after toggling it back in a
            # test, so the panel offers no picture-off button for this projector.
            state["hide_flag"] = ctrl.get("hid", "")

    def _command_acer(self, action, value):
        host = self.config["projector_host"]
        with self.lock:
            current = dict(self.projector)
        try:
            if action == "power" and value == "on":
                if current.get("reachable"):
                    if current.get("power") == "off":            # standby with network still up
                        acer_post(host, "/form/control_cgi", "pwr=pwr")
                    return None
                wake = self.config.get("wake_url")
                if not wake:
                    return "Einschalten ist nicht eingerichtet (wake_url fehlt)"
                request = urllib.request.Request(wake, data=b"wake", method="POST")
                with urllib.request.urlopen(request, timeout=6) as reply:
                    if b"ok" not in reply.read(200):
                        return "Der Funk-Sender konnte das Weck-Paket nicht schicken"
                self._waking_until = time.monotonic() + 90
                return None
            if not current.get("reachable"):
                return "Der Beamer ist aus"
            if action == "power" and value == "off":
                if current.get("power") == "on":                 # the device only has a toggle
                    acer_post(host, "/form/control_cgi", "pwr=pwr")
            elif action == "input" and value in dict(ACER_INPUTS):
                acer_post(host, "/form/control_cgi", "src=" + value)
            else:
                return "Unbekannter Befehl"
        except OSError:
            return "Der Beamer antwortet nicht"
        return None

    def _poll_pjlink(self, state):
        try:
            link = self.pjlink()
            answer = link.run([("POWR", "?"), ("NAME", "?"), ("INF1", "?"), ("INF2", "?"),
                               ("INST", "?"), ("ERST", "?"), ("LAMP", "?")])
            state["reachable"] = True
            state["power"] = POWER.get(answer.get("POWR", ""), "unknown")
            state["name"] = answer.get("NAME", "")
            state["model"] = " ".join(x for x in (answer.get("INF1", ""), answer.get("INF2", ""))
                                      if x and not x.startswith("ERR"))
            inputs = answer.get("INST", "")
            state["inputs"] = ([{"code": c, "label": describe_input(c)} for c in inputs.split()]
                               if inputs and not inputs.startswith("ERR") else [])
            errors = answer.get("ERST", "")
            state["errors"] = ([{"part": ERROR_PARTS[i], "level": "warning" if d == "1" else "error"}
                                for i, d in enumerate(errors[:6]) if d in "12"]
                               if len(errors) >= 6 and errors[:6].isdigit() else [])
            lamp = answer.get("LAMP", "").split()
            state["lamp_hours"] = int(lamp[0]) if lamp and lamp[0].isdigit() else None
            if state["power"] == "on":                 # only answered while the lamp is on
                more = link.run([("INPT", "?"), ("AVMT", "?")])
                code = more.get("INPT", "")
                state["input"] = code if code and not code.startswith("ERR") else ""
                state["input_label"] = describe_input(state["input"])
                state["mute"] = more.get("AVMT", "") in ("11", "31")
        except (OSError, PJLinkError) as exc:
            state["error"] = str(exc)

    def poll_beacon(self):
        host = self.config.get("beacon_host")
        if not host:
            return
        state = {"configured": True, "reachable": False, "host": host}
        try:
            with urllib.request.urlopen("http://%s/beacon.json" % host, timeout=4) as reply:
                state.update(json.loads(reply.read(4096).decode("utf-8", "replace")))
            state["reachable"] = True
            # The box has no reliable clock. Its report is stale when the uptime in it
            # has not moved for three minutes (the box rewrites the file every minute).
            uptime = state.get("uptime")
            if uptime != self._beacon_uptime:
                self._beacon_uptime, self._beacon_changed = uptime, time.monotonic()
            state["stale"] = time.monotonic() - self._beacon_changed > 180
        except (OSError, ValueError):
            state["error"] = "antwortet nicht"
        with self.lock:
            self.beacon = state

    def run(self):
        while True:
            self.poll_projector()
            if time.monotonic() - self._beacon_checked > 20:
                self._beacon_checked = time.monotonic()
                self.poll_beacon()
            self.wake.wait(5)
            self.wake.clear()

    def command(self, action, value):
        """Send one control command to the projector. Returns an error text or None."""
        if not self.config.get("projector_host"):
            return "Kein Beamer konfiguriert"
        if self.config.get("projector_control", "acer_web") == "acer_web":
            error = self._command_acer(action, value)
            self.wake.set()
            return error
        if action == "power" and value in ("on", "off"):
            cmd = ("POWR", "1" if value == "on" else "0")
        elif action == "mute" and value in ("on", "off"):
            cmd = ("AVMT", "31" if value == "on" else "30")
        elif action == "input" and len(value) == 2 and value.isalnum():
            cmd = ("INPT", value)
        else:
            return "Unbekannter Befehl"
        try:
            answer = self.pjlink().run([cmd])[cmd[0]]
        except (OSError, PJLinkError) as exc:
            return str(exc)
        self.wake.set()
        if answer != "OK":
            return {"ERR2": "Wert wird vom Beamer nicht unterstützt",
                    "ERR3": "Im Moment nicht möglich (Beamer aus oder beschäftigt)",
                    "ERR4": "Beamer meldet einen Gerätefehler"}.get(answer, "Antwort: %s" % answer)
        return None

    def status(self):
        with self.lock:
            projector, beacon = dict(self.projector), dict(self.beacon)
        session = self.sink.session
        receiver = {"session": None, "name": self.config.get("room_name", "")}
        if session is not None:
            receiver["session"] = {
                "source": session.name, "ip": session.source_ip, "playing": session.playing,
                "seconds": int(time.time() - session.started),
                "frames": session.decoder.frames if session.decoder else 0,
            }
        return {"projector": projector, "beacon": beacon, "receiver": receiver, "time": int(time.time())}


class PanelHandler(http.server.BaseHTTPRequestHandler):
    hub = None
    monitor = None
    page = b""

    def log_message(self, *_args):
        pass

    def _send(self, code, content_type, body):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, code, data):
        self._send(code, "application/json", json.dumps(data).encode())

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/":
            self._send(200, "text/html; charset=utf-8", self.page)
        elif path == "/api/status":
            self._json(200, self.monitor.status())
        elif path == "/frame.jpg":
            _serial, frame = self.hub.wait(-1, 0)
            if frame is None:
                self.send_error(503, "no session")
            else:
                self._send(200, "image/jpeg", frame)
        elif path == "/stream":
            self.send_response(200)
            self.send_header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            serial = -1
            try:
                while True:
                    serial, frame = self.hub.wait(serial)
                    if frame is None:
                        continue
                    self.wfile.write(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: "
                                     + str(len(frame)).encode() + b"\r\n\r\n" + frame + b"\r\n")
            except OSError:
                return
        else:
            self.send_error(404)

    def do_POST(self):
        if self.path != "/api/projector":
            self.send_error(404)
            return
        # Browsers cannot send JSON cross-site without a preflight, which we never answer.
        if "application/json" not in self.headers.get("Content-Type", ""):
            self._json(415, {"ok": False, "error": "Content-Type muss application/json sein"})
            return
        try:
            length = min(int(self.headers.get("Content-Length", "0")), 1024)
            data = json.loads(self.rfile.read(length).decode())
            action, value = str(data["action"]), str(data["value"])
        except (ValueError, KeyError, TypeError):
            self._json(400, {"ok": False, "error": "Ungültige Anfrage"})
            return
        error = self.monitor.command(action, value)
        self._json(200 if error is None else 502, {"ok": error is None, "error": error})


def start(bind, port, hub, sink, config):
    monitor = Monitor(config, sink)
    monitor.start()
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "panel.html"), "rb") as handle:
        PanelHandler.page = handle.read()
    PanelHandler.hub = hub
    PanelHandler.monitor = monitor
    httpd = http.server.ThreadingHTTPServer((bind, port), PanelHandler)
    httpd.daemon_threads = True
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    return monitor
