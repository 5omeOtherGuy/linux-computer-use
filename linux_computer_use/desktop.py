"""The persistent, app-scoped API exposed to the model."""

import base64
import io
import json
import os
import subprocess
import time
import uuid
from pathlib import Path

from PIL import Image, ImageGrab

from .errors import DesktopError
from .x11 import X11

SKY_BINARY = Path('/usr/lib/chatgpt/resources/cua_node/lib/node_modules/@oai/sky/bin/linux/sky_linux_x64')


class Desktop:
    def __init__(self, ownership_path, emit):
        self.x = X11(ownership_path)
        self.emit = emit
        self.session = uuid.uuid4().hex
        self.sequence = 0
        self.actions = []
        self.observed = {}
        self.last_observations = {}
        self.a11y_error = None
        try:
            from .accessibility import Accessibility
            self.a11y = Accessibility()
        except Exception as error:
            self.a11y = None
            self.a11y_error = str(error)

    def status(self):
        return {"version": "0.1.0", "session": self.session, "display": os.environ.get("DISPLAY"),
                "session_type": os.environ.get("XDG_SESSION_TYPE", "unknown"),
                "capabilities": {"x11_input": True, "screenshots": True,
                                 "accessibility": self.a11y is not None, "persistent_execution": True,
                                 "native_sky": SKY_BINARY.exists(), "wayland_native": False,
                                 "window_control": True, "clipboard_preserved_by_typing": True},
                "accessibility_error": self.a11y_error, "screen": self.x.screens(),
                "active_window": self.x.active(), "windows": self.windows()}

    def windows(self):
        return self.x.windows()

    def apps(self):
        """List installed launchable desktop applications without launching them."""
        import gi
        gi.require_version("Gio", "2.0")
        from gi.repository import Gio
        return sorted([dict(id=a.get_id(), name=a.get_display_name()) for a in Gio.AppInfo.get_all()
                       if a.get_id() and a.should_show()], key=lambda a: a["name"].lower())

    def launch(self, app_id):
        import gi
        gi.require_version("Gio", "2.0")
        from gi.repository import Gio
        app = Gio.DesktopAppInfo.new(app_id)
        if app is None:
            raise DesktopError("APP_NOT_FOUND", "Use an exact desktop-file ID returned by desktop.apps()")
        # Launched GUI apps may inherit stdio: pipes would keep communicate() waiting
        # after gio itself exits. Do not attach their lifetime to the desktop worker.
        completed = subprocess.run(["gio", "launch", app.get_filename()], stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, start_new_session=True, timeout=10)
        if completed.returncode:
            raise DesktopError("LAUNCH_FAILED", f"Desktop launcher exited with code {completed.returncode}")
        self.record("launch", app_id=app_id)

    def window(self, target=None):
        windows = self.windows()
        if target is None:
            target = self.x.active()
        if isinstance(target, int) or (isinstance(target, str) and target.isdigit()):
            found = [w for w in windows if w["id"] == int(target)]
        else:
            found = [w for w in windows if w["title"] == target or target in w["app"]]
            if not found:
                found = [w for w in windows if str(target).lower() in w["title"].lower()]
        if len(found) != 1:
            raise DesktopError("AMBIGUOUS_WINDOW" if found else "WINDOW_NOT_FOUND",
                               "Choose an exact unique window ID from desktop.windows()")
        return Window(self, found[0])

    def record(self, action, **details):
        self.actions.append({"action": action, "status": "executed", **details})

    def display(self, image):
        if not isinstance(image, Image.Image):
            raise TypeError("display() expects a Pillow image, e.g. desktop.capture()")
        buffer = io.BytesIO()
        image.save(buffer, format="PNG")
        data = buffer.getvalue()
        if len(data) > 16 * 1024 * 1024:
            raise DesktopError("IMAGE_TOO_LARGE", "Capture a window or smaller region")
        self.emit({"type": "image", "mimeType": "image/png", "data": base64.b64encode(data).decode(),
                   "_meta": {"codex/imageDetail": "original"}})

    def capture(self, region=None):
        """Return an in-memory Pillow image. Region is [x,y,width,height] in desktop pixels."""
        bbox = None
        if region is not None:
            if len(region) != 4:
                raise ValueError("Region is [x,y,width,height]")
            x, y, w, h = map(int, region)
            bounds = self.x.screens()
            if w <= 0 or h <= 0 or x < 0 or y < 0 or x+w > bounds["width"] or y+h > bounds["height"]:
                raise DesktopError("OUT_OF_BOUNDS", "Capture region must be inside the desktop")
            bbox = (x, y, x+w, y+h)
        return ImageGrab.grab(bbox=bbox, xdisplay=os.environ.get("DISPLAY"))

    def observe(self, window=None, screenshot=True, max_elements=250, diff=True):
        if not 1 <= max_elements <= 1000:
            raise ValueError("max_elements must be 1..1000")
        target = self.window(window) if window is not None else None
        self.sequence += 1
        result = {"state_id": f"{self.session}:{self.sequence}", "captured_at": time.time(),
                  "active_window": self.x.active(), "screen": self.x.screens()}
        region = None
        if target:
            if screenshot:
                target.focus()
                result["active_window"] = self.x.active()
            current = target.info()
            result["window"] = current
            accessibility = target._snapshot(max_elements)
            current_elements = {item["id"]: item for item in accessibility["elements"]}
            previous = self.last_observations.get(target.id)
            if diff and previous and not accessibility.get("truncated") and not previous[2]:
                result["accessibility"] = {**accessibility, "mode": "diff", "base_state_id": previous[0],
                    "elements": [item for key, item in current_elements.items() if previous[1].get(key) != item],
                    "removed": [key for key in previous[1] if key not in current_elements]}
            else:
                result["accessibility"] = {**accessibility, "mode": "full"}
            self.last_observations[target.id] = (result["state_id"], current_elements, accessibility.get("truncated"))
            geometry = {k: current[k] for k in ("x", "y", "width", "height")}
            self.observed[target.id] = geometry
            bounds = self.x.screens()
            x, y = max(0, current["x"]), max(0, current["y"])
            right, bottom = min(bounds["width"], current["x"]+current["width"]), min(bounds["height"], current["y"]+current["height"])
            if right <= x or bottom <= y:
                screenshot = False
                result["screenshot_unavailable"] = "Window is outside the visible desktop"
            else:
                region = [x, y, right-x, bottom-y]
        else:
            result["windows"] = self.windows()
            self.observed[0] = result["screen"]
        result["image_coordinates"] = {"origin": (region or [0, 0])[:2], "scale": 1,
                                       "input_space": "desktop physical pixels",
                                       "window_coordinates": "relative to window client area"}
        self.emit({"type": "text", "text": json.dumps(result, ensure_ascii=False)})
        if screenshot:
            self.display(self.capture(region))
            if target and self.x.geometry(target.id) != self.observed[target.id]:
                self.observed.pop(target.id, None)
                raise DesktopError("GEOMETRY_CHANGED", "Window moved during capture; observe again")
        return result

    def move(self, x, y):
        if 0 not in self.observed:
            raise DesktopError("OBSERVATION_REQUIRED", "Call desktop.observe() before desktop coordinates")
        if self.x.screens() != self.observed[0]:
            raise DesktopError("GEOMETRY_CHANGED", "Display geometry changed; observe again")
        self.x.move(int(x), int(y))
        self.record("move", x=x, y=y)

    def native(self, action, **arguments):
        """Optional bundled Sky backend, restricted to its documented input actions."""
        allowed = {"move"}
        if action not in allowed or not SKY_BINARY.is_file():
            raise DesktopError("UNSUPPORTED", "Native Sky action unavailable")
        if 0 not in self.observed:
            raise DesktopError("OBSERVATION_REQUIRED", "Observe the desktop before native global input")
        if set(arguments) != {"x", "y"}:
            raise ValueError("Native move accepts exactly x and y; use the regular API for tracked input")
        bounds = self.x.screens()
        if not 0 <= arguments["x"] < bounds["width"] or not 0 <= arguments["y"] < bounds["height"]:
            raise DesktopError("OUT_OF_BOUNDS", "Native pointer coordinates must be inside the desktop")
        result = subprocess.run([str(SKY_BINARY), action], input=json.dumps(arguments), text=True,
                                capture_output=True, timeout=10)
        if result.returncode:
            raise DesktopError("NATIVE_FAILED", result.stderr[-2000:])
        self.record("native_" + action)

    def click(self, x, y, button=1, count=1):
        return _Screen(self).click(x, y, button, count)

    def press_key(self, chord, duration=0):
        return _Screen(self).press_key(chord, duration)

    def type_text(self, text):
        return _Screen(self).type_text(text)

    def scroll(self, direction, steps=3, x=None, y=None):
        return _Screen(self).scroll(direction, steps, x, y)

    def drag(self, path, duration=.25):
        return _Screen(self).drag(path, duration)


class Window:
    def __init__(self, desktop, info):
        self.desktop = desktop
        self.id = info["id"]
        self.pid = info["pid"]

    def info(self):
        candidates = [w for w in self.desktop.windows() if w["id"] == self.id and w["pid"] == self.pid]
        if len(candidates) != 1:
            raise DesktopError("STALE_WINDOW", "Window closed or its identity changed; discover it again")
        return candidates[0]

    def observe(self, screenshot=True, max_elements=250, diff=True):
        return self.desktop.observe(self.id, screenshot, max_elements, diff)

    def _snapshot(self, limit=250):
        if self.desktop.a11y is None:
            return {"elements": [], "unavailable": self.desktop.a11y_error}
        try:
            return self.desktop.a11y.snapshot(self.info(), limit)
        except DesktopError as error:
            return {"elements": [], "unavailable": str(error)}

    def find(self, name=None, role=None, text=None, max_elements=1000):
        if name is None and role is None and text is None:
            raise ValueError("Specify a name, role, or exact text")
        if not 1 <= max_elements <= 1000:
            raise ValueError("max_elements must be 1..1000")
        snapshot = self._snapshot(max_elements)
        if snapshot.get("unavailable"):
            raise DesktopError("NO_ACCESSIBILITY", snapshot["unavailable"])
        if snapshot.get("truncated"):
            raise DesktopError("SEARCH_TRUNCATED", "Cannot prove a unique match in a truncated tree; observe and use an explicit element handle or physical input")
        matches = [e for e in snapshot["elements"] if
                   (name is None or e["name"] == name) and (role is None or e["role"] == role)
                   and (text is None or e.get("text") == text) and e["showing"]]
        if len(matches) != 1:
            raise DesktopError("AMBIGUOUS_ELEMENT" if matches else "ELEMENT_NOT_FOUND",
                               f"Expected one matching visible control, found {len(matches)}; observe to inspect")
        return Element(self, matches[0])

    def element(self, handle):
        if self.desktop.a11y is None:
            raise DesktopError("NO_ACCESSIBILITY", "Accessibility is unavailable")
        self.desktop.a11y.resolve(handle, self.id)
        return Element(self, {"id": handle})

    def focus(self):
        self.info()
        self.desktop.x.focus(self.id)
        self.desktop.record("focus", window=self.id)

    def _physical(self):
        self.info()
        observed = self.desktop.observed.get(self.id)
        if observed is None:
            raise DesktopError("OBSERVATION_REQUIRED", "Call window.observe() before physical input")
        if observed != self.desktop.x.geometry(self.id):
            raise DesktopError("GEOMETRY_CHANGED", "Window moved or resized; observe again")
        self.focus()
        return observed

    def click(self, x, y, button=1, count=1):
        if count not in (1, 2, 3):
            raise ValueError("Click count must be 1, 2, or 3")
        g = self._physical()
        if not (0 <= x < g["width"] and 0 <= y < g["height"]):
            raise DesktopError("OUT_OF_BOUNDS", "Click must be inside the observed client area")
        self.desktop.x.move(g["x"]+int(x), g["y"]+int(y))
        for _ in range(count):
            try:
                self.desktop.x.button_down(button)
            finally:
                self.desktop.x.button_up(button)
            if count > 1:
                time.sleep(.04)
        self.desktop.record("click", window=self.id, x=x, y=y)

    def press_key(self, chord, duration=0):
        self._physical()
        self.desktop.x.press_key(chord, duration)
        self.desktop.record("press_key", window=self.id, chord=chord)

    def type_text(self, text):
        self._physical()
        self.desktop.x.type_text(text)
        self.desktop.record("type_text", window=self.id, characters=len(text))

    def scroll(self, direction, steps=3, x=None, y=None):
        if direction not in ("up", "down", "left", "right") or not 1 <= steps <= 100:
            raise ValueError("Use up/down/left/right and 1..100 steps")
        g = self._physical()
        x, y = g["width"]/2 if x is None else x, g["height"]/2 if y is None else y
        if not (0 <= x < g["width"] and 0 <= y < g["height"]):
            raise DesktopError("OUT_OF_BOUNDS", "Scroll position must be inside the window")
        self.desktop.x.move(g["x"] + int(x), g["y"] + int(y))
        button = {"up": 4, "down": 5, "left": 6, "right": 7}[direction]
        for _ in range(steps):
            try:
                self.desktop.x.button_down(button)
            finally:
                self.desktop.x.button_up(button)
        self.desktop.record("scroll", window=self.id, direction=direction, steps=steps)

    def drag(self, path, duration=.25):
        if len(path) < 2 or not 0 <= duration <= 10:
            raise ValueError("Drag needs at least two points and a duration of 0..10 seconds")
        g = self._physical()
        if any(len(p) != 2 or not (0 <= p[0] < g["width"] and 0 <= p[1] < g["height"]) for p in path):
            raise DesktopError("OUT_OF_BOUNDS", "Drag points must be inside the window")
        self.desktop.x.move(g["x"]+int(path[0][0]), g["y"]+int(path[0][1]))
        try:
            self.desktop.x.button_down()
            for x, y in path[1:]:
                if duration:
                    time.sleep(duration / (len(path)-1))
                self.desktop.x.move(g["x"]+int(x), g["y"]+int(y))
        finally:
            self.desktop.x.button_up()
        self.desktop.record("drag", window=self.id)

    def wait_for(self, name=None, role=None, text=None, checked=None, timeout=5):
        if not 0 < timeout <= 30:
            raise ValueError("Timeout must be between 0 and 30 seconds")
        deadline = time.monotonic()+timeout
        while True:
            try:
                element = self.find(name, role, text)
                if checked is None or element.data.get("checked") == checked:
                    return element
            except DesktopError as error:
                if error.code != "ELEMENT_NOT_FOUND":
                    raise
            if time.monotonic() >= deadline:
                raise DesktopError("WAIT_TIMEOUT", "Expected control state did not appear")
            if self.desktop.a11y:
                self.desktop.a11y.changed.wait(min(.05, max(0, deadline-time.monotonic())))
                self.desktop.a11y.changed.clear()
            else:
                time.sleep(.05)

    def configure(self, **geometry):
        self.info()
        self.desktop.x.configure(self.id, **geometry)
        self.desktop.observed.pop(self.id, None)
        self.desktop.record("configure", window=self.id, geometry=geometry)

    def close(self):
        self.info()
        self.desktop.x.close_window(self.id)
        self.desktop.observed.pop(self.id, None)
        self.desktop.record("close_requested", window=self.id)


class _Screen(Window):
    """Physical input to desktop panels, decorations and other non-client surfaces."""

    def __init__(self, desktop):
        self.desktop = desktop
        self.id = 0

    def _physical(self):
        observed = self.desktop.observed.get(0)
        if observed is None:
            raise DesktopError("OBSERVATION_REQUIRED", "Call desktop.observe() before global input")
        if self.desktop.x.screens() != observed:
            raise DesktopError("GEOMETRY_CHANGED", "Display geometry changed; observe again")
        return dict(x=0, y=0, width=observed["width"], height=observed["height"])


class Element:
    def __init__(self, window, data):
        self.window = window
        self.data = data
        self.id = data["id"]

    def click(self, action=None):
        self.window.info()
        self.window.desktop.a11y.action(self.id, self.window.id, action)
        self.window.desktop.record("accessibility_action", window=self.window.id, element=self.id)

    def set_value(self, value):
        self.window.info()
        self.window.desktop.a11y.set_value(self.id, self.window.id, value)
        self.window.desktop.record("set_value", window=self.window.id, element=self.id)

    def select_text(self, text):
        self.window.desktop.a11y.select_text(self.id, self.window.id, text)
        self.window.desktop.record("select_text", window=self.window.id, element=self.id)
