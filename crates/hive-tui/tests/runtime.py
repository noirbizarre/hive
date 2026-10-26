"""Drive the instrumented TUI test binary through its real terminal and ACP event loop."""

import errno
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
from terminal_screen import Screen


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
            # The child injects a failing clipboard backend rather than touching the native clipboard.
            env["XDG_RUNTIME_DIR"] = str(directory)
            if create:
                env["HIVE_RUNTIME_CREATE"] = "1"
            os.execve(sys.argv[1], [sys.argv[1], "terminal_tests::runtime_fixture", "--ignored", "--exact", "--nocapture"], env)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        original = settings(master)
        output = bytearray()
        screen = Screen()
        address = None
        cursor_queries = 0
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

        def pump():
            nonlocal address, cursor_queries
            if not select.select([master], [], [], .02)[0]:
                return
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            output.extend(chunk)
            screen.feed(chunk)
            while output.count(b"\x1b[6n") > cursor_queries:
                os.write(master, b"\x1b[3;3R")
                cursor_queries += 1
            match = re.search(rb"HIVE_RUNTIME_READY (http://[^\s]+)", output)
            if match:
                address = match[1].decode()

        def wait(predicate, timeout=20):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                pump()
                if predicate():
                    return
            raise AssertionError("TUI fixture did not reach the expected state")

        def state():
            with opener.open(f"{address}/fixture", timeout=5) as response:
                return json.load(response)

        def action(name):
            with opener.open(urllib.request.Request(f"{address}/fixture/{name}", data=b""), timeout=5) as response:
                return json.load(response)

        def send(value):
            os.write(master, value)

        try:
            wait(lambda: address is not None)
            if create:
                wait(lambda: screen.contains("fixture"))
                send(b"\x0e\r")
                wait(lambda: state()["creations"] == 1 and b"\x1b[?25h" in output)
            else:
                wait(lambda: state()["resumes"] == 1 and b"\x1b[?25h" in output)
            send(b"az\x1b[Db\x1b[C\x1b[200~\npasted\x1b[201~")
            # Editor read-back verifies the actual draft, rather than terminal diff fragments.
            send(b"\x18\x05")
            wait(ready.exists)
            assert ready.read_text() == "abz\npasted", ready.read_text()
            assert not state()["prompts"]
            action("update")
            action("permission")
            assert state()["permissions"] == 0
            release.touch()
            wait(lambda: screen.contains("PTY approval"))
            send(b"1\r")
            wait(lambda: state()["permissions"] == 1)
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
            send(b"\x01\x1bc\x1bx\x1bv")
            wait(lambda: screen.contains("Clipboard"))
            send(b"\r")
            wait(lambda: len(state()["prompts"]) == 1)
            assert state()["prompts"][0] == [{"type": "text", "text": "from editor\nsecond"}]

            send(b"\x10\x1b[B\r")
            wait(lambda: state()["configurations"] == 1)
            wait(lambda: screen.contains("Session configuration updated"))
            action("stall")
            send(b"\t")
            wait(lambda: screen.contains("Sessions"))
            send(b"\t")
            wait(lambda: screen.contains("Message"))
            wait(lambda: state()["stalled"])
            ready.unlink()
            release.unlink()
            send(b"responsive\x18\x05")
            # Less than the HTTP timeout: progress must happen before a stalled refresh can fail.
            wait(ready.exists, timeout=4)
            assert ready.read_text() == "responsive", "catalog fetch blocked prompt editing"
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
            wait(lambda: screen.contains("Message"))
            send(b"\x1b[200~survives handoff\x1b[201~")
            # Global session/control keys must remain usable without consuming the editable draft.
            for keys, count in [(b"\x1b[104;5u", 1), (b"\x14", 1), (b"\x12", 1), (b"\x19", 2)]:
                resumes = state()["resumes"]
                send(keys)
                wait(lambda: state()["resumes"] >= resumes + count)
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
        except BaseException:
            sys.stderr.write(output.decode(errors="replace"))
            raise
        finally:
            if pid is not None:
                os.killpg(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            os.close(master)


if __name__ == "__main__":
    main()
