"""Exercise a real detached service without a model call, on either local transport."""

import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    binary = Path("target/debug/nanus.exe" if os.name == "nt" else "target/debug/nanus")
    binary = binary.resolve()
    # Unix socket paths are short; macOS's default temporary directory is often too long.
    temp_root = None if os.name == "nt" else "/tmp"
    with tempfile.TemporaryDirectory(prefix="nanus-service-", dir=temp_root) as home:
        env = dict(os.environ, NANUS_HOME=home, NANUS_CONFIG=str(Path(home) / "config.toml"))
        Path(env["NANUS_CONFIG"]).write_text("", encoding="utf-8")

        def run(*args):
            return subprocess.run(
                [str(binary), *args], env=env, cwd=home, capture_output=True,
                text=True, timeout=30, check=False,
            )

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
            run("service", "stop")
            deadline = time.monotonic() + 10
            while run("service", "status").returncode == 0:
                assert time.monotonic() < deadline, "cleanup did not stop the service"
                time.sleep(0.05)


if __name__ == "__main__":
    main()
