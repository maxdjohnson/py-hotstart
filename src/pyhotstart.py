import os  # noqa
import runpy  # noqa
import termios  # noqa

# prelude


def __py_hotstart_loop__():
    with open(3) as ctrl:
        # While under supervision, evaluate expressions from control fd in a loop.
        __py_hotstart_supervised__ = True
        while __py_hotstart_supervised__:
            # 'line' is a Python string literal like: "try:\n    foo()\n..."
            # Convert it from a literal representation to the actual string, then run it.
            exec(eval(ctrl.readline().strip()))
        # Supervision done. Read control fd until eof, parse from string with eval, and return it.
        res = eval(ctrl.read().strip())
    return res


exec(__py_hotstart_loop__())
