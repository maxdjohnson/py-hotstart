# py-hotstart

**A python CLI with instant startup via an interpreter server.**

`py-hotstart` launches a persistent Python server that can rapidly execute arbitrary Python code, similar to invoking `python` with `-c`, `-m`, or a script file. By maintaining a warm runtime, it avoids the overhead of starting a fresh interpreter each time, enabling near-instant script execution.

## Key Features

- **Instant Startup**: Code runs quickly by running in an exiting Python interpreter.
- **Familiar Interface**: Uses flags like `-c` for code strings, `-m` for modules, and script invocation just like Python.
- **Preloading Dependencies**: The server must be initialized with the `-i` flag before use, which executes a given prelude string. Use this to import slow-loading dependencies once, keeping subsequent launches fast.
- **PTY Support**: Seamlessly handles TTY/PTY for interactive terminals or pipelines.

## Usage

1. Start the server with optional prelude code:

```bash
py-hotstart -i "import numpy"
```

This step ensures that numpy (or any other heavy dependency) is pre-loaded in the server. Rerun to restart the server.

2. Run your Python code instantly:

```bash
# Run a code snippet
py-hotstart -c "print('hello')"

# Run a module
py-hotstart -m mymodule

# Run a script file
py-hotstart myscript.py
```

Subsequent commands after the initial -i call will execute rapidly.

## Installation

1. Ensure you have Rust and Python 3 installed.
2. Build with `cargo build --release`
3. Place the binary (`target/release/py-hotstart`) in your PATH.
4. Initialize the server with `py-hotstart -i "import your_heavy_dependency"` before running commands.

## License

MIT
