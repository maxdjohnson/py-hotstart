import os  # noqa
import runpy  # noqa
import termios  # noqa

# prelude


def __py_hotstart_loop__():
    try:
        print("starting supervision")
        # While under supervision, evaluate expressions from control fd in a loop.
        state = {**globals(), "__supervised__": True}
        while state["__supervised__"]:
            # 'line' is a Python string literal like: "try:\n    foo()\n..."
            # Convert it from a literal representation to the actual string, then run it.
            print(f"{state['__supervised__']=}")
            line = '"__supervised__ = False"'
            print(f"running line {line}")
            exec(eval(line), state)
            print(f"{state['__supervised__']=}")

        # Supervision done. Read the rest of ctrl for instructions.
        print("finished supervision")
    except Exception:
        import traceback

        print(f"error {traceback.format_exc()}\n")
        os._exit(1)


__py_hotstart_loop__()
