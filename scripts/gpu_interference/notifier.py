#!/usr/bin/env python3
"""Stand-in for a Steam notification: a small X11 window with STEAM_OVERLAY=1,
redrawn at a fixed rate so its content changes like Steam's toast animation."""
import sys, time
from Xlib import X, display, Xatom

w, h = int(sys.argv[1]) if len(sys.argv) > 1 else 420, int(sys.argv[2]) if len(sys.argv) > 2 else 120
rate = float(sys.argv[3]) if len(sys.argv) > 3 else 60.0
d = display.Display()
screen = d.screen()
root = screen.root
geo = root.get_geometry()
x, y = geo.width - w - 24, geo.height - h - 24
win = root.create_window(x, y, w, h, 0, screen.root_depth, X.InputOutput, X.CopyFromParent,
                         background_pixel=screen.black_pixel, override_redirect=0)
win.change_property(d.intern_atom("STEAM_OVERLAY"), Xatom.CARDINAL, 32, [1])
win.set_wm_name("pyroshine-notification-standin")
gc = win.create_gc(foreground=screen.white_pixel)
win.map()
d.sync()
colors = [0x3366cc, 0xcc6633, 0x33cc66, 0xcccc33]
t0 = time.monotonic()
frame = 0
while True:
    gc.change(foreground=colors[(frame // 30) % len(colors)])
    win.fill_rectangle(gc, 0, 0, w, h)
    gc.change(foreground=screen.white_pixel)
    bx = int((frame * 4) % max(1, w - 40))
    win.fill_rectangle(gc, bx, h // 3, 40, h // 3)
    d.flush()
    frame += 1
    next_t = t0 + frame / rate
    time.sleep(max(0.0, next_t - time.monotonic()))
