"""Deterministic GTK fixture. Launch only on a task-owned private display."""
import os
import sys
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib

if not os.environ.get('DISPLAY') or os.environ['DISPLAY'].split('.')[0] == ':0':
    raise SystemExit('A private display is required')

if '--guard' in sys.argv:
    guard = Gtk.Window(title='CUA Background Guard')
    guard.set_wmclass('linux-cua-guard', 'LinuxCuaGuard')
    guard.set_default_size(600, 450)
    guard.move(220, 130)
    guard.add(Gtk.Label(label='Foreground guard: must not receive input'))
    guard.connect('destroy', Gtk.main_quit)
    guard.show_all()
    guard.present()
    Gtk.main()
    raise SystemExit(0)

window = Gtk.Window(title='Linux CUA Benchmark')
window.set_wmclass('linux-cua-fixture', 'LinuxCuaFixture')
window.set_default_size(540, 340)
window.connect('destroy', Gtk.main_quit)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16, margin=24)
window.add(box)
box.pack_start(Gtk.Label(label='Clean-room Linux Computer Use'), False, False, 0)
entry = Gtk.Entry()
entry.get_accessible().set_name('Message')
box.pack_start(entry, False, False, 0)
button = Gtk.Button(label='Apply')
box.pack_start(button, False, False, 0)
check = Gtk.CheckButton(label='Enabled')
box.pack_start(check, False, False, 0)
status = Gtk.Label(label='Ready')
box.pack_start(status, False, False, 0)
button.connect('clicked', lambda _: status.set_text('Applied: ' + entry.get_text()))
window.show_all()
entry.grab_focus()
Gtk.main()
