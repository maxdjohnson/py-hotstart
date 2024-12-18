import os
import runpy  # noqa

# prelude


def __py_hotstart_loop__():
    try:
        with open(3) as ctrl:
            # While under supervision, evaluate expressions from control fd in a loop.
            state = {"__supervised__": True}
            while state["__supervised__"]:
                # 'line' is a Python string literal like: "try:\n    foo()\n..."
                # Convert it from a literal representation to the actual string, then run it.
                line = ctrl.readline().strip()
                exec(eval(line), globals(), state)

            # Supervision done. Read the rest of ctrl for instructions.
            line = ctrl.read().strip()

        # Parse instructions from string with eval, and return it.
        return eval(line)
    except Exception:
        import time
        import traceback

        with open("/tmp/py-hotstart.interpreter.log", "+a") as f:
            f.write(f"{time.time()} {os.getpid()} {traceback.format_exc()}\n")
        os._exit(1)


exec(__py_hotstart_loop__(), {k: v for k, v in globals().items() if k != "__py_hotstart_loop__"})
