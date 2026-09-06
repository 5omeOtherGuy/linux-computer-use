"""Independent GTK application used for observable desktop acceptance tests."""
import json
import os
from pathlib import Path

import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gdk, GLib, Gtk

out = Path(os.environ["LCU_FIXTURE_DIR"])
state = {"saved": None, "checked": False, "drop": False, "scroll": 0, "selected_file": None}


def record():
    temporary = out / "state.tmp"
    temporary.write_text(json.dumps(state))
    temporary.replace(out / "state.json")


window = Gtk.Window(title="Linux Computer Use Acceptance")
window.set_default_size(760, 640)
window.move(80, 80)
window.set_border_width(20)
window.connect("destroy", Gtk.main_quit)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
window.add(box)
box.pack_start(Gtk.Label(label="Linux desktop integration acceptance"), False, False, 0)
field = Gtk.Entry()
field.get_accessible().set_name("Message")
box.pack_start(field, False, False, 0)
toggle = Gtk.CheckButton(label="Enable option")
toggle.connect("toggled", lambda button: (state.update(checked=button.get_active()), record()))
box.pack_start(toggle, False, False, 0)
status = Gtk.Label(label="Ready")


def save(button):
    state["saved"] = field.get_text()
    record()
    status.set_text("Saved successfully")


save_button = Gtk.Button(label="Save message")
save_button.connect("clicked", save)
box.pack_start(save_button, False, False, 0)
row = Gtk.Box(spacing=12)
box.pack_start(row, False, False, 0)


def choose(button):
    dialog = Gtk.FileChooserDialog(title="LCU File Chooser", transient_for=window,
                                   action=Gtk.FileChooserAction.OPEN)
    dialog.add_buttons("Cancel", Gtk.ResponseType.CANCEL, "Open", Gtk.ResponseType.OK)
    dialog.set_default_response(Gtk.ResponseType.OK)
    dialog.set_current_folder(str(out))
    dialog.show_all()

    def response(d, value):
        if value == Gtk.ResponseType.OK:
            state["selected_file"] = d.get_filename()
            record()
            status.set_text("File selected")
        d.destroy()
    dialog.connect("response", response)


button = Gtk.Button(label="Open file dialog")
button.connect("clicked", choose)
row.pack_start(button, True, True, 0)
menu_button = Gtk.MenuButton(label="Options")
menu = Gtk.Menu()
item = Gtk.MenuItem(label="Apply preset")
item.connect("activate", lambda item: field.set_text("Preset applied"))
menu.append(item)
menu.show_all()
menu_button.set_popup(menu)
row.pack_start(menu_button, True, True, 0)


def replace(button):
    global field
    old = field
    field = Gtk.Entry()
    field.get_accessible().set_name("Message")
    box.pack_start(field, False, False, 0)
    box.reorder_child(field, 1)
    old.destroy()
    field.show()


button = Gtk.Button(label="Replace field")
button.connect("clicked", replace)
row.pack_start(button, True, True, 0)
disabled = Gtk.Button(label="Disabled action")
disabled.set_sensitive(False)
box.pack_start(disabled, False, False, 0)
duplicate_row = Gtk.Box(spacing=10)
for _ in range(2):
    duplicate_row.pack_start(Gtk.Button(label="Duplicate"), True, True, 0)
box.pack_start(duplicate_row, False, False, 0)

canvas = Gtk.DrawingArea()
canvas.set_size_request(700, 180)
canvas.get_accessible().set_name("Drag canvas")
canvas.add_events(Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.BUTTON_RELEASE_MASK |
                  Gdk.EventMask.POINTER_MOTION_MASK | Gdk.EventMask.SCROLL_MASK)
token = [60, 90]
held = False


def draw(widget, ctx):
    ctx.set_source_rgb(.08, .14, .2)
    ctx.paint()
    ctx.set_source_rgb(.4, .8, .65)
    ctx.set_line_width(3)
    ctx.rectangle(550, 30, 120, 120)
    ctx.stroke()
    ctx.set_source_rgb(.4, .8, .65) if state["drop"] else ctx.set_source_rgb(.95, .75, .3)
    ctx.arc(*token, 22, 0, 6.2832)
    ctx.fill()


def press(widget, event):
    global held
    held = abs(event.x-token[0]) < 24 and abs(event.y-token[1]) < 24


def motion(widget, event):
    if held:
        token[:] = [event.x, event.y]
        canvas.queue_draw()


def release(widget, event):
    global held
    if held:
        state["drop"] = 550 < event.x < 670 and 30 < event.y < 150
        record()
    held = False
    canvas.queue_draw()


def scroll(widget, event):
    state["scroll"] += 1
    record()
    return True


canvas.connect("draw", draw)
canvas.connect("button-press-event", press)
canvas.connect("motion-notify-event", motion)
canvas.connect("button-release-event", release)
canvas.connect("scroll-event", scroll)
box.pack_start(canvas, True, True, 0)
box.pack_start(status, False, False, 0)
window.show_all()
record()
print("READY", flush=True)
Gtk.main()
