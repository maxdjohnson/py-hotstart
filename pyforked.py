import os
import sys
import socket
import termios
import fcntl
import struct
import tty
import pty
import threading
import code
import pickle
import signal

# -------------------------------------------------------
# CLI code snippet
# -------------------------------------------------------
def cli_main():
    # Obtain current terminal
    cli_tty_name = os.ttyname(sys.stdin.fileno())
    # Get current termios attributes
    attrs = termios.tcgetattr(sys.stdin.fileno())

    # Ensure fork-server is running (for simplicity, we just run it once)
    # In practice, you'd check if it's running and only spawn if needed.
    forkserver_address = os.path.join(os.environ['TMPDIR'], 'pyforked-server.sock')
    ensure_forkserver_running(forkserver_address)

    # Connect to fork-server and send request
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(forkserver_address)
        # We send (tty_name, attrs)
        data = (cli_tty_name, attrs)
        s.sendall(pickle.dumps(data))

    # The fork-server will fork a child that attaches to cli_tty_name and runs a REPL.
    # The user will now interact with the newly spawned REPL.

def ensure_forkserver_running(address):
    # Very simplistic check. In practice, you'd do something more robust.
    if not os.path.exists(address):
        pid = os.fork()
        if pid == 0:
            # Child: run fork-server main
            forkserver_main(address)
            os._exit(0)
        else:
            # Parent: wait a bit for server to start up
            import time
            time.sleep(0.5)

# -------------------------------------------------------
# Fork-Server code snippet
# -------------------------------------------------------
def forkserver_main(address):
    # If already exists, remove
    if os.path.exists(address):
        os.unlink(address)

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(address)
    server.listen(1)

    # Preload heavy modules here if desired
    # e.g. import numpy, tensorflow, etc.

    # Main loop: accept requests
    while True:
        conn, _ = server.accept()
        with conn:
            data = conn.recv(10**6)
            if not data:
                continue
            tty_name, attrs = pickle.loads(data)
            pid = os.fork()
            if pid == 0:
                # Child process
                # setsid to become session leader
                os.setsid()

                # Open the requested tty
                fd = os.open(tty_name, os.O_RDWR)
                # Set as controlling terminal
                fcntl.ioctl(fd, termios.TIOCSCTTY, 0)

                # Dup over stdin, stdout, stderr
                os.dup2(fd, 0)
                os.dup2(fd, 1)
                os.dup2(fd, 2)
                os.close(fd)

                # Restore termios attributes from the CLI
                termios.tcsetattr(0, termios.TCSANOW, attrs)

                # Start a Python REPL
                # Local namespace can include preloaded modules if needed
                code.interact(local=globals())
                os._exit(0)
            else:
                # Parent (fork-server)
                # We don't wait here, just accept next connection
                pass

# -------------------------------------------------------
# Example usage
# -------------------------------------------------------
if __name__ == '__main__':
    # This is a simple demonstration:
    # Run "python3 this_script.py cli" in one terminal
    # It will connect to the fork-server (launched in the background)
    # and spawn a REPL attached to your current terminal.
    if len(sys.argv) > 1 and sys.argv[1] == 'cli':
        cli_main()
    else:
        # If run with no arguments, just print a help message
        print("Run: python3 this_script.py cli")
