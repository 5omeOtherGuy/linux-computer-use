"""Synthetic GTK UI for input correctness tests; all data is disposable."""
import json
import sys
from pathlib import Path
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk

output = Path(sys.argv[1])
window = Gtk.Window(title='Agent Desktop Speed Fixture')
window.set_default_size(1000, 650)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
window.add(box)
entry = Gtk.Entry()
box.pack_start(entry, False, False, 0)
text = Gtk.TextView()
box.pack_start(text, True, True, 0)
button = Gtk.Button(label='Count click')
box.pack_start(button, False, False, 0)
state = {'clicks': 0, 'activations': 0}

def save(*args):
    buf = text.get_buffer()
    state.update(entry=entry.get_text(), text=buf.get_text(buf.get_start_iter(), buf.get_end_iter(), True))
    tmp = output.with_suffix('.tmp')
    tmp.write_text(json.dumps(state))
    tmp.replace(output)

def click(*args):
    state['clicks'] += 1
    save()

def activate(*args):
    state['activations'] += 1
    state['submitted_entry'] = entry.get_text()
    save()

entry.connect('changed', save)
entry.connect('activate', activate)
text.get_buffer().connect('changed', save)
button.connect('clicked', click)
window.connect('destroy', Gtk.main_quit)
window.show_all()
entry.grab_focus()
save()
Gtk.main()
