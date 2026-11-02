import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("measure_embedded_capacity", ROOT / "tools" / "measure-embedded-capacity.py")
MEASURE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MEASURE)


class EmbeddedMeasurementTests(unittest.TestCase):
    def sample(self, system):
        contract = MEASURE.rss_metric_contract(system)
        sample = {
            "rss_metric_version": 2,
            "rss_unit": "bytes",
            "rss_peak_source": contract["peak_source"],
            "rss_current_source": contract["current_source"],
            "getrusage_maxrss_source": contract["getrusage_source"],
        }
        for stage in ["startup", "final"]:
            sample[f"{stage}_peak_rss_bytes"] = 4096
            sample[f"{stage}_current_rss_bytes"] = 2048 if system == "Linux" else None
            sample[f"{stage}_getrusage_maxrss_bytes"] = 8192
        return sample, contract

    def test_metric_sources_distinguish_current_image_peaks_from_rusage(self):
        for system in ["Linux", "Darwin"]:
            sample, contract = self.sample(system)
            MEASURE.validate_rss_metrics(sample, contract)
        with self.assertRaisesRegex(RuntimeError, "unsupported"):
            MEASURE.rss_metric_contract("unverified")

    def test_missing_or_incompatible_metric_identity_fails(self):
        original, contract = self.sample("Linux")
        for field in ["rss_metric_version", "rss_unit", "rss_peak_source", "rss_current_source", "getrusage_maxrss_source"]:
            for action in ["missing", "wrong"]:
                sample = dict(original)
                if action == "missing":
                    del sample[field]
                else:
                    sample[field] = "unlabelled old metric"
                with self.assertRaisesRegex(RuntimeError, field):
                    MEASURE.validate_rss_metrics(sample, contract)

    def test_missing_invalid_or_overflowed_counters_fail(self):
        original, contract = self.sample("Linux")
        for stage in ["startup", "final"]:
            for suffix in ["peak_rss_bytes", "current_rss_bytes", "getrusage_maxrss_bytes"]:
                field = f"{stage}_{suffix}"
                sample = dict(original)
                del sample[field]
                with self.assertRaisesRegex(RuntimeError, field):
                    MEASURE.validate_rss_metrics(sample, contract)
                for invalid in [None, -1, 1 << 64, True, "4096", 1.5]:
                    sample[field] = invalid
                    with self.assertRaisesRegex(RuntimeError, field):
                        MEASURE.validate_rss_metrics(sample, contract)
        sample, contract = self.sample("Darwin")
        sample["startup_current_rss_bytes"] = 1
        with self.assertRaisesRegex(RuntimeError, "must be null"):
            MEASURE.validate_rss_metrics(sample, contract)

    def test_file_hashing_is_streamed_with_identical_sha256(self):
        payload = b"bounded digest input" * 100_000
        expected = hashlib.sha256(payload).hexdigest()
        reads = []

        class BoundedReader(io.BytesIO):
            def read(self, count=-1):
                if count <= 0 or count > MEASURE.HASH_CHUNK_BYTES:
                    raise AssertionError("unbounded digest read")
                reads.append(count)
                return super().read(count)

        class Input:
            def open(self, mode):
                if mode != "rb":
                    raise AssertionError(mode)
                return BoundedReader(payload)

        self.assertEqual(MEASURE.sha256_file(Input()), expected)
        self.assertGreater(len(reads), 1)

    @unittest.skipUnless(sys.platform == "linux", "Linux current-mm and exec accounting")
    def test_linux_preexec_peak_is_separate_from_current_image_peak(self):
        # exec_mmap retains the old mm high water in signal.maxrss; task_mem's
        # VmHWM uses the new mm. Keep this diagnostic separate from engine runs.
        usage_code = """def usage():
    result = {}
    for line in Path('/proc/self/status').read_text().splitlines():
        if line.startswith(('VmHWM:', 'VmRSS:')):
            name, value = line.split(':', 1)
            result[name] = int(value.split()[0]) * 1024
    result['getrusage'] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
    return result
"""
        imports = "import gc,json,os,resource,subprocess,sys\nfrom pathlib import Path\n"
        child = imports + usage_code + "print(json.dumps(usage()))\n"
        program = imports + usage_code + f"child = {child!r}\n" + """
payload = bytearray(32 * 1024 * 1024)
for index in range(0, len(payload), 4096):
    payload[index] = 1
before_free = usage()
del payload
gc.collect()
parent = usage()
default = json.loads(subprocess.run([sys.executable, '-c', child], cwd='/', check=True, capture_output=True, text=True).stdout)
read_fd, write_fd = os.pipe()
pid = os.fork()
if pid == 0:
    os.dup2(write_fd, 1)
    os.close(read_fd)
    os.close(write_fd)
    os.execl(sys.executable, sys.executable, '-c', child)
    os._exit(99)
os.close(write_fd)
with os.fdopen(read_fd) as source:
    forked = json.load(source)
_, status = os.waitpid(pid, 0)
if status != 0:
    raise RuntimeError('fork/exec diagnostic failed')
print(json.dumps({'allocated_bytes': 33554432, 'before_free': before_free, 'parent': parent, 'default_child': default, 'fork_exec_child': forked}))
"""
        result = json.loads(subprocess.run([sys.executable, "-c", program], check=True, text=True, capture_output=True).stdout)
        print("Linux RSS counter diagnostic: " + json.dumps(result, sort_keys=True))
        self.assertGreater(result["before_free"]["VmRSS"] - result["parent"]["VmRSS"], 16 * 1024**2)
        # This bounds the fresh image independently of which spawn optimization
        # the Python build selects. A large getrusage value is diagnostic only.
        self.assertLess(result["default_child"]["VmHWM"], result["before_free"]["VmRSS"])
        self.assertLess(result["fork_exec_child"]["VmHWM"], result["before_free"]["VmRSS"])
        self.assertGreaterEqual(result["default_child"]["getrusage"], result["default_child"]["VmHWM"] - 1024**2)


if __name__ == "__main__":
    unittest.main()
