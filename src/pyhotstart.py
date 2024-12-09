import array
import fcntl
import os
import resource
import signal
import socket
import termios
import traceback

SERVER_ADDRESS = "/tmp/pyhotstart.sock"
LOG_PATH = os.path.expanduser("~/Library/Logs/pyhotstart.log")
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


def run_child_then_exit(cmd, code_snippet, fds):
    # In the child:
    alive_fd = None
    try:
        os.setsid()
        if cmd == "RUN_PTY":
            # We are meant to run in a pty. Set it as the controlling terminal.
            (slave_fd,) = fds
            fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
            # Use the pty as fds 0-2
            os.dup2(slave_fd, 0)
            os.dup2(slave_fd, 1)
            os.dup2(slave_fd, 2)
            # Close the original fd
            if slave_fd > 2:
                os.close(slave_fd)
            # Send SIGWINCH to update to new terminal state
            os.kill(os.getpid(), signal.SIGWINCH)
        elif cmd == "RUN":
            # We are running outside a tty. Use the provided FDs
            for i in range(3):
                os.dup2(fds[i], i)
                if fds[i] > 2:
                    os.close(fds[i])
            alive_fd = fds[3]

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
        except Exception:
            traceback.print_exc()
    finally:
        if alive_fd is not None:
            os.close(alive_fd)
        os._exit(0)


def handle_sigchld(signum, frame):
    """Handler for SIGCHLD to reap zombie processes."""
    while True:
        try:
            # -1 means wait for any child process
            # WNOHANG means return immediately if no child has exited
            pid, _ = os.waitpid(-1, os.WNOHANG)
            if pid <= 0:
                break
        except ChildProcessError:
            break


def shutdown(server, server_pid):
    if os.getpid() != server_pid:
        # This is not the original parent process; do not remove pidfile.
        return
    server.close()
    try:
        os.unlink(SERVER_ADDRESS)
    except Exception:
        traceback.print_exc()


def run_forkserver():
    server_pid = os.getpid()

    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(SERVER_ADDRESS)
    server.listen(5)
    print(f"Server listening on {SERVER_ADDRESS}")

    # Handle signals for clean shutdown
    def handle_sigterm(signum, frame):
        print("Received shutdown signal, terminating forkserver.")
        shutdown(server, server_pid)
        os._exit(0)

    default_sigterm = signal.signal(signal.SIGTERM, handle_sigterm)
    default_sigint = signal.signal(signal.SIGINT, handle_sigterm)
    default_sigchld = signal.signal(signal.SIGCHLD, handle_sigchld)

    try:
        while True:
            try:
                conn, _ = server.accept()
            except OSError as e:
                print(f"Error on accept: {e}")
                continue

            try:
                msg, fds = recv_fds(conn)

                # Handle EXIT message by breaking out of the loop
                if msg.decode("utf-8", errors="replace") == "EXIT":
                    assert not fds, "unexpected file descriptors"
                    break

                # Parse and validate command
                cmd, code_snippet = [
                    p.decode("utf-8", errors="replace") for p in msg.split(b" ", 1)
                ]
                if cmd not in {"RUN", "RUN_PTY"}:
                    if msg:
                        print(f"Invalid command: {msg}")
                    continue

                # Fork; child runs command while server acks
                print(f"Running {cmd=} {code_snippet=} {fds=}")
                pid = os.fork()
                if pid == 0:
                    # Clean up server resources and reset signal handlers
                    conn.close()
                    signal.signal(signal.SIGTERM, default_sigterm)
                    signal.signal(signal.SIGINT, default_sigint)
                    signal.signal(signal.SIGCHLD, default_sigchld)
                    server.close()
                    # Run child, then exit immeditatly without cleanup
                    run_child_then_exit(cmd, code_snippet, fds)
                else:
                    # Close the fds that got copied to child
                    for fd in fds:
                        os.close(fd)
                    try:
                        conn.sendall(f"OK {pid}".encode())
                    except OSError as e:
                        print(f"Error sending OK to client: {e}")
            except Exception:
                traceback.print_exc()
            finally:
                conn.close()
    finally:
        print("Received EXIT command, terminating forkserver.")
        shutdown(server, server_pid)


def main():
    if os.path.exists(SERVER_ADDRESS):
        raise ValueError(f"File {SERVER_ADDRESS} already exists")
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
