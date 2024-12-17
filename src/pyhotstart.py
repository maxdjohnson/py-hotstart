import os  # noqa
import runpy  # noqa
import sys
import termios  # noqa

if sys.path[0] == "":
    del sys.path[0]

# prelude

# While under supervision, evaluate expressions in a loop.
__py_hotstart_supervised__ = True
while __py_hotstart_supervised__:
    # 'line' is a Python string literal like: "try:\n    foo()\n..."
    # Convert it from a literal representation to the actual string, then run it.
    exec(eval(sys.stdin.readline().strip()))

# After supervision, run a single command and exit.
del __py_hotstart_supervised__
exec(eval(sys.stdin.readline().strip()))
