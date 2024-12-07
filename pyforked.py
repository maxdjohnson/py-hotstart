import os
import sys
import socket
import termios
import fcntl
import pickle
import code
import traceback
import time

def cli_main():
    # Obtain current terminal
    cli_tty_name = os.ttyname(sys.stdin.fileno())
    # Get current termios attributes
    attrs = termios.tcgetattr(sys.stdin.fileno())

    # Ensure fork-server is running
    forkserver_address = os.path.join(os.environ['TMPDIR'], 'pyforked-server.sock')
    ensure_forkserver_running(forkserver_address)

    # Connect to fork-server and send request
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(forkserver_address)
        data = (cli_tty_name, attrs)
        s.sendall(pickle.dumps(data))

def ensure_forkserver_running(address):
    # Very simplistic check. If server socket doesn't exist, start it.
    if not os.path.exists(address):
        # Fork to start the server
        pid = os.fork()
        if pid == 0:
            # In child: run fork-server as a daemon
            forkserver_main(address)
            os._exit(0)
        else:
            # Parent waits a bit for server to start
            time.sleep(1.0)


def forkserver_main(address):
    """
    Daemonize the process and then run the fork-server loop.
    Output and errors go to ./pyforked-server.log.
    """

    # First fork
    pid = os.fork()
    if pid > 0:
        # Parent exits
        os._exit(0)

    # Become session leader, detach from tty
    os.setsid()

    # Second fork
    pid = os.fork()
    if pid > 0:
        # Parent of second fork exits
        os._exit(0)

    # Now we are fully detached from controlling terminal
    # Change working directory
    # os.chdir('/')
    # Clear file mode creation mask
    os.umask(0)

    # Close all file descriptors except those needed
    # We'll just assume a minimal environment - close all except stdin/out/err
    # Typically you'd do a loop from 3 to maxfd and close them.
    maxfd = 1024
    fds_closed = []
    for fd in range(3, maxfd):
        try:
            os.close(fd)
            fds_closed.append(fd)
        except OSError:
            pass

    # Open log file for writing
    log_path = "./pyforked-server.log"
    log_fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)

    # Redirect stdout and stderr to log file
    os.dup2(log_fd, 1)  # stdout
    os.dup2(log_fd, 2)  # stderr
    # stdin from /dev/null
    null_fd = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null_fd, 0)
    os.close(null_fd)
    if log_fd > 2:
        os.close(log_fd)

    # Now run the fork-server logic in a try/except block
    try:
        _run_forkserver_loop(address)
    except Exception:
        # Log the exception traceback to stderr (which is now our log file)
        traceback.print_exc()
        os._exit(1)
    os._exit(0)


def _run_forkserver_loop(address):
    # If already exists, remove
    if os.path.exists(address):
        os.unlink(address)

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(address)
    server.listen(5)

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
                os.setsid()

                fd = os.open(tty_name, os.O_RDWR | os.O_NOCTTY)
                fcntl.ioctl(fd, termios.TIOCSCTTY, 0)
                os.dup2(fd, 0)
                os.dup2(fd, 1)
                os.dup2(fd, 2)
                os.close(fd)

                termios.tcsetattr(0, termios.TCSANOW, attrs)

                code.interact(local=globals())
                os._exit(0)
            else:
                # Parent - doesn't wait; continues to next request
                pass


if __name__ == '__main__':
    # Run: python3 this_script.py cli
    # This will start the daemonized forkserver and spawn a REPL child on request.
    if len(sys.argv) > 1 and sys.argv[1] == 'cli':
        cli_main()
    else:
        print("Run: python3 this_script.py cli")
