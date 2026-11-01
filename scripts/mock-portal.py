#!/usr/bin/env python3
"""A stand-in for xdg-desktop-portal's RemoteDesktop and ScreenCast interfaces.

Answers every request at once (as if the person at the machine clicked
Share), hands out a real PipeWire remote with a real video node as "the
screen", and logs every call with its arguments as one JSON line each, so a
test can check what Tain sent. Signatures are declared, so a call with the
wrong argument types is refused here as it would be by the real portal.

What tain-core's `the_host_path_against_a_stand_in_portal` test runs against:
it tests the whole Linux host path — the portal conversation, the PipeWire
stream, input sent back, the restore token — on a machine with no desktop.
docs/linux.md has the commands. Needs python3-dbus and python3-gi.

    mock-portal.py <pipewire-node-id> <log-file> <pipewire-socket>
"""
import json
import socket
import sys

import dbus
import dbus.mainloop.glib
import dbus.service
from gi.repository import GLib

NODE, LOG, PW_SOCKET = int(sys.argv[1]), sys.argv[2], sys.argv[3]
PATH = "/org/freedesktop/portal/desktop"
RD = "org.freedesktop.portal.RemoteDesktop"
SC = "org.freedesktop.portal.ScreenCast"

dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
starts = 0


def plain(value):
    if isinstance(value, (dbus.Dictionary, dict)):
        return {str(k): plain(v) for k, v in value.items()}
    if isinstance(value, (dbus.Array, dbus.Struct, list, tuple)):
        return [plain(v) for v in value]
    if isinstance(value, (dbus.Boolean,)):
        return bool(value)
    if isinstance(value, (dbus.Int32, dbus.UInt32, dbus.Int64, dbus.UInt64, dbus.Int16, dbus.UInt16, dbus.Byte)):
        return int(value)
    if isinstance(value, dbus.Double):
        return float(value)
    return str(value)


def log(*entry):
    with open(LOG, "a") as f:
        f.write(json.dumps([plain(e) for e in entry]) + "\n")


def sender_part(sender):
    return sender[1:].replace(".", "_")


class Request(dbus.service.Object):
    @dbus.service.signal("org.freedesktop.portal.Request", signature="ua{sv}")
    def Response(self, code, results):
        pass

    @dbus.service.method("org.freedesktop.portal.Request", in_signature="", out_signature="")
    def Close(self):
        log("Request.Close")


class Session(dbus.service.Object):
    @dbus.service.method("org.freedesktop.portal.Session", in_signature="", out_signature="")
    def Close(self):
        log("Session.Close")
        self.remove_from_connection()


def respond(sender, options, results):
    path = "%s/request/%s/%s" % (PATH, sender_part(sender), options["handle_token"])
    request = Request(bus, path)

    def answer():
        request.Response(dbus.UInt32(0), dbus.Dictionary(results, signature="sv"))
        request.remove_from_connection()
        return False

    GLib.timeout_add(30, answer)
    return dbus.ObjectPath(path)


PROPERTIES = {
    RD: {"version": dbus.UInt32(2), "AvailableDeviceTypes": dbus.UInt32(7)},
    SC: {"version": dbus.UInt32(5), "AvailableSourceTypes": dbus.UInt32(3), "AvailableCursorModes": dbus.UInt32(7)},
}


class Portal(dbus.service.Object):
    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="ss", out_signature="v")
    def Get(self, interface, name):
        return PROPERTIES[interface][name]

    @dbus.service.method(dbus.PROPERTIES_IFACE, in_signature="s", out_signature="a{sv}")
    def GetAll(self, interface):
        return dbus.Dictionary(PROPERTIES.get(interface, {}), signature="sv")

    def create(self, kind, options, sender):
        log(kind + ".CreateSession", options)
        handle = "%s/session/%s/%s" % (PATH, sender_part(sender), options["session_handle_token"])
        Session(bus, handle)
        return respond(sender, options, {"session_handle": dbus.String(handle)})

    def start(self, kind, session, options, sender, devices):
        global starts
        starts += 1
        log(kind + ".Start", session)
        size = dbus.Struct((dbus.Int32(1280), dbus.Int32(720)), signature="ii")
        stream = dbus.Struct((dbus.UInt32(NODE), dbus.Dictionary({"size": size}, signature="sv")), signature="ua{sv}")
        results = {
            "streams": dbus.Array([stream], signature="(ua{sv})"),
            "restore_token": dbus.String("token-%d" % starts),
        }
        if devices:
            results["devices"] = dbus.UInt32(devices)
        return respond(sender, options, results)

    # ── RemoteDesktop ──
    @dbus.service.method(RD, in_signature="a{sv}", out_signature="o", sender_keyword="sender")
    def CreateSession(self, options, sender=None):
        return self.create("RemoteDesktop", options, sender)

    @dbus.service.method(RD, in_signature="oa{sv}", out_signature="o", sender_keyword="sender")
    def SelectDevices(self, session, options, sender=None):
        log("RemoteDesktop.SelectDevices", session, options)
        return respond(sender, options, {})

    @dbus.service.method(RD, in_signature="osa{sv}", out_signature="o", sender_keyword="sender")
    def Start(self, session, parent, options, sender=None):
        return self.start("RemoteDesktop", session, options, sender, 7)

    @dbus.service.method(RD, in_signature="oa{sv}udd", out_signature="")
    def NotifyPointerMotionAbsolute(self, session, options, stream, x, y):
        log("NotifyPointerMotionAbsolute", stream, x, y)

    @dbus.service.method(RD, in_signature="oa{sv}dd", out_signature="")
    def NotifyPointerMotion(self, session, options, dx, dy):
        log("NotifyPointerMotion", dx, dy)

    @dbus.service.method(RD, in_signature="oa{sv}iu", out_signature="")
    def NotifyPointerButton(self, session, options, button, state):
        log("NotifyPointerButton", button, state)

    @dbus.service.method(RD, in_signature="oa{sv}ui", out_signature="")
    def NotifyPointerAxisDiscrete(self, session, options, axis, steps):
        log("NotifyPointerAxisDiscrete", axis, steps)

    @dbus.service.method(RD, in_signature="oa{sv}iu", out_signature="")
    def NotifyKeyboardKeycode(self, session, options, keycode, state):
        log("NotifyKeyboardKeycode", keycode, state)

    @dbus.service.method(RD, in_signature="oa{sv}uudd", out_signature="")
    def NotifyTouchDown(self, session, options, stream, slot, x, y):
        log("NotifyTouchDown", stream, slot, x, y)

    @dbus.service.method(RD, in_signature="oa{sv}uudd", out_signature="")
    def NotifyTouchMotion(self, session, options, stream, slot, x, y):
        log("NotifyTouchMotion", stream, slot, x, y)

    @dbus.service.method(RD, in_signature="oa{sv}u", out_signature="")
    def NotifyTouchUp(self, session, options, slot):
        log("NotifyTouchUp", slot)

    # ── ScreenCast ──
    @dbus.service.method(SC, in_signature="oa{sv}", out_signature="o", sender_keyword="sender")
    def SelectSources(self, session, options, sender=None):
        log("ScreenCast.SelectSources", session, options)
        return respond(sender, options, {})

    @dbus.service.method(SC, in_signature="oa{sv}", out_signature="h")
    def OpenPipeWireRemote(self, session, options):
        log("ScreenCast.OpenPipeWireRemote", session)
        remote = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        remote.connect(PW_SOCKET)
        return dbus.types.UnixFd(remote)


name = dbus.service.BusName("org.freedesktop.portal.Desktop", bus)
Portal(bus, PATH)
log("ready")
GLib.MainLoop().run()
