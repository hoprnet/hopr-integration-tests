"""Exercise runner logging and summaries without starting the integration stack."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RunnerTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        scripts = self.root / "scripts" / "integration"
        scripts.mkdir(parents=True)
        shutil.copy(ROOT / "scripts/integration/run-binchain.sh", scripts)
        lib = (ROOT / "scripts/integration/lib.sh").read_text()
        # Keep the real environment and result helpers; replace only stack operations.
        lib += r'''
chain_start() {
  if [ -n "${MOCK_SIGNAL:-}" ]; then kill -"${MOCK_SIGNAL}" "$$"; fi
  return "${MOCK_CHAIN_RC:-0}"
}
chain_stop() { echo stopped >>"${REPO_ROOT}/chain-stops"; }
reap_nodes() {
  if [ -f "${REPO_ROOT}/leaked-pid" ]; then
    read -r pid <"${REPO_ROOT}/leaked-pid"
    kill "${pid}"
    wait "${pid}" 2>/dev/null || true
    rm "${REPO_ROOT}/leaked-pid"
  fi
}
list_scenarios() { echo smoke; }
cargo_it() {
  if [ "${MOCK_LEAK_FD:-0}" = 1 ]; then
    sleep 30 &
    echo $! >"${REPO_ROOT}/leaked-pid"
  fi
  printf 'test smoke ... %s\n' "${MOCK_RESULT:-ok}"
  return "${MOCK_CARGO_RC:-0}"
}
'''
        (scripts / "lib.sh").write_text(lib)
        self.script = scripts / "run-binchain.sh"
        # Delay tee deliberately: parsing before the copy finishes loses the result.
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        tee = bin_dir / "tee"
        tee.write_text(f'#!/bin/sh\nsleep 0.1\nexec {shutil.which("tee")} "$@"\n')
        tee.chmod(0o755)
        self.env = {"PATH": f'{bin_dir}:{os.environ["PATH"]}', "HOPR_LOG_DIR": "logs"}

    def run_runner(self, **env):
        return subprocess.run(
            ["bash", str(self.script)], cwd=self.root, env={**self.env, **env},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10,
        )

    def run_dir(self):
        return next((self.root / "logs").glob("run-*"))

    def test_success_keeps_log_and_summary(self):
        result = self.run_runner(TEST_TARGETS="integration pix")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("2 passed, 0 failed", result.stdout)
        for target in ("integration", "pix"):
            self.assertIn("test smoke ... ok", (self.run_dir() / f"client-{target}.log").read_text())
        self.assertIn("status=passed", (self.run_dir() / "run.status").read_text())

    def test_inherited_log_writer_is_reaped_before_waiting(self):
        result = self.run_runner(MOCK_LEAK_FD="1")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 passed, 0 failed", result.stdout)
        self.assertFalse((self.root / "leaked-pid").exists())

    def test_failure_keeps_log_and_summary(self):
        result = self.run_runner(MOCK_CARGO_RC="1", MOCK_RESULT="FAILED")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("0 passed, 1 failed", result.stdout)
        self.assertIn("status=failed", (self.run_dir() / "run.status").read_text())
        self.assertIn("FAILED", (self.run_dir() / "client-integration.log").read_text())

    def test_invalid_scenario_writes_status(self):
        result = self.run_runner(SCENARIOS_EXCEPT="typo")
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("status=failed", (self.run_dir() / "run.status").read_text())

    def test_chain_start_failure_writes_status(self):
        result = self.run_runner(MOCK_CHAIN_RC="7")
        self.assertEqual(result.returncode, 7, result.stdout)
        self.assertIn("exit_code=7", (self.run_dir() / "run.status").read_text())

    def test_signals_write_interrupted_status(self):
        for signal, code in (("INT", 130), ("TERM", 143)):
            with self.subTest(signal=signal):
                result = self.run_runner(MOCK_SIGNAL=signal, HOPR_LOG_DIR=f"logs-{signal}")
                self.assertEqual(result.returncode, code, result.stdout)
                run_dir = next((self.root / f"logs-{signal}").glob("run-*"))
                self.assertIn("status=interrupted", (run_dir / "run.status").read_text())
                self.assertIn(f"exit_code={code}", (run_dir / "run.status").read_text())

    def test_without_retention_still_reports_results(self):
        result = self.run_runner(HOPR_LOG_DIR="")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 passed, 0 failed", result.stdout)
        self.assertFalse((self.root / "logs").exists())

    def test_shared_chain_is_not_stopped(self):
        chain = self.root / "shared-chain"
        chain.mkdir()
        (chain / "anvil.log").write_text("existing chain log\n")
        result = self.run_runner(CHAIN_SHARED="1", CHAIN_DATA_DIR=str(chain))
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertFalse((self.root / "chain-stops").exists())
        self.assertIn(str(chain / "anvil.log"), (self.run_dir() / "chain-slice.txt").read_text())


if __name__ == "__main__":
    unittest.main()
