"""Drive the instrumented TUI test binary through its real terminal and ACP event loop."""

import base64
import errno
import faulthandler
from concurrent.futures import ThreadPoolExecutor
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time
import urllib.request
import zlib
from terminal_screen import Screen


def interrupted(signum, _frame):
    # Unlike faulthandler's exit=True, raising runs the process-group cleanup below.
    faulthandler.dump_traceback()
    raise RuntimeError(f"terminal supervisor interrupted by signal {signum}")


def settings(fd):
    values = termios.tcgetattr(fd)
    # BSD may mark input for reprocessing after tcsetattr; PENDIN is not an application mode.
    values[3] &= ~getattr(termios, "PENDIN", 0)
    return values


def main():
    with tempfile.TemporaryDirectory(prefix="hive terminal fixture ") as directory:
        directory = Path(directory)
        editor = directory / "editor fixture.py"
        ready = directory / "ready"
        release = directory / "release"
        mode = directory / "mode"
        mode.write_text("success")
        create = sys.argv[2] == "create"
        osc52 = sys.argv[2] == "osc52"
        rich = sys.argv[2] == "rich"
        status_scenario = sys.argv[2].startswith("status")
        status_subset = sys.argv[2] == "status-subset"
        editor.write_text(
            "from pathlib import Path\nimport sys, time\n"
            "root = Path(__file__).parent\n"
            "(root / 'ready.tmp').write_text(Path(sys.argv[1]).read_text())\n"
            "(root / 'ready.tmp').replace(root / 'ready')\n"
            "deadline = time.monotonic() + 20\n"
            "while not (root / 'release').exists():\n"
            "    if time.monotonic() > deadline: raise RuntimeError('editor fixture timed out')\n"
            "    time.sleep(.01)\n"
            "if (root / 'mode').read_text() == 'failure': sys.exit(7)\n"
            "Path(sys.argv[1]).write_text('from editor\\nsecond')\n"
        )
        pid, master = pty.fork()
        if pid == 0:
            env = dict(os.environ, VISUAL=f'"{sys.executable}" "{editor}"')
            # Exercise startup detection without inheriting the developer's outer terminal hints.
            env["TERM"] = "dumb" if create else "xterm-kitty"
            for marker in ("TMUX", "STY", "ZELLIJ"):
                env.pop(marker, None)
            # The child injects a failing clipboard backend rather than touching the native clipboard.
            env["XDG_RUNTIME_DIR"] = str(directory)
            env.pop("HIVE_RUNTIME_OSC52", None)
            env.pop("HIVE_RUNTIME_RICH", None)
            env["HIVE_RUNTIME_STATUS"] = sys.argv[2]
            if rich:
                env["HIVE_RUNTIME_RICH"] = "1"
            if osc52:
                env["HIVE_RUNTIME_OSC52"] = "1"
            if create:
                env["HIVE_RUNTIME_CREATE"] = "1"
            else:
                env.pop("HIVE_RUNTIME_CREATE", None)
            os.execve(sys.argv[1], [sys.argv[1], "terminal_tests::runtime_fixture", "--ignored", "--exact", "--nocapture"], env)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        os.set_blocking(master, False)
        # A stalled kernel/HTTP call must leave a useful trace before the outer test timeout.
        previous_alarm = signal.signal(signal.SIGALRM, interrupted)
        previous_term = signal.signal(signal.SIGTERM, interrupted)
        signal.alarm(45)
        original = settings(master)
        output = bytearray()
        screen = Screen()
        address = None
        cursor_queries = 0
        mode_queries = 0
        status_queries = 0
        packet_offset = 0
        clipboard_reads = []
        clipboard_writes = []
        deny_read = False

        def png_chunk(kind, data):
            return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

        png = (b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
               + png_chunk(b"IDAT", zlib.compress(b"\x00\x01\x02\x03\xff")) + png_chunk(b"IEND", b""))

        def clipboard_packet(metadata, data=b""):
            return b"\x1b]5522;" + metadata.encode() + b";" + base64.b64encode(data) + b"\x1b\\"

        def read_reply(identifier, mime, data):
            # All-motion reporting must not invalidate an unchanged asynchronous paste target.
            os.write(master, b"\x1b[<35;80;12M")
            prefix = "type=read" + (f":id={identifier}" if identifier else "")
            packets = clipboard_packet(prefix + ":status=OK")
            for offset in range(0, len(data), 32):
                packets += clipboard_packet(prefix + ":status=DATA:mime=" + base64.b64encode(mime.encode()).decode(), data[offset:offset + 32])
            packets += clipboard_packet(prefix + ":status=DONE")
            # Deliberately split framing and Base64 across writes, including ST boundaries.
            for offset in range(0, len(packets), 7):
                os.write(master, packets[offset:offset + 7])
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        # Start threads only in the parent, after fork; macOS forbids inherited runtime locks.
        http = ThreadPoolExecutor(max_workers=1)

        def pump():
            nonlocal address, cursor_queries, mode_queries, packet_offset, status_queries
            if not select.select([master], [], [], .02)[0]:
                return
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno in (errno.EIO, errno.EAGAIN, errno.EWOULDBLOCK):
                    return
                raise
            output.extend(chunk)
            screen.feed(chunk)
            while output.count(b"\x1b]7501;?\x1b\\") > status_queries:
                if status_scenario:
                    features = b"?:states=idle,working:kinds=" if status_subset else b"?"
                    os.write(master, b"\x1b]7501;" + features + b"\x1b\\")
                status_queries += 1
            if rich:
                while output.count(b"\x1b[?5522$p") > mode_queries:
                    os.write(master, b"\x1b[?5522;2$y")
                    mode_queries += 1
                for packet in re.finditer(rb"\x1b\]5522;([^;]*);([^\x1b]*)\x1b\\", output[packet_offset:]):
                    metadata = dict(field.split("=", 1) for field in packet[1].decode().split(":"))
                    if metadata["type"] == "read":
                        mime = base64.b64decode(packet[2]).decode()
                        clipboard_reads.append((metadata, mime))
                        if deny_read:
                            os.write(master, clipboard_packet(f"type=read:id={metadata['id']}:status=EPERM"))
                        else:
                            read_reply(metadata["id"], mime, b"image/png text/plain" if mime == "." else png)
                    elif metadata["type"] == "wdata":
                        clipboard_writes.append((metadata, base64.b64decode(packet[2])))
                        if "mime" not in metadata:
                            os.write(master, clipboard_packet(f"type=write:id={metadata['id']}:status=DONE"))
                # Retain an incomplete final packet for the next read.
                complete = list(re.finditer(rb"\x1b\]5522;[^\x1b]*\x1b\\", output[packet_offset:]))
                if complete:
                    packet_offset += complete[-1].end()
            while output.count(b"\x1b[6n") > cursor_queries:
                os.write(master, b"\x1b[3;3R")
                cursor_queries += 1
            # PTY reads can split the address anywhere; only publish a complete readiness record.
            match = re.search(rb"HIVE_RUNTIME_READY (http://[^\s]+)[\r\n]", output)
            if match:
                address = match[1].decode()

        def wait(predicate, timeout=20):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                pump()
                if predicate():
                    return
            raise AssertionError("TUI fixture did not reach the expected state")

        def request(path, data=None):
            def fetch():
                with opener.open(urllib.request.Request(f"{address}{path}", data=data), timeout=5) as response:
                    return json.load(response)
            result = http.submit(fetch)
            # BSD PTY buffers can fill in one redraw; never wait for the peer without draining output.
            while not result.done():
                pump()
            return result.result()

        def state():
            return request("/fixture")

        def action(name):
            return request(f"/fixture/{name}", data=b"")

        def send(value):
            os.write(master, value)

        def statuses():
            return [dict(field.split("=", 1) for field in match[1].decode().split(":"))
                    for match in re.finditer(rb"\x1b\]7501;(state=[^\x1b]*)\x1b\\", output)]

        try:
            print(f"PTY scenario: {sys.argv[2]}", file=sys.stderr, flush=True)
            wait(lambda: address is not None)
            if create:
                wait(lambda: screen.contains("fixture"))
                send(b"\x0e\r")
                wait(lambda: state()["creations"] == 1 and b"\x1b[?25h" in output)
            else:
                wait(lambda: state()["resumes"] == 1 and b"\x1b[?25h" in output)
            if status_scenario and sys.argv[2] != "status-off":
                wait(lambda: statuses() and statuses()[-1]["state"] == "idle")
                for action_name, expected in [("running", "working"), ("blocked", "working" if status_subset else "blocked"),
                                              ("running", "working"), ("done", "idle" if status_subset else "done"),
                                              ("error", "idle" if status_subset else "error"), ("cancelled", "idle")]:
                    before = len(statuses())
                    action(action_name)
                    wait(lambda: len(statuses()) > before and statuses()[-1]["state"] == expected)
                action("running")
                wait(lambda: statuses()[-1]["state"] == "working")
                action("permission")
                wait(lambda: screen.contains("PTY approval"))
                wait(lambda: statuses()[-1]["state"] == ("working" if status_subset else "blocked"))
                assert statuses()[-1].get("kind") == (None if status_subset else "permission")
                send(b"1\r")
                wait(lambda: state()["permissions"] == 1)
                wait(lambda: statuses()[-1]["state"] == "working")
                send(b"draft\x18\x05")
                wait(ready.exists)
                wait(lambda: statuses()[-1]["state"] == "clear")
                assert ready.read_text() == "draft", "detection reply leaked into the composer"
                editor_output_start = len(output)
                action("done")
                release.touch()
                wait(lambda: screen.contains("Editor changes loaded"))
                wait(lambda: statuses()[-1]["state"] == ("idle" if status_subset else "done"))
                # Inspect the complete handoff interval after output has crossed the resume barrier.
                editor_output_end = output.index(b"\x1b[?1049h", editor_output_start)
                assert b"\x1b]7501;state=" not in output[editor_output_start:editor_output_end], "status emitted while editor owned terminal"
                before = len(statuses())
                action("update")
                wait(lambda: screen.contains("History row"))
                send(b"typed")
                # Rendering the resulting composer is an ordered barrier for processing the keys.
                wait(lambda: screen.contains("secondtyped"))
                assert len(statuses()) == before, "typing/background output acknowledged completion"
                send(b"\t")
                wait(lambda: screen.contains("Sessions"))
                send(b"\x1b[B\r")
                wait(lambda: base64.b64decode(statuses()[-1].get("title", "")) == b"hive-second")
                assert statuses()[-1]["state"] == "idle"
                send(b"\t")
                wait(lambda: screen.contains("Sessions"))
                send(b"\x1b[A\r")
                wait(lambda: base64.b64decode(statuses()[-1].get("title", "")) == b"hive-fixture")
                assert statuses()[-1]["state"] == "idle", "session switch resurrected completion"
                action("running")
                wait(lambda: statuses()[-1]["state"] == "working")
                before = len(statuses())
                resumes = state()["resumes"]
                action("gap")
                wait(lambda: state()["resumes"] > resumes)
                wait(lambda: [report["state"] for report in statuses()[before:]] == ["clear", "idle"])
                # Reattach after a gap, then exercise exit cleanup on a live report.
                send(b"\x11")
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    pump()
                    exited, code = os.waitpid(pid, os.WNOHANG)
                    if exited:
                        pid = None
                        assert os.waitstatus_to_exitcode(code) == 0
                        break
                else:
                    raise AssertionError("status fixture did not stop")
                reports = statuses()
                assert reports[-1]["state"] == "clear"
                assert len({report["id"] for report in reports}) == 1
                assert all(report.get("app") == "hive" for report in reports if report["state"] != "clear")
                assert status_queries == (0 if sys.argv[2] == "status-on" else 1)
                assert settings(master) == original
                return
            if rich:
                wait(lambda: b"\x1b[?5522h" in output)
                # Terminal-native offer authorizes exactly the follow-up representation read.
                token = base64.b64encode(b"paste-token").decode()
                send(clipboard_packet(f"type=read:status=OK:pw={token}"))
                send(clipboard_packet("type=read:status=DATA:mime=Lg==", b"text/plain image/png"))
                send(clipboard_packet("type=read:status=DONE"))
                wait(lambda: screen.contains("Image attached"))
                assert clipboard_reads[-1][0]["pw"] == token
                assert not state()["prompts"]
                send(b"describe\x18\x05")
                wait(ready.exists)
                assert ready.read_text() == "describe"
                assert b"\x1b[?5522l" in output
                release.touch()
                wait(lambda: output.count(b"\x1b[?5522h") >= 2 and screen.contains("Editor changes loaded"))
                send(b"\r")
                wait(lambda: len(state()["prompts"]) == 1)
                assert state()["prompts"][0] == [{"type": "text", "text": "from editor\nsecond"},
                                               {"type": "image", "mimeType": "image/png", "data": base64.b64encode(png).decode()}]
                # Explicit paste lists formats, then sends an image-only prompt.
                send(b"\x1bv")
                wait(lambda: len(clipboard_reads) == 3 and screen.contains("Image attached"))
                send(b"\r")
                wait(lambda: len(state()["prompts"]) == 2)
                assert len(state()["prompts"][1]) == 1 and state()["prompts"][1][0]["type"] == "image"
                action("image")
                wait(lambda: screen.contains("[image 1:"))
                rows = sorted({row for row, _ in screen.cells})
                image_row = max(row for row in rows if "[image 1:" in "".join(screen.cells.get((row, col), " ") for col in range(100)))
                send(f"\x1b[<0;5;{image_row + 1}M\x1b[<0;5;{image_row + 1}m\x1bc".encode())
                wait(lambda: clipboard_writes and "mime" not in clipboard_writes[-1][0])
                wait(lambda: screen.contains("Selection copied"))
                assert b"".join(data for _, data in clipboard_writes).startswith(b"\x89PNG")
                deny_read = True
                send(b"\x1bv")
                wait(lambda: screen.contains("permission denied"))
                assert len(state()["prompts"]) == 2
                send(b"\x11")
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    pump()
                    exited, status = os.waitpid(pid, os.WNOHANG)
                    if exited:
                        pid = None
                        assert os.waitstatus_to_exitcode(status) == 0
                        break
                else:
                    raise AssertionError("rich clipboard fixture did not stop")
                assert b"\x1b[?5522l" in output
                assert settings(master) == original
                return
            if osc52:
                selection = "copy 界 👩‍💻\nsecond"
                sequence = b"\x1b]52;c;" + base64.b64encode(selection.encode()) + b"\x1b\\"
                send(b"\x1b[200~" + selection.encode() + b"\x1b[201~\x01\x1bc")
                wait(lambda: output.count(sequence) == 1)
                send(b"\x1bx")
                wait(lambda: output.count(sequence) == 2)
                assert not state()["prompts"], "copy/cut submitted a prompt"
                # Editor read-back below also proves the successful cut left an empty composer.
            send(b"az\x1b[Db\x1b[C\x1b[200~\npasted\x1b[201~")
            # Editor read-back verifies the actual draft, rather than terminal diff fragments.
            send(b"\x18\x05")
            wait(ready.exists)
            assert ready.read_text() == "abz\npasted", ready.read_text()
            assert not state()["prompts"]
            print("PTY: first editor owns the terminal", file=sys.stderr, flush=True)
            action("update")
            action("permission")
            assert state()["permissions"] == 0
            release.touch()
            wait(lambda: screen.contains("PTY approval"))
            send(b"1\r")
            wait(lambda: state()["permissions"] == 1)
            print("PTY: queued permission answered", file=sys.stderr, flush=True)
            assert not state()["prompts"]

            ready.unlink()
            release.unlink()
            mode.write_text("failure")
            send(b"\x18\x05")
            wait(ready.exists)
            assert ready.read_text() == "from editor\nsecond"
            release.touch()
            wait(lambda: output.count(b"\x1b[?1049h") >= 3)
            # Headless clipboard failure must not cut, replace, submit, or cancel this draft.
            send(b"\x1bv" if osc52 else b"\x01\x1bc\x1bx\x1bv")
            wait(lambda: screen.contains("Clipboard"))
            send(b"\r")
            wait(lambda: len(state()["prompts"]) == 1)
            assert state()["prompts"][0] == [{"type": "text", "text": "from editor\nsecond"}]
            print("PTY: editor failure and clipboard preserved draft", file=sys.stderr, flush=True)

            send(b"\x10\x1b[B\r")
            wait(lambda: state()["configurations"] == 1)
            wait(lambda: screen.contains("Session configuration updated"))
            action("stall")
            send(b"\t")
            wait(lambda: screen.contains("Sessions"))
            capability_summary = "Terminal caps: unavailable" if create else "Terminal caps: graphics=+"
            wait(lambda: screen.contains(capability_summary))
            send(b"\t")
            wait(lambda: screen.contains("Message"))
            wait(lambda: state()["stalled"])
            ready.unlink()
            release.unlink()
            send(b"responsive\x18\x05")
            # Less than the HTTP timeout: progress must happen before a stalled refresh can fail.
            wait(ready.exists, timeout=4)
            assert ready.read_text() == "responsive", "catalog fetch blocked prompt editing"
            print("PTY: editing progressed during stalled catalog", file=sys.stderr, flush=True)
            release.touch()
            wait(lambda: output.count(b"\x1b[?1049h") >= 4)
            send(b"\x01\x1b[3~")
            action("release")
            send(b"\x1b[5~\x1b[6~\x1b[H\x1b[F")
            send(b"\x1b[<0;5;8M\x1b[<32;15;8M\x1b[<0;15;8m")
            send(b"\x1b[O\x03\t\t")
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 18, 80, 0, 0))
            os.kill(pid, signal.SIGWINCH)
            wait(lambda: state()["catalogs"] >= 5)
            resumes = state()["resumes"]
            action("gap")
            wait(lambda: state()["resumes"] > resumes)
            print("PTY: replay gap recovered", file=sys.stderr, flush=True)
            wait(lambda: screen.contains("Message"))
            send(b"\x1b[200~survives handoff\x1b[201~")
            # Global session/control keys must remain usable without consuming the editable draft.
            for keys, count in [(b"\x1b[104;5u", 1), (b"\x14", 1), (b"\x12", 1), (b"\x19", 1)]:
                resumes = state()["resumes"]
                send(keys)
                wait(lambda: state()["resumes"] >= resumes + count)
            print("PTY: control transitions preserved draft", file=sys.stderr, flush=True)
            send(b"\t")
            wait(lambda: screen.contains("Sessions"))
            resumes = state()["resumes"]
            send(b"a\x1b[B\x1b[A\r")
            wait(lambda: state()["resumes"] > resumes)
            creations = state()["creations"]
            send(b"\x0e\r")
            wait(lambda: state()["creations"] > creations)
            wait(lambda: screen.contains("Message"))
            ready.unlink()
            send(b"\x18\x05")
            wait(ready.exists)
            assert ready.read_text() == "survives handoff"
            print("PTY: session navigation preserved draft", file=sys.stderr, flush=True)
            wait(lambda: output.count(b"\x1b[?1049h") >= 5)
            send(b"\x11")
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                pump()
                exited, status = os.waitpid(pid, os.WNOHANG)
                if exited:
                    pid = None
                    assert os.waitstatus_to_exitcode(status) == 0
                    break
            else:
                raise AssertionError("TUI failed to shut down")
            assert settings(master) == original, "terminal modes were not restored"
            # Detection must not inject capability queries into the terminal's input conversation.
            for protocol in (b"\x1b_G", b"\x1b]5522;", b"\x1b]52;c;?"):
                assert protocol not in output, "capability detection emitted a protocol sequence"
            assert status_queries == (0 if create or sys.argv[2] == "status-off" else 1)
            assert not statuses(), "unsupported or disabled terminal received a status report"
            assert output.count(b"\x1b]52;") == (2 if osc52 else 0)
        except BaseException:
            sys.stderr.write(output.decode(errors="replace"))
            raise
        finally:
            signal.alarm(0)
            if pid is not None:
                for kill in (os.killpg, os.kill):
                    try:
                        kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            os.close(master)
            if pid is not None:
                os.waitpid(pid, 0)
            http.shutdown(wait=True)
            signal.signal(signal.SIGALRM, previous_alarm)
            signal.signal(signal.SIGTERM, previous_term)


if __name__ == "__main__":
    main()
