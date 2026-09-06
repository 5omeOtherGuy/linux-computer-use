"""One persistent X11 connection, with recoverable ownership of synthetic input."""

import json
import os
import time
from pathlib import Path

from Xlib import X, XK, display, protocol
from Xlib.ext import xtest

from .errors import DesktopError


class X11:
    def __init__(self, ownership_path):
        self.connection = display.Display()
        if not self.connection.has_extension("XTEST"):
            raise DesktopError("UNSUPPORTED", "X11 server has no XTEST extension")
        self.root = self.connection.screen().root
        self.ownership_path = Path(ownership_path)
        self.owned = {"keys": [], "buttons": [], "maps": {}}
        self.atoms = {}

    def atom(self, name):
        if name not in self.atoms:
            self.atoms[name] = self.connection.intern_atom(name)
        return self.atoms[name]

    def prop(self, window, name, default=None):
        result = window.get_full_property(self.atom(name), X.AnyPropertyType)
        return default if result is None else result.value

    def window(self, window_id):
        return self.connection.create_resource_object("window", int(window_id))

    def geometry(self, window_id):
        window = self.window(window_id)
        geometry = window.get_geometry()
        point = self.root.translate_coords(window, 0, 0)
        return dict(x=point.x, y=point.y, width=geometry.width, height=geometry.height)

    def windows(self):
        result = []
        for wid in self.prop(self.root, "_NET_CLIENT_LIST", []):
            try:
                w = self.window(wid)
                title = self.prop(w, "_NET_WM_NAME", b"")
                if isinstance(title, bytes):
                    title = title.decode("utf-8", errors="replace")
                pid = self.prop(w, "_NET_WM_PID", [0])
                result.append(dict(id=int(wid), title=str(title or w.get_wm_name() or ""),
                                   app=list(w.get_wm_class() or []), pid=int(pid[0]),
                                   visible=w.get_attributes().map_state == X.IsViewable,
                                   **self.geometry(wid)))
            except Exception:
                continue
        return result

    def active(self):
        values = self.prop(self.root, "_NET_ACTIVE_WINDOW", [0])
        return int(values[0])

    def screens(self):
        root = self.root.get_geometry()
        monitors = []
        try:
            for m in self.root.xrandr_get_monitors(True).monitors:
                monitors.append(dict(name=self.connection.get_atom_name(m.name), x=m.x, y=m.y,
                                     width=m.width_in_pixels, height=m.height_in_pixels,
                                     primary=bool(m.primary)))
        except Exception:
            pass
        return {"width": root.width, "height": root.height, "monitors": monitors}

    def send_wm(self, wid, name, data):
        self.root.send_event(protocol.event.ClientMessage(window=int(wid), client_type=self.atom(name),
                                                         data=(32, (data + [0] * 5)[:5])),
                             event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
        self.connection.sync()

    def focus(self, wid):
        self.send_wm(wid, "_NET_ACTIVE_WINDOW", [2, X.CurrentTime, self.active()])
        deadline = time.monotonic() + 2
        while self.active() != int(wid):
            if time.monotonic() >= deadline:
                raise DesktopError("FOCUS_FAILED", "Window manager did not activate the requested window")
            time.sleep(.01)

    def close_window(self, wid):
        self.send_wm(wid, "_NET_CLOSE_WINDOW", [X.CurrentTime, 2])

    def configure(self, wid, **geometry):
        allowed = {"x", "y", "width", "height"}
        if not geometry or set(geometry) - allowed:
            raise ValueError("Use x, y, width, height")
        values = {key: int(value) for key, value in geometry.items()}
        if any(values.get(k, 1) <= 0 for k in ("width", "height")):
            raise ValueError("Window dimensions must be positive")
        self.window(wid).configure(**values)
        self.connection.sync()

    def pointer(self):
        p = self.root.query_pointer()
        return {"x": p.root_x, "y": p.root_y, "mask": p.mask}

    def _save(self):
        temporary = self.ownership_path.with_suffix(".tmp")
        temporary.write_text(json.dumps(self.owned))
        os.replace(temporary, self.ownership_path)

    def move(self, x, y):
        bounds = self.screens()
        if not (0 <= x < bounds["width"] and 0 <= y < bounds["height"]):
            raise DesktopError("OUT_OF_BOUNDS", "Pointer coordinates are outside the desktop")
        xtest.fake_input(self.connection, X.MotionNotify, x=int(x), y=int(y))
        self.connection.sync()

    def button_down(self, button=1):
        if button not in range(1, 8):
            raise ValueError("Button must be 1..7")
        if button in self.owned["buttons"]:
            raise DesktopError("INPUT_HELD", "This run already holds that button")
        if button <= 5 and self.pointer()["mask"] & (1 << (7 + button)):
            raise DesktopError("INPUT_BUSY", "That button is already held by another input source")
        self.owned["buttons"].append(button)
        self._save()
        xtest.fake_input(self.connection, X.ButtonPress, button)
        self.connection.sync()

    def button_up(self, button=1):
        if button in self.owned["buttons"]:
            xtest.fake_input(self.connection, X.ButtonRelease, button)
            self.connection.sync()
            self.owned["buttons"].remove(button)
            self._save()

    def _key_down(self, code):
        if code in self.owned["keys"]:
            raise DesktopError("INPUT_HELD", "This run already holds that key")
        if self.connection.query_keymap()[code // 8] & (1 << (code % 8)):
            raise DesktopError("INPUT_BUSY", "That key is already held by another input source")
        self.owned["keys"].append(code)
        self._save()
        xtest.fake_input(self.connection, X.KeyPress, code)
        self.connection.sync()

    def _key_up(self, code):
        if code in self.owned["keys"]:
            xtest.fake_input(self.connection, X.KeyRelease, code)
            self.connection.sync()
            self.owned["keys"].remove(code)
            self._save()

    def keycode(self, name):
        aliases = {"ctrl": "Control_L", "control": "Control_L", "shift": "Shift_L",
                   "alt": "Alt_L", "super": "Super_L", "meta": "Super_L",
                   "enter": "Return", "escape": "Escape", "space": "space",
                   "backspace": "BackSpace", "delete": "Delete"}
        name = aliases.get(name.lower(), name)
        sym = XK.string_to_keysym(name)
        if not sym and len(name) == 1:
            sym = ord(name)
        code = self.connection.keysym_to_keycode(sym)
        if not code:
            raise DesktopError("UNKNOWN_KEY", f"Unknown keyboard key: {name}")
        return code

    def press_key(self, chord, duration=0):
        if not 0 <= duration <= 5:
            raise ValueError("Key duration must be 0..5 seconds")
        codes = [self.keycode(n.strip()) for n in chord.split("+")]
        pressed = []
        try:
            for code in codes:
                self._key_down(code)
                pressed.append(code)
            if duration:
                time.sleep(duration)
        finally:
            for code in reversed(pressed):
                self._key_up(code)

    def type_text(self, text):
        """Type Unicode without changing the user's clipboard."""
        if len(text) > 100000:
            raise ValueError("Text exceeds 100,000 characters; use accessibility for large fields")
        shift = self.keycode("Shift_L")
        try:
            for char in text:
                if char in "\n\t":
                    self.press_key("Return" if char == "\n" else "Tab")
                    continue
                sym = ord(char) if ord(char) < 256 else 0x01000000 | ord(char)
                mappings = [(code, level) for code, level in self.connection.keysym_to_keycodes(sym)
                            if level in (0, 1)]
                temporary = not mappings
                if temporary:
                    info = self.connection.display.info
                    maps = self.connection.get_keyboard_mapping(info.min_keycode, info.max_keycode-info.min_keycode+1)
                    free = [info.min_keycode+i for i, row in enumerate(maps) if not any(row)]
                    if not free:
                        raise DesktopError("NO_KEYCODE", "No unused keycode for Unicode input; use set_value")
                    code, level = free[-1], 0
                    self.owned["maps"][str(code)] = list(maps[code-info.min_keycode])
                    self._save()
                    self.connection.change_keyboard_mapping(code, [(sym, 0)])
                    self.connection.sync()
                    # XSync only drains our connection. Other clients must consume
                    # MappingNotify before translating this otherwise unmapped key.
                    time.sleep(.012)
                else:
                    code, level = mappings[0]
                try:
                    if level:
                        self._key_down(shift)
                    self._key_down(code)
                    self._key_up(code)
                finally:
                    self._key_up(code)
                    if level:
                        self._key_up(shift)
                    if temporary:
                        # Let the app translate queued key events before restoring
                        # the scratch mapping (the same pacing used by xdotool).
                        time.sleep(.012)
                        self.connection.change_keyboard_mapping(code, [self.owned["maps"][str(code)]])
                        self.connection.sync()
                        del self.owned["maps"][str(code)]
                        self._save()
        finally:
            self.release()

    def release(self):
        for code in list(reversed(self.owned["keys"])):
            self._key_up(code)
        for button in list(reversed(self.owned["buttons"])):
            self.button_up(button)
        for code, mapping in self.owned["maps"].items():
            self.connection.change_keyboard_mapping(int(code), [mapping])
        self.connection.sync()
        self.owned["maps"] = {}
        self._save()


def recover_inputs(path):
    """Called by an independent supervisor after a worker is terminated."""
    p = Path(path)
    if not p.exists():
        return
    owned = json.loads(p.read_text())
    driver = X11(p)
    driver.owned = owned
    driver.release()
