#!/usr/bin/env python3
"""Drive nano against mock.py and print the final screen.

One command replaces the manual bootstrap: a throwaway config, an in-process
mock provider, a tmux pty of a known size, the keystrokes, and a virtual
terminal to read back. Nothing is shared between runs.

A positional argument is a turn: it is typed and Enter is pressed. One that
starts with `!` is a list of tmux key names sent as-is, no Enter:

  harness.py                              # one turn "text:ok"
  harness.py "text:hi" "tool:read:src/ui.rs"
  harness.py "!C-u" "!a b c"              # clear the line, type abc
  harness.py --size 40x12 "blank:hi"
  harness.py --keep "text:ok"             # keep the temp workdir

The reply is chosen by the turn's text; see mock.py for text:, split:, blank:,
slow:, tool:NAME:ARG, reason:TEXT|ANSWER and empty:.
"""

import argparse
import importlib.util
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
DEFAULT_BIN = HERE / "target/debug/nano"

# Importing mock.py must not leave a __pycache__ in the repo.
sys.dont_write_bytecode = True


def load_mock():
    spec = importlib.util.spec_from_file_location("nano_mock", HERE / "mock.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    # The mock calls its model "mock/mock-1"; the provider is "mock", so make
    # the id a single segment or every spec becomes mock/mock/mock-1.
    module.MODEL = "mock-1"
    module.CHUNKS["model"] = module.MODEL
    return module


class Tmux:
    def __init__(self, socket, conf, session="nano"):
        self.base = ["tmux", "-L", socket, "-f", str(conf)]
        self.session = session

    def run(self, *args, check=False):
        return subprocess.run(
            self.base + list(args), text=True, capture_output=True, check=check
        )

    def start(self, size, cwd, command):
        # Options come from the config file: a server with no sessions exits,
        # and the session may start (and die) before set-option could run.
        self.run(
            "new-session",
            "-d",
            "-s",
            self.session,
            "-x",
            str(size[0]),
            "-y",
            str(size[1]),
            "-c",
            str(cwd),
            command,
            check=True,
        )

    def send_literal(self, text):
        self.run("send-keys", "-t", self.session, "-l", "--", text, check=True)
        self.run("send-keys", "-t", self.session, "Enter", check=True)

    def send_keys(self, keys):
        self.run("send-keys", "-t", self.session, *keys, check=True)

    def capture(self, scroll, ansi):
        args = ["capture-pane", "-p"]
        if ansi:
            args.append("-e")
        args += ["-t", self.session, "-S", f"-{scroll}"]
        return self.run(*args).stdout

    def dead(self):
        out = self.run("display-message", "-p", "-t", self.session, "#{pane_dead}")
        return out.stdout.strip() == "1"

    def exit_code(self):
        out = self.run(
            "display-message", "-p", "-t", self.session, "#{pane_dead_status}"
        )
        return out.stdout.strip()

    def kill(self):
        self.run("kill-server")


def parse_size(raw):
    try:
        cols, rows = raw.lower().split("x")
        return int(cols), int(rows)
    except ValueError:
        raise SystemExit(f"harness: bad --size {raw!r}, want COLSxROWS")


def write_config(root, port):
    cfg = Path(root) / "config" / "nano"
    cfg.mkdir(parents=True)
    (cfg / "config.json").write_text(
        json.dumps(
            {
                "providers": {"mock": {"base_url": f"http://127.0.0.1:{port}/v1"}},
                "model": "mock/mock-1",
            },
            indent=2,
        )
        + "\n"
    )
    return cfg.parent


def settle(tmux, wait, timeout, scroll, ansi):
    """Block until the screen has not changed for `wait` seconds."""
    deadline = time.monotonic() + timeout
    last, since = None, time.monotonic()
    while time.monotonic() < deadline:
        if tmux.dead():
            return
        screen = tmux.capture(scroll, ansi)
        if screen == last:
            if time.monotonic() - since >= wait:
                return
        else:
            last, since = screen, time.monotonic()
        time.sleep(0.05)


def main():
    ap = argparse.ArgumentParser(
        description="Run nano against mock.py in a pty and print the screen.",
        epilog="An argument starting with ! is sent as tmux keys, not as a turn.",
    )
    ap.add_argument("turns", nargs="*", metavar="ARG")
    ap.add_argument("--bin", default=str(DEFAULT_BIN), help="nano binary")
    ap.add_argument("--size", default="80x24", metavar="COLSxROWS")
    ap.add_argument("--scroll", type=int, default=200, metavar="N",
                    help="capture N lines of history above the screen")
    ap.add_argument("--wait", type=float, default=0.4, metavar="SEC",
                    help="quiet time that means idle")
    ap.add_argument("--timeout", type=float, default=30.0, metavar="SEC",
                    help="max wait for an idle screen")
    ap.add_argument("--ansi", action="store_true", help="keep colors")
    ap.add_argument("--keep", action="store_true",
                    help="keep the temp workdir")
    args = ap.parse_args()
    if not args.turns:
        args.turns = ["text:ok"]

    binary = Path(args.bin).resolve()
    if not binary.exists():
        print("harness: building nano...", file=sys.stderr)
        subprocess.run(["cargo", "build"], cwd=HERE, check=True)
    size = parse_size(args.size)

    work = tempfile.mkdtemp(prefix="nano-harness-")
    conf = Path(work) / "tmux.conf"
    conf.write_text(
        "set-option -g status off\n"
        "set-option -g remain-on-exit on\n"
        "set-option -g remain-on-exit-format ''\n"
        "set-option -g history-limit 10000\n"
    )
    mock = load_mock()
    server = ThreadingHTTPServer(("127.0.0.1", 0), mock.Handler)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()

    socket = f"nano-harness-{os.getpid()}"
    tmux = Tmux(socket, conf)
    ok = False
    try:
        config_home = write_config(work, port)
        command = "env XDG_CONFIG_HOME={} {}".format(
            shlex.quote(str(config_home)), shlex.quote(str(binary))
        )
        tmux.start(size, work, command)

        # Wait for the model line, or for the process to die with its error.
        start = time.monotonic()
        while time.monotonic() - start < 5:
            screen = tmux.capture(args.scroll, args.ansi)
            if "model:" in screen or tmux.dead():
                break
            time.sleep(0.05)

        for arg in args.turns:
            if arg.startswith("!"):
                tmux.send_keys(arg[1:].split())
            else:
                tmux.send_literal(arg)
            settle(tmux, args.wait, args.timeout, args.scroll, args.ansi)

        screen = tmux.capture(args.scroll, args.ansi)
        if not args.ansi:
            lines = [line.rstrip() for line in screen.splitlines()]
            while lines and not lines[-1]:
                lines.pop()
            screen = "\n".join(lines) + "\n"
        sys.stdout.write(screen)
        sys.stdout.flush()
        ok = not tmux.dead() or tmux.exit_code() in ("", "0")
    finally:
        server.shutdown()
        server.server_close()
        tmux.kill()
        if args.keep:
            print(f"harness: kept {work}", file=sys.stderr)
        else:
            shutil.rmtree(work, ignore_errors=True)

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
