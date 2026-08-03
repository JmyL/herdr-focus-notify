#!/usr/bin/env python3
"""Mimic notify-send --print-id -A default=Focus --wait TITLE BODY.

Only the special "default" action is registered (body click / swaync -ad).
A separate labeled "focus" button is intentionally omitted — same behavior,
and it made it harder to tell whether default-action wiring works.
"""
import sys

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

title = sys.argv[1] if len(sys.argv) > 1 else "Herdr"
body = sys.argv[2] if len(sys.argv) > 2 else ""

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
loop = GLib.MainLoop()
state = {"id": None, "action": None}


def on_signal(_conn, _sender, _path, _iface, member, params):
    args = params.unpack()
    if not args or args[0] != state["id"]:
        return
    if member == "ActionInvoked" and len(args) > 1:
        state["action"] = args[1]
        loop.quit()
    elif member == "NotificationClosed":
        loop.quit()


bus.signal_subscribe(
    "org.freedesktop.Notifications",
    "org.freedesktop.Notifications",
    None,
    "/org/freedesktop/Notifications",
    None,
    Gio.DBusSignalFlags.NONE,
    on_signal,
)

reply = bus.call_sync(
    "org.freedesktop.Notifications",
    "/org/freedesktop/Notifications",
    "org.freedesktop.Notifications",
    "Notify",
    GLib.Variant(
        "(susssasa{sv}i)",
        (
            "herdr-focus-notify",
            0,
            "",
            title,
            body,
            ["default", "Focus"],
            {},
            # Milliseconds. 0 falls back to swaync's timeout (20s here), which
            # disappears before agent-done notices are easy to read/click.
            40000,
        ),
    ),
    GLib.VariantType("(u)"),
    Gio.DBusCallFlags.NONE,
    -1,
    None,
)
state["id"] = reply.unpack()[0]
print(state["id"], flush=True)

loop.run()
if state["action"]:
    print(state["action"], flush=True)
