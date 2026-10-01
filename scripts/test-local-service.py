"""Exercise a real detached service without a model call, on either local transport."""

import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time


def captured_run(command, env, cwd):
    # communicate() can wait forever after killing a launcher when its detached child
    # retains a pipe handle. Wait for the process separately and bound the EOF checks.
    process = subprocess.Popen(command, env=env, cwd=cwd,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    output = [[], []]

    def read(pipe, chunks):
        with pipe:
            while chunk := pipe.read1(8192):
                chunks.append(chunk)

    readers = [threading.Thread(target=read, args=(pipe, chunks), daemon=True)
               for pipe, chunks in zip((process.stdout, process.stderr), output)]
    for reader in readers:
        reader.start()
    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
        raise
    for reader in readers:
        reader.join(timeout=2)
    assert all(not reader.is_alive() for reader in readers), (
        f"captured output stayed open after {command[1:]} exited with {process.returncode}; "
        "a descendant retained the launcher's pipe handles"
    )
    stdout, stderr = (b''.join(chunks).decode('utf-8', errors='replace') for chunks in output)
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def main():
    binary = Path("target/debug/nanus.exe" if os.name == "nt" else "target/debug/nanus")
    binary = binary.resolve()
    # Unix socket paths are short; macOS's default temporary directory is often too long.
    temp_root = None if os.name == "nt" else "/tmp"
    with tempfile.TemporaryDirectory(prefix="nanus-service-", dir=temp_root) as home:
        env = dict(os.environ, NANUS_HOME=home, NANUS_CONFIG=str(Path(home) / "config.toml"),
                   RUST_LOG="nanus=debug,nanus_link=debug")
        Path(env["NANUS_CONFIG"]).write_text("", encoding="utf-8")

        def run(*args):
            print(f"checking {' '.join(args)}", flush=True)
            return captured_run([str(binary), *args], env, home)

        initial = run("service", "status")
        assert initial.returncode != 0, "an absent service must report failure"
        try:
            started = run("service", "start")
            assert started.returncode == 0, started.stderr
            status = run("service", "status")
            assert status.returncode == 0, status.stderr
            assert "link version:" in status.stdout, status.stdout
            assert "sessions: none held" in status.stdout, status.stdout
            second = run("service", "start")
            assert second.returncode != 0, "a second owner must be refused"
            assert "already listening" in second.stderr, second.stderr
            stopped = run("service", "stop")
            assert stopped.returncode == 0, stopped.stderr
            deadline = time.monotonic() + 10
            while run("service", "status").returncode == 0:
                assert time.monotonic() < deadline, "the service did not stop"
                time.sleep(0.05)
            # The same endpoint is immediately reusable, without a stale pipe/socket owner.
            restarted = run("service", "start")
            assert restarted.returncode == 0, restarted.stderr
        finally:
            try:
                run("service", "stop")
                deadline = time.monotonic() + 10
                while run("service", "status").returncode == 0:
                    assert time.monotonic() < deadline, "cleanup did not stop the service"
                    time.sleep(0.05)
            finally:
                log = Path(home) / "nanus-service.log"
                if log.exists():
                    print(log.read_text(encoding="utf-8", errors="replace"), flush=True)


if __name__ == "__main__":
    main()
