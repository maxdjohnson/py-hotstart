import array
import fcntl
import os
import resource
import signal
import socket
import termios
import traceback

SERVER_ADDRESS = "/tmp/pyforked-server.sock"
LOG_PATH = os.path.expanduser("~/Library/Logs/pyforked-server.log")
PIDFILE = "/tmp/pyforked-server.pid"
MAXFD = 2048


def daemonize():
    # Daemonize
    pid = os.fork()
    if pid > 0:
        os._exit(0)
    os.setsid()
    pid = os.fork()
    if pid > 0:
        os._exit(0)
    os.chdir("/")
    os.umask(0)
    maxfd = resource.getrlimit(resource.RLIMIT_NOFILE)[1]
    maxfd = MAXFD if maxfd == resource.RLIM_INFINITY else maxfd
    for fd in range(3, maxfd):
        try:
            os.close(fd)
        except OSError:
            pass
    log_fd = os.open(LOG_PATH, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    os.dup2(log_fd, 1)
    os.dup2(log_fd, 2)
    null_fd = os.open(os.devnull, os.O_RDONLY)
    os.dup2(null_fd, 0)
    os.close(null_fd)
    if log_fd > 2:
        os.close(log_fd)


def write_pidfile(pid):
    with open(PIDFILE, "w") as f:
        f.write(str(pid))


def remove_pidfile():
    if os.path.exists(PIDFILE):
        os.remove(PIDFILE)


def recv_fds(conn, max_fds=1):
    buf = bytearray(1024)
    fds = array.array("i", [-1] * max_fds)
    try:
        msg, ancdata, flags, addr = conn.recvmsg_into([buf], 1024, socket.CMSG_SPACE(max_fds * 4))
    except OSError as e:
        print(f"Error receiving message: {e}")
        return b"", []
    if msg == 0:
        # Premature disconnect or no data
        print("Received 0 bytes, client disconnected prematurely.")
        return b"", []
    for cmsg_level, cmsg_type, cmsg_data in ancdata:
        if cmsg_level == socket.SOL_SOCKET and cmsg_type == socket.SCM_RIGHTS:
            fds.frombytes(cmsg_data)
    data = buf[:msg]
    out_fds = [fd for fd in fds if fd != -1]
    return data, out_fds


def run_child(slave_fd, code_snippet):
    # In the child:
    try:
        os.setsid()
        fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
        os.dup2(slave_fd, 0)
        os.dup2(slave_fd, 1)
        os.dup2(slave_fd, 2)
        if slave_fd > 2:
            os.close(slave_fd)

        # If no code snippet provided, just exit or do something default:
        if not code_snippet.strip():
            # If desired, run an empty snippet just exits immediately.
            # code.interact(local={}) # or revert to an interactive shell if needed
            return

        # Execute the provided code snippet
        # It's safer to exec in a controlled namespace.
        local_ns = {}
        try:
            exec(code_snippet, {}, local_ns)
        except BaseException:
            traceback.print_exc()

    except BaseException:
        traceback.print_exc()
    finally:
        os._exit(0)


def handle_client(conn):
    msg, fds = recv_fds(conn)
    if not msg.startswith(b"RUN"):
        if msg:
            print(f"Invalid command: {msg}")
        return

    # Extract code snippet:
    # if msg = b"RUN print('hello')", then code_snippet = "print('hello')"
    # If msg = b"RUN", then code_snippet = "" (no code)
    parts = msg.split(b" ", 1)
    if len(parts) == 1:
        code_snippet = ""
    else:
        code_snippet = parts[1].decode("utf-8", errors="replace")

    if not fds:
        print("No fds received or invalid fd.")
        return

    slave_fd = fds[0]
    if slave_fd < 0:
        print("Invalid slave fd received.")
        return

    pid = os.fork()
    if pid == 0:
        run_child(slave_fd, code_snippet)
    else:
        # parent
        os.close(slave_fd)

    try:
        conn.sendall(b"OK")
    except OSError as e:
        print(f"Error sending OK to client: {e}")


def shutdown(server, server_pid):
    if os.getpid() != server_pid:
        # This is not the original parent process; do not remove pidfile.
        return
    print("Received shutdown signal, terminating forkserver.")
    server.close()
    if os.path.exists(SERVER_ADDRESS):
        os.unlink(SERVER_ADDRESS)
    remove_pidfile()
    os._exit(0)


def run_forkserver():
    server_pid = os.getpid()

    if os.path.exists(SERVER_ADDRESS):
        os.unlink(SERVER_ADDRESS)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(SERVER_ADDRESS)
    server.listen(5)

    # Handle signals for clean shutdown
    def handle_signal(signum, frame):
        shutdown(server, server_pid)

    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    write_pidfile(server_pid)

    while True:
        try:
            conn, _ = server.accept()
        except OSError as e:
            print(f"Error on accept: {e}")
            continue

        with conn:
            try:
                handle_client(conn)
            except Exception:
                traceback.print_exc()


def main():
    try:
        daemonize()
        run_forkserver()
    except Exception:
        with open(LOG_PATH, "a") as f:
            traceback.print_exc(file=f)
        os._exit(1)
    os._exit(0)


if __name__ == "__main__":
    main()
