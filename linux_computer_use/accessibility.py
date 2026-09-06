"""Bounded AT-SPI observations and handles that never recycle numeric indices."""

import threading
import time
import uuid
from collections import deque

import pyatspi
from gi.repository import Atspi, GLib

from .errors import DesktopError


class Accessibility:
    def __init__(self):
        Atspi.set_timeout(500, 1500)
        self.desktop = pyatspi.Registry.getDesktop(0)
        self.handles = {}
        self.identities = {}
        self.events = deque(maxlen=128)
        self.changed = threading.Event()
        self.sequence = 0
        self.loop = GLib.MainLoop()
        self.listener = Atspi.EventListener.new(self._event, None)
        for name in ("object:state-changed", "object:property-change", "object:children-changed",
                     "object:text-changed", "window"):
            self.listener.register(name)
        self.thread = threading.Thread(target=self.loop.run, daemon=True, name="atspi-events")
        self.thread.start()

    def _event(self, event, *unused):
        self.sequence += 1
        self.events.append({"sequence": self.sequence, "type": event.type})
        self.changed.set()

    def frame(self, window):
        candidates = []
        for app in self.desktop:
            try:
                if window["pid"] and app.get_process_id() != window["pid"]:
                    continue
                for frame in app:
                    if frame.name == window["title"]:
                        candidates.append(frame)
            except Exception:
                continue
        if len(candidates) != 1:
            raise DesktopError("NO_ACCESSIBILITY", "This window has no unique accessible frame; use its screenshot and physical input")
        return candidates[0]

    def snapshot(self, window, limit=250):
        root = self.frame(window)
        queue = deque([(root, None, 0)])
        result = []
        seen = set()
        while queue and len(result) < limit:
            node, parent, depth = queue.popleft()
            try:
                identity = hash(node)
                if identity in seen or depth > 24:
                    continue
                seen.add(identity)
                state = node.getState()
                if state.contains(pyatspi.STATE_DEFUNCT):
                    continue
                if depth and not state.contains(pyatspi.STATE_SHOWING):
                    continue
                role, name = node.getRoleName(), node.name or ""
                key = (window["id"], identity, name, role)
                handle = self.identities.get(key)
                if handle not in self.handles:
                    handle = "e_" + uuid.uuid4().hex[:16]
                    self.identities[key] = handle
                interfaces = node.get_interfaces()
                item = {"id": handle, "parent": parent, "role": role, "name": name[:1000],
                        "enabled": state.contains(pyatspi.STATE_ENABLED),
                        "showing": state.contains(pyatspi.STATE_SHOWING),
                        "focused": state.contains(pyatspi.STATE_FOCUSED)}
                if "Action" in interfaces:
                    action = node.queryAction()
                    item["actions"] = [action.getName(i) for i in range(action.nActions)]
                if "Component" in interfaces:
                    rect = node.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
                    item["bounds"] = dict(x=rect.x, y=rect.y, width=rect.width, height=rect.height)
                if role in ("check box", "radio button", "toggle button"):
                    item["checked"] = state.contains(pyatspi.STATE_CHECKED)
                if "Text" in interfaces and role != "password text":
                    item["text"] = node.queryText().getText(0, min(node.queryText().characterCount, 2000))
                if "Value" in interfaces:
                    value = node.queryValue()
                    item["value"] = value.currentValue
                self.handles[handle] = dict(node=node, root=root, window=window["id"],
                                            name=name, role=role, created=time.monotonic())
                result.append(item)
                for child in node:
                    if child is not None:
                        queue.append((child, handle, depth+1))
            except Exception:
                continue
        # Keep bounded lifetime and memory even across long-running agent sessions.
        cutoff = time.monotonic() - 600
        self.handles = {k: v for k, v in self.handles.items() if v["created"] > cutoff}
        if len(self.handles) > 4000:
            self.handles = dict(list(self.handles.items())[-3000:])
        self.identities = {k: v for k, v in self.identities.items() if v in self.handles}
        return {"elements": result, "truncated": bool(queue), "event_sequence": self.sequence}

    def resolve(self, handle, wid):
        record = self.handles.get(handle)
        if record is None or record["window"] != wid:
            raise DesktopError("STALE_ELEMENT", "Element handle is absent or belongs to another window; observe again")
        node = record["node"]
        try:
            if (node.getState().contains(pyatspi.STATE_DEFUNCT) or node.name != record["name"]
                    or node.getRoleName() != record["role"]):
                raise ValueError("Element identity changed")
            parent = node
            for _ in range(30):
                if parent == record["root"]:
                    return node
                # GTK4 can return -1 for a live intermediate container's index.
                # Verify ancestry itself instead of treating that hint as authority.
                if parent is None:
                    break
                parent = parent.parent
        except Exception:
            pass
        raise DesktopError("STALE_ELEMENT", "Element is detached or changed identity; observe again")

    def action(self, handle, wid, action_name=None):
        node = self.resolve(handle, wid)
        state = node.getState()
        if not state.contains(pyatspi.STATE_ENABLED) or not state.contains(pyatspi.STATE_SHOWING):
            raise DesktopError("NOT_ACTIONABLE", "Control is not enabled and showing")
        try:
            action = node.queryAction()
            names = [action.getName(i) for i in range(action.nActions)]
        except Exception as error:
            raise DesktopError("NO_ACTION", "Control exposes no semantic action; use its visible bounds") from error
        if action_name is None:
            matching = [n for n in ("click", "press", "activate", "toggle") if n in names]
            if not matching:
                raise DesktopError("NO_ACTION", f"Choose an exposed action: {names}")
            action_name = matching[0]
        if action_name not in names:
            raise DesktopError("NO_ACTION", f"Available actions: {names}")
        if not action.doAction(names.index(action_name)):
            raise DesktopError("ACTION_REJECTED", "Application rejected the accessibility action")

    def set_value(self, handle, wid, value):
        node = self.resolve(handle, wid)
        if not node.getState().contains(pyatspi.STATE_ENABLED):
            raise DesktopError("NOT_ACTIONABLE", "Control is disabled")
        interfaces = node.get_interfaces()
        if "EditableText" in interfaces:
            accepted = node.queryEditableText().setTextContents(str(value))
        elif "Value" in interfaces:
            accepted = node.queryValue().setCurrentValue(float(value))
        else:
            raise DesktopError("NOT_EDITABLE", "Control has no editable-text or numeric-value interface")
        if not accepted:
            raise DesktopError("ACTION_REJECTED", "Application rejected the value")

    def select_text(self, handle, wid, text):
        node = self.resolve(handle, wid)
        editable = node.queryText()
        value = editable.getText(0, -1)
        if value.count(text) != 1 or not text:
            raise DesktopError("AMBIGUOUS_TEXT", "Text must occur exactly once in the control")
        start = value.index(text)
        while editable.getNSelections():
            editable.removeSelection(0)
        if not editable.addSelection(start, start + len(text)):
            raise DesktopError("ACTION_REJECTED", "Application rejected text selection")

    def stop(self):
        for name in ("object:state-changed", "object:property-change", "object:children-changed",
                     "object:text-changed", "window"):
            self.listener.deregister(name)
        self.loop.quit()
