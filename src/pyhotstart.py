import os
import runpy  # noqa
import socket

# Control socket fd is originally inheritable to survive `exec python`. Set it to False now that
# python is running to avoid leaking it to child processes.
os.set_inheritable(3, False)


# prelude


def __py_hotstart_loop__():
    with socket.socket(family=socket.AF_UNIX, fileno=3) as sock, sock.makefile() as ctrl:
        # While under supervision, evaluate expressions from control fd in a loop.
        ctx = {"supervised": True, "ctrl": ctrl}
        while ctx["supervised"]:
            line = ctrl.readline().strip()
            exec(eval(line), globals(), ctx)

        # Supervision done. Read the rest of ctrl for instructions, then shut down socket.
        line = ctrl.read().strip()

        # Parse instructions from string with eval, and return it.
        return eval(line)


exec(__py_hotstart_loop__(), {k: v for k, v in globals().items() if k != "__py_hotstart_loop__"})
