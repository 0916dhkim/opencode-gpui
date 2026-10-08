#!/usr/bin/python3
"""Private D-Bus FileChooser portal for headless GPUI integration tests.

Each OpenFile call consumes one entry of a JSON array selection plan. A null
entry means cancel; an array contains absolute file paths. An object can set
"paths" and "delay_ms" to keep a picker pending across a session switch.
The portal only returns file URIs; the client must read/upload the bytes.
"""

import json
import sys
from pathlib import Path

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib

DESKTOP = "org.freedesktop.portal.Desktop"
REQUEST = "org.freedesktop.portal.Request"
FILE_CHOOSER = "org.freedesktop.portal.FileChooser"


class Request(dbus.service.Object):
    def __init__(self, bus, path, result, delay_ms=250):
        super().__init__(bus, path)
        self.result = result
        self.closed = False
        GLib.timeout_add(delay_ms, self.respond)

    def respond(self):
        if self.closed:
            return False
        if self.result is None:
            self.Response(1, {})
        else:
            self.Response(0, {"uris": dbus.Array([Path(path).as_uri() for path in self.result], signature="s")})
        self.remove_from_connection()
        return False

    @dbus.service.signal(REQUEST, signature="ua{sv}")
    def Response(self, code, results):
        pass

    @dbus.service.method(REQUEST)
    def Close(self):
        self.closed = True
        self.remove_from_connection()


class Portal(dbus.service.Object):
    def __init__(self, bus, selections, log_path):
        super().__init__(bus, "/org/freedesktop/portal/desktop")
        self.bus = bus
        self.selections = selections
        self.log_path = log_path
        self.calls = 0
        self.requests = []

    @dbus.service.method(FILE_CHOOSER, in_signature="ssa{sv}", out_signature="o", sender_keyword="sender")
    def OpenFile(self, parent, title, options, sender=None):
        index = self.calls
        self.calls += 1
        if index >= len(self.selections):
            raise dbus.exceptions.DBusException("No planned selection", name="org.freedesktop.portal.Error.Failed")
        token = str(options.get("handle_token", f"test_{index}"))
        caller = sender.lstrip(":").replace(".", "_")
        path = f"/org/freedesktop/portal/desktop/request/{caller}/{token}"
        choice = self.selections[index]
        delay_ms = 250
        if isinstance(choice, dict):
            delay_ms = choice.get("delay_ms", delay_ms)
            choice = choice.get("paths")
        with open(self.log_path, "a", encoding="utf-8") as stream:
            stream.write(json.dumps({"call": index + 1, "multiple": bool(options.get("multiple")), "paths": choice}) + "\n")
        self.requests.append(Request(self.bus, path, choice, delay_ms))
        return dbus.ObjectPath(path)

    @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
    def Get(self, interface, property_name):
        if interface == FILE_CHOOSER and property_name == "version":
            return dbus.UInt32(4)
        raise dbus.exceptions.DBusException("Unknown property", name="org.freedesktop.DBus.Error.UnknownProperty")


if __name__ == "__main__":
    plan, log, ready = sys.argv[1:]
    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus()
    name = dbus.service.BusName(DESKTOP, bus)
    portal = Portal(bus, json.loads(Path(plan).read_text(encoding="utf-8")), log)
    Path(ready).write_text("ready", encoding="utf-8")
    GLib.MainLoop().run()
