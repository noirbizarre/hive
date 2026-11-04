"""Run the opt-in terminal lifecycle test with a real controlling PTY (Unix)."""

import errno
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time


def settings(fd):
    values = termios.tcgetattr(fd)
    # BSD may mark input for reprocessing after tcsetattr; PENDIN is not an application mode.
    values[3] &= ~getattr(termios, "PENDIN", 0)
    return values


def main():
    pid, master = pty.fork()
    if pid == 0:
        test = "tests::a_real_terminal_can_be_handed_to_a_child_and_restored_after_failure"
        # CI passes its already instrumented executable; manual runs may build through Cargo.
        command = ([sys.argv[1], test, "--ignored", "--exact", "--nocapture"] if len(sys.argv) > 1
                   else ["cargo", "test", "-p", "hive-tui", test, "--", "--ignored", "--exact", "--nocapture"])
        os.execvp(command[0], command)

    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    original = settings(master)
    output = bytearray()
    pasted = False
    resized = False
    cursor_queries = 0
    deadline = time.monotonic() + 600
    try:
        while time.monotonic() < deadline:
            if not select.select([master], [], [], 0.1)[0]:
                continue
            try:
                chunk = os.read(master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            output.extend(chunk)
            # Crossterm queries the cursor while resetting its screen after the editor exits.
            while output.count(b"\x1b[6n") > cursor_queries:
                os.write(master, b"\x1b[3;3R")
                cursor_queries += 1
            if not resized and b"HIVE_PTY_IMAGE_RESIZE_READY" in output:
                fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 17, 50, 0, 0))
                os.write(master, b"r")
                resized = True
            if not pasted and b"HIVE_PTY_PASTE_READY" in output:
                os.write(master, b"\x1b[200~one\ntwo\x1b[201~")
                pasted = True
        else:
            os.killpg(pid, signal.SIGKILL)
            raise RuntimeError("terminal test timed out")
        _, status = os.waitpid(pid, 0)
        assert os.waitstatus_to_exitcode(status) == 0, "terminal lifecycle test failed"
        assert pasted, "test never requested bracketed paste"
        assert resized, "test never requested image reflow"
        assert settings(master) == original, "terminal modes were not restored"
        for sequence in [b"\x1b[?1049h", b"\x1b[?1049l", b"\x1b[?2004h", b"\x1b[?2004l", b"\x1b[?25h"]:
            assert sequence in output, f"missing terminal transition {sequence!r}"
        # Images are uploaded afresh after editor handoff and deleted before each
        # alternate-screen exit. Assertions inspect raw APC, not a text emulator.
        screens = output.split(b"\x1b[?1049h")[1:]
        assert len(screens) == 2, "expected TUI entry and editor return"
        image_ids = []
        for index, screen in enumerate(screens):
            owned = screen.split(b"\x1b[?1049l", 1)[0]
            uploads = re.findall(rb"\x1b_Ga=t,t=d,f=100,i=(\d+),q=2,m=0;", owned)
            assert len(uploads) == (2 if index == 0 else 1), "redraw must reuse uploads; resize must invalidate them"
            image_ids.extend(uploads)
            assert owned.count(b"\x1b_Ga=p,") == (6 if index == 0 else 2), "missing inline or thumbnail placement"
            if index == 0:
                assert b"c=60,r=8,C=1,q=2" in owned, "incorrect initial inline bounds"
                assert b"c=20,r=6,C=1,q=2" in owned, "incorrect initial thumbnail bounds"
            assert b"c=50,r=8,C=1,q=2" in owned, "incorrect resized inline bounds"
            assert b"c=20,r=5,C=1,q=2" in owned, "incorrect resized thumbnail bounds"
            for image_id in uploads:
                cleanup = b"\x1b_Ga=d,d=I,i=" + image_id + b",q=2\x1b\\"
                assert cleanup in owned, "owned image was not freed before screen exit"
            if index == 0:
                first_cleanup = b"\x1b_Ga=d,d=I,i=" + uploads[0] + b",q=2\x1b\\"
                second_upload = b"\x1b_Ga=t,t=d,f=100,i=" + uploads[1] + b","
                assert owned.index(first_cleanup) < owned.index(second_upload), "resize left a stale image"
        assert len(set(image_ids)) == 3, "resize and handoff must invalidate image residency"
    except BaseException:
        sys.stderr.write(output.decode(errors="replace"))
        raise
    finally:
        os.close(master)
    print("PTY smoke test passed: images, cursor, paste, editor handoff, and terminal restoration")


if __name__ == "__main__":
    main()
