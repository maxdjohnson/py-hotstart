import os
import runpy  # noqa

# prelude


def __py_hotstart_loop__():
    def log(msg):
        with open("/tmp/py-hotstart.interpreter.log", "+a") as f:
            f.write(f"{os.getpid()} {msg}\n")

    try:
        log("starting supervision")
        with open(3) as ctrl:
            # While under supervision, evaluate expressions from control fd in a loop.
            state = {"__supervised__": True}
            while state["__supervised__"]:
                # 'line' is a Python string literal like: "try:\n    foo()\n..."
                # Convert it from a literal representation to the actual string, then run it.
                line = ctrl.readline().strip()
                log(f"running line {line}")
                exec(eval(line), globals(), state)

            # Supervision done. Read the rest of ctrl for instructions.
            log("finished supervision")
            line = ctrl.read().strip()
        # parse from string with eval, and return it.
        return eval(line)
    except Exception:
        import traceback

        log(f"error {traceback.format_exc()}")
        os._exit(1)


exec(__py_hotstart_loop__(), {k: v for k, v in globals().items() if k != "__py_hotstart_loop__"})
