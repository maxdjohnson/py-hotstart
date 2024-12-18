import os
import runpy  # noqa
import socket

# prelude


def __py_hotstart_loop__():
    with socket.socket(family=socket.AF_UNIX, fileno=3) as sock, sock.makefile() as ctrl:
        try:
            # While under supervision, evaluate expressions from control fd in a loop.
            supervised = True
            while supervised:
                line = ctrl.readline().strip()
                exec(eval(line))

            # Supervision done. Read the rest of ctrl for instructions.
            line = ctrl.read().strip()

            # Parse instructions from string with eval, and return it.
            return eval(line)
        except Exception:
            import traceback

            ctrl.write(repr(traceback.format_exc()) + "\n")
            ctrl.close()
            os._exit(1)


# Control socket fd is originally inheritable to survive `exec python`. Set it to False now that
# python is running to avoid leaking it to child processes.
os.set_inheritable(3, False)
exec(__py_hotstart_loop__(), {k: v for k, v in globals().items() if k != "__py_hotstart_loop__"})
